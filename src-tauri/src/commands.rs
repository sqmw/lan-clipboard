use crate::net::{
    self, DiscoveredDevice, NetworkInterfaceOption, RuntimeLog, RuntimeStatus, TransferProgress,
};
use crate::settings::{
    self, Settings, SettingsError, SettingsNotice, SettingsNoticeKind, SettingsUpdate,
};
use crate::state::{AppState, DiscoveryRequest, DiscoverySingleflight};
use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};
use tokio::task;

const MANUAL_DISCOVERY_SETTLE_MS: u64 = 700;

pub fn settings_path(app: &AppHandle) -> tauri::Result<std::path::PathBuf> {
    let dir = app.path().app_config_dir()?;
    Ok(dir.join("settings.json"))
}

fn load_initial_settings(
    path: &std::path::Path,
) -> Result<(Settings, Option<SettingsNotice>), String> {
    match Settings::load(path) {
        Ok(settings) => {
            // Persist generated identifiers, canonical casing and restrictive file permissions.
            settings.save(path).map_err(|error| error.to_string())?;
            Ok((settings, None))
        }
        Err(SettingsError::NotFound(_)) => {
            let settings = Settings::default()
                .normalized()
                .map_err(|error| error.to_string())?;
            settings.save(path).map_err(|error| error.to_string())?;
            Ok((settings, None))
        }
        Err(error) if error.is_legacy_shared_code() => {
            let (settings, backup_path) = Settings::migrate_legacy_shared_code(path)
                .map_err(|migration_error| migration_error.to_string())?;
            let backup_name = backup_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("settings.legacy-v3.json");
            let notice = SettingsNotice {
                kind: SettingsNoticeKind::LegacyPairingMigrated,
                backup_file: backup_name.to_string(),
            };
            Ok((settings, Some(notice)))
        }
        Err(SettingsError::Corrupt { .. } | SettingsError::Validation { .. }) => {
            let (settings, backup_path) = Settings::recover_invalid(path)
                .map_err(|recovery_error| recovery_error.to_string())?;
            let backup_name = backup_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("settings.invalid-v4.json");
            let notice = SettingsNotice {
                kind: SettingsNoticeKind::InvalidSettingsRecovered,
                backup_file: backup_name.to_string(),
            };
            Ok((settings, Some(notice)))
        }
        Err(error) => Err(error.to_string()),
    }
}

fn runtime_settings_changed(previous: &Settings, next: &Settings) -> bool {
    previous.limits != next.limits
        || previous.sync != next.sync
        || previous.security != next.security
}

fn discovery_settings_changed(previous: &Settings, next: &Settings) -> bool {
    previous.sync != next.sync || previous.security != next.security
}

fn configure_runtime(
    state: &AppState,
    settings: &Settings,
    should_run: bool,
) -> Result<(), String> {
    if should_run {
        let presence_error = state
            .presence_service
            .ensure(settings.clone(), settings.sync_device_id())
            .err()
            .map(|error| error.to_string());
        if let Err(error) = state
            .sync_engine
            .start(settings.clone(), settings.sync_device_id())
        {
            state.presence_service.disable();
            return Err(error.to_string());
        }
        if let Some(error) = presence_error {
            state
                .sync_engine
                .record_error(format!("mDNS presence unavailable: {error}"));
        }
        Ok(())
    } else {
        let result = state.sync_engine.stop().map_err(|error| error.to_string());
        state.presence_service.disable();
        result
    }
}

fn restore_discovery_cache(state: &AppState, cached_devices: Option<&[DiscoveredDevice]>) {
    if let Some(devices) = cached_devices {
        state
            .sync_engine
            .replace_discovered_devices(None, devices.to_vec());
    }
}

fn transaction_error(stage: &str, error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => format!("failed to {stage}: {error}; previous runtime settings restored"),
        Err(rollback_error) => {
            format!("failed to {stage}: {error}; runtime rollback also failed: {rollback_error}")
        }
    }
}

fn ensure_current_revision(
    current_revision: &std::sync::atomic::AtomicU64,
    expected_revision: u64,
) -> Result<(), String> {
    if current_revision.load(Ordering::SeqCst) == expected_revision {
        Ok(())
    } else {
        Err("discovery result discarded because settings changed".to_string())
    }
}

fn validate_discovery_selection(value: Option<String>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 45 {
        return Err("selected local ip is too long".to_string());
    }
    let selected = value
        .parse::<Ipv4Addr>()
        .map_err(|_| "selected local ip must be a valid IPv4 address".to_string())?;
    let selected = selected.to_string();
    if !selected_ip_is_assigned(&selected, &net::list_network_interfaces()) {
        return Err("selected local ip is not assigned on this machine".to_string());
    }
    Ok(Some(selected))
}

fn selected_ip_is_assigned(selected: &str, interfaces: &[NetworkInterfaceOption]) -> bool {
    interfaces.iter().any(|interface| interface.ip == selected)
}

struct DiscoveryLease {
    singleflight: Arc<DiscoverySingleflight>,
    request: DiscoveryRequest,
}

impl DiscoveryLease {
    fn claim(
        singleflight: Arc<DiscoverySingleflight>,
        request: DiscoveryRequest,
    ) -> Result<Self, String> {
        let mut active = singleflight
            .active
            .lock()
            .map_err(|_| "discovery singleflight lock poisoned".to_string())?;
        if active.is_some() {
            return Err("discovery already in progress".to_string());
        }
        *active = Some(request.clone());
        drop(active);
        Ok(Self {
            singleflight,
            request,
        })
    }
}

impl Drop for DiscoveryLease {
    fn drop(&mut self) {
        let mut active = self
            .singleflight
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.as_ref() == Some(&self.request) {
            active.take();
        }
    }
}

pub fn initialize_runtime(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let path = settings_path(app).map_err(|e| e.to_string())?;
    let _update = state
        .settings_update
        .lock()
        .map_err(|_| "settings update lock poisoned".to_string())?;
    let (settings, settings_notice) = load_initial_settings(&path)?;
    let mut guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?;
    *guard = settings.clone();
    drop(guard);
    state.settings_revision.fetch_add(1, Ordering::SeqCst);
    *state
        .settings_notice
        .lock()
        .map_err(|_| "settings notice lock poisoned".to_string())? = settings_notice;

    if let Err(error) = configure_runtime(state, &settings, settings.sync.enabled) {
        state
            .sync_engine
            .record_error(format!("runtime startup failed: {error}"));
    }
    Ok(())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())
        .map(|guard| guard.clone())
}

#[tauri::command]
pub async fn set_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    update: SettingsUpdate,
) -> Result<(), String> {
    let path = settings_path(&app).map_err(|e| e.to_string())?;
    let state = state.inner().clone();
    task::spawn_blocking(move || set_settings_blocking(&state, &path, update))
        .await
        .map_err(|error| format!("settings worker failed: {error}"))?
}

fn set_settings_blocking(
    state: &AppState,
    path: &std::path::Path,
    update: SettingsUpdate,
) -> Result<(), String> {
    let _update = state
        .settings_update
        .lock()
        .map_err(|_| "settings update lock poisoned".to_string())?;
    let previous = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?
        .clone();
    let next = previous
        .apply_update(update)
        .map_err(|error| error.to_string())?;

    let was_running = state.sync_engine.status(&previous, None).running;
    let settings_changed = previous != next;
    let runtime_changed = runtime_settings_changed(&previous, &next);
    let discovery_changed = discovery_settings_changed(&previous, &next);
    let runtime_must_be_applied = runtime_changed || !was_running;
    let cached_devices = runtime_must_be_applied.then(|| state.sync_engine.devices(None));

    if runtime_must_be_applied {
        if state.sync_engine.has_active_transfers() {
            return Err(
                "settings that restart synchronization cannot be applied during an active transfer"
                    .to_string(),
            );
        }
        if let Err(error) = configure_runtime(state, &next, true) {
            let rollback = configure_runtime(state, &previous, was_running);
            restore_discovery_cache(state, cached_devices.as_deref());
            return Err(transaction_error("apply runtime settings", error, rollback));
        }
    }

    if settings_changed {
        if let Err(error) = next.save(path) {
            let rollback = if runtime_must_be_applied {
                configure_runtime(state, &previous, was_running)
            } else {
                Ok(())
            };
            restore_discovery_cache(state, cached_devices.as_deref());
            return Err(transaction_error(
                "persist settings",
                error.to_string(),
                rollback,
            ));
        }

        *state
            .settings
            .lock()
            .map_err(|_| "settings lock poisoned".to_string())? = next.clone();
        if discovery_changed {
            state.settings_revision.fetch_add(1, Ordering::SeqCst);
        }
    }

    if discovery_changed {
        state
            .sync_engine
            .replace_discovered_devices(None, Vec::new());
    } else {
        restore_discovery_cache(state, cached_devices.as_deref());
    }
    Ok(())
}

#[tauri::command]
pub fn generate_pairing_key() -> String {
    settings::generate_pairing_key()
}

#[tauri::command]
pub fn sync_status(state: State<'_, AppState>) -> Result<RuntimeStatus, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?
        .clone();
    let notice = state
        .settings_notice
        .lock()
        .map_err(|_| "settings notice lock poisoned".to_string())?
        .clone();
    let mut status = state.sync_engine.status(&settings, None);
    status.settings_notice = notice;
    Ok(status)
}

#[tauri::command]
pub async fn discover_devices(
    state: State<'_, AppState>,
    selected_local_ip: Option<String>,
) -> Result<Vec<DiscoveredDevice>, String> {
    let selected_local_ip = validate_discovery_selection(selected_local_ip)?;
    let settings_revision = state.settings_revision.load(Ordering::SeqCst);
    let request = DiscoveryRequest {
        settings_revision,
        selected_local_ip: selected_local_ip.clone(),
    };
    let lease = DiscoveryLease::claim(state.discovery_singleflight.clone(), request)?;
    let state = state.inner().clone();

    task::spawn_blocking(move || -> Result<Vec<DiscoveredDevice>, String> {
        let _lease = lease;
        let settings = {
            let _update = state
                .settings_update
                .lock()
                .map_err(|_| "settings update lock poisoned".to_string())?;
            ensure_current_revision(&state.settings_revision, settings_revision)?;
            state
                .settings
                .lock()
                .map_err(|_| "settings lock poisoned".to_string())?
                .clone()
        };
        let devices = net::discover_devices(
            &settings.sync_device_id(),
            &settings.sync.shared_code,
            selected_local_ip.as_deref(),
            2200,
        )
        .map_err(|error| error.to_string())?;

        {
            let _update = state
                .settings_update
                .lock()
                .map_err(|_| "settings update lock poisoned".to_string())?;
            ensure_current_revision(&state.settings_revision, settings_revision)?;
            state.sync_engine.refresh_discovered_devices(devices);
        }

        std::thread::sleep(std::time::Duration::from_millis(MANUAL_DISCOVERY_SETTLE_MS));

        let _update = state
            .settings_update
            .lock()
            .map_err(|_| "settings update lock poisoned".to_string())?;
        ensure_current_revision(&state.settings_revision, settings_revision)?;
        Ok(state.sync_engine.devices(selected_local_ip.as_deref()))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn cached_devices(
    state: State<'_, AppState>,
    selected_local_ip: Option<String>,
) -> Result<Vec<DiscoveredDevice>, String> {
    let selected_local_ip = validate_discovery_selection(selected_local_ip)?;
    let _update = state
        .settings_update
        .lock()
        .map_err(|_| "settings update lock poisoned".to_string())?;
    Ok(state.sync_engine.devices(selected_local_ip.as_deref()))
}

#[tauri::command]
pub fn list_network_interfaces() -> Result<Vec<NetworkInterfaceOption>, String> {
    Ok(net::list_network_interfaces())
}

#[tauri::command]
pub fn get_runtime_logs(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<RuntimeLog>, String> {
    Ok(state.sync_engine.logs(limit.unwrap_or(250)))
}

#[tauri::command]
pub fn get_transfer_progress(state: State<'_, AppState>) -> Result<Vec<TransferProgress>, String> {
    Ok(state.sync_engine.transfers())
}

#[tauri::command]
pub fn clear_runtime_logs(state: State<'_, AppState>) -> Result<(), String> {
    state.sync_engine.clear_logs();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64;
    use uuid::Uuid;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("lan-clipboard-commands-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn initial_load_creates_valid_settings_only_when_missing() {
        let directory = TestDirectory::new();
        let path = directory.path().join("settings.json");

        let (settings, notice) = load_initial_settings(&path).expect("initialize settings");

        assert!(notice.is_none());
        assert_eq!(settings.sync.shared_code.len(), 26);
        assert_eq!(Settings::load(&path).expect("reload settings"), settings);
    }

    #[test]
    fn initial_load_backs_up_corrupt_settings_and_recovers_with_notice() {
        let directory = TestDirectory::new();
        let path = directory.path().join("settings.json");
        let original = b"{broken-json";
        std::fs::write(&path, original).expect("write corrupt settings");

        let (settings, notice) = load_initial_settings(&path).expect("recover corrupt settings");

        let backup = directory.path().join("settings.invalid-v4.json");
        assert_eq!(std::fs::read(backup).expect("read exact backup"), original);
        assert_eq!(
            Settings::load(&path).expect("read recovered settings"),
            settings
        );
        let notice = notice.expect("recovery notice");
        assert_eq!(notice.kind, SettingsNoticeKind::InvalidSettingsRecovered);
        assert_eq!(notice.backup_file, "settings.invalid-v4.json");
    }

    #[test]
    fn initial_load_still_reports_io_errors() {
        let directory = TestDirectory::new();

        let error = load_initial_settings(directory.path()).expect_err("directory read must fail");

        assert!(error.contains("failed to read settings file"));
        assert!(!directory.path().join("settings.invalid-v4.json").exists());
    }

    #[test]
    fn initial_load_migrates_legacy_code_with_visible_notice() {
        let directory = TestDirectory::new();
        let path = directory.path().join("settings.json");
        let mut legacy = Settings::default();
        legacy.sync.shared_code = "123456".to_string();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy settings"),
        )
        .expect("write legacy settings");

        let (settings, notice) = load_initial_settings(&path).expect("migrate settings");

        assert_eq!(settings.sync.shared_code.len(), 26);
        let notice = notice.expect("migration notice");
        assert_eq!(notice.kind, SettingsNoticeKind::LegacyPairingMigrated);
        assert_eq!(notice.backup_file, "settings.legacy-v3.json");
        assert!(directory.path().join("settings.legacy-v3.json").exists());
    }

    #[test]
    fn runtime_change_detection_ignores_ui_only_updates() {
        let previous = Settings::default();
        let mut ui_only = previous.clone();
        ui_only.ui.language = "en-US".to_string();
        assert!(!runtime_settings_changed(&previous, &ui_only));

        let mut network_change = previous.clone();
        network_change.sync.listen_port += 1;
        assert!(runtime_settings_changed(&previous, &network_change));
        assert!(discovery_settings_changed(&previous, &network_change));

        let mut limit_change = previous.clone();
        limit_change.limits.max_item_bytes += 1;
        assert!(runtime_settings_changed(&previous, &limit_change));
        assert!(!discovery_settings_changed(&previous, &limit_change));
    }

    #[test]
    fn stale_discovery_revision_is_rejected() {
        let revision = AtomicU64::new(4);
        assert!(ensure_current_revision(&revision, 4).is_ok());
        assert!(ensure_current_revision(&revision, 3).is_err());
    }

    #[test]
    fn discovery_singleflight_rejects_queued_scans_and_releases_leader() {
        let singleflight = Arc::new(DiscoverySingleflight::default());
        let request = DiscoveryRequest {
            settings_revision: 7,
            selected_local_ip: Some("192.168.1.2".to_string()),
        };

        let leader = DiscoveryLease::claim(singleflight.clone(), request.clone())
            .expect("claim discovery leader");
        assert!(DiscoveryLease::claim(singleflight.clone(), request.clone()).is_err());
        assert!(DiscoveryLease::claim(
            singleflight.clone(),
            DiscoveryRequest {
                settings_revision: 7,
                selected_local_ip: Some("192.168.2.2".to_string()),
            },
        )
        .is_err());
        drop(leader);
        assert!(singleflight.active.lock().expect("flight state").is_none());

        let next = DiscoveryLease::claim(singleflight.clone(), request)
            .expect("released flight accepts next scan");
        drop(next);
    }

    #[test]
    fn selected_discovery_ip_must_be_well_formed_and_assigned() {
        assert!(validate_discovery_selection(Some("not-an-ip".to_string())).is_err());
        assert!(validate_discovery_selection(Some("x".repeat(46))).is_err());

        let interfaces = vec![NetworkInterfaceOption {
            name: "test0".to_string(),
            ip: "192.168.20.4".to_string(),
            label: "test0 (192.168.20.4)".to_string(),
        }];
        assert!(selected_ip_is_assigned("192.168.20.4", &interfaces));
        assert!(!selected_ip_is_assigned("192.168.20.5", &interfaces));
    }
}
