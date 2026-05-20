use crate::clipboard;
use crate::desktop;
use crate::net::{
    self, DiscoveredDevice, NetworkInterfaceOption, RuntimeLog, RuntimeStatus, TransferProgress,
};
use crate::protocol::ClipboardPayload;
use crate::settings::Settings;
use crate::state::AppState;
use tauri::{AppHandle, Manager, State};
use tokio::task;

const MANUAL_DISCOVERY_SETTLE_MS: u64 = 700;

pub fn settings_path(app: &AppHandle) -> tauri::Result<std::path::PathBuf> {
    let dir = app.path().app_config_dir()?;
    Ok(dir.join("settings.json"))
}

pub fn initialize_runtime(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let path = settings_path(app).map_err(|e| e.to_string())?;
    let mut guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?;

    if let Ok(Some(mut loaded)) = Settings::load(&path) {
        loaded.ensure_sync_identifiers();
        let _ = loaded.save(&path);
        *guard = loaded;
    } else {
        let current = guard.clone();
        let _ = current.save(&path);
    }

    let settings = guard.clone();
    drop(guard);
    state
        .presence_service
        .ensure(settings.clone(), settings.sync_device_id())
        .map_err(|e| e.to_string())?;
    if settings.sync.enabled {
        state
            .sync_engine
            .start(settings.clone(), settings.sync_device_id())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn get_settings(app: AppHandle, state: State<'_, AppState>) -> Result<Settings, String> {
    initialize_runtime(&app, &state)?;
    state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())
        .map(|guard| guard.clone())
}

#[tauri::command]
pub fn set_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    next: Settings,
) -> Result<(), String> {
    let path = settings_path(&app).map_err(|e| e.to_string())?;
    next.save(&path).map_err(|e| e.to_string())?;
    let mut guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?;
    *guard = next.clone();
    drop(guard);
    state
        .presence_service
        .ensure(next.clone(), next.sync_device_id())
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn read_clipboard_snapshot(state: State<'_, AppState>) -> Result<ClipboardPayload, String> {
    let guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?;
    clipboard::read_snapshot(&guard.limits).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn write_clipboard_item(
    state: State<'_, AppState>,
    payload: ClipboardPayload,
) -> Result<(), String> {
    let guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?;
    let item = net::build_item(&payload, "local")
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "empty payload".to_string())?;
    clipboard::write_item(&item, &guard.limits)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn start_sync(state: State<'_, AppState>) -> Result<(), String> {
    let guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?
        .clone();
    state
        .presence_service
        .ensure(guard.clone(), guard.sync_device_id())
        .map_err(|e| e.to_string())?;
    state
        .sync_engine
        .start(guard.clone(), guard.sync_device_id())
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn stop_sync(state: State<'_, AppState>) -> Result<(), String> {
    state.sync_engine.stop().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn sync_status(
    state: State<'_, AppState>,
    selected_local_ip: Option<String>,
) -> Result<RuntimeStatus, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?
        .clone();
    Ok(state
        .sync_engine
        .status(&settings, selected_local_ip.as_deref()))
}

#[tauri::command]
pub async fn discover_devices(
    state: State<'_, AppState>,
    selected_local_ip: Option<String>,
) -> Result<Vec<DiscoveredDevice>, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?
        .clone();
    let presence_service = state.presence_service.clone();
    let sync_engine = state.sync_engine.clone();
    let selected_local_ip = selected_local_ip.map(|value| value.trim().to_string());
    let device_id = settings.sync_device_id();
    let shared_code = settings.sync.shared_code.clone();

    task::spawn_blocking(move || -> Result<Vec<DiscoveredDevice>, String> {
        presence_service
            .ensure(settings.clone(), device_id.clone())
            .map_err(|e| e.to_string())?;
        let devices =
            net::discover_devices(&device_id, &shared_code, selected_local_ip.as_deref(), 2200)
                .map_err(|e| e.to_string())?;
        sync_engine.replace_discovered_devices(selected_local_ip.as_deref(), devices, &settings);
        std::thread::sleep(std::time::Duration::from_millis(MANUAL_DISCOVERY_SETTLE_MS));
        Ok(sync_engine.devices(selected_local_ip.as_deref()))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn cached_devices(
    state: State<'_, AppState>,
    selected_local_ip: Option<String>,
) -> Result<Vec<DiscoveredDevice>, String> {
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

#[tauri::command]
pub fn ensure_desktop_shell(app: AppHandle) -> Result<(), String> {
    desktop::ensure_tray_icon(&app).map_err(|e| e.to_string())
}
