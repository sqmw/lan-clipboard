use crate::clipboard;
use crate::net::{self, DiscoveredDevice, RuntimeLog, RuntimeStatus};
use crate::protocol::ClipboardPayload;
use crate::settings::Settings;
use crate::state::AppState;
use tauri::{AppHandle, Manager, State};

fn settings_path(app: &AppHandle) -> tauri::Result<std::path::PathBuf> {
    let dir = app.path().app_config_dir()?;
    Ok(dir.join("settings.json"))
}

#[tauri::command]
pub fn get_settings(app: AppHandle, state: State<'_, AppState>) -> Result<Settings, String> {
    let path = settings_path(&app).map_err(|e| e.to_string())?;
    let mut guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?;

    if let Ok(Some(mut loaded)) = Settings::load(&path) {
        loaded.ensure_sync_identifiers();
        if loaded.sync.peers.is_empty() { loaded.sync.peers = Vec::new(); }
        let _ = loaded.save(&path);
        *guard = loaded;
    }
    Ok(guard.clone())
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
    *guard = next;
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
pub fn write_clipboard_item(state: State<'_, AppState>, payload: ClipboardPayload) -> Result<(), String> {
    let guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?;
    let item = net::build_item(&payload, "local").ok_or_else(|| "empty payload".to_string())?;
    clipboard::write_item(&item, &guard.limits).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn start_sync(state: State<'_, AppState>) -> Result<(), String> {
    let guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?
        .clone();
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
pub fn sync_status(state: State<'_, AppState>) -> Result<RuntimeStatus, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?
        .clone();
    Ok(state.sync_engine.status(&settings.sync_device_id()))
}

#[tauri::command]
pub fn list_devices(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?;
    Ok(settings.sync.peers.clone())
}

#[tauri::command]
pub fn discover_devices(state: State<'_, AppState>) -> Result<Vec<DiscoveredDevice>, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_string())?
        .clone();
    net::discover_devices(&settings.sync_device_id(), &settings.sync.device_code, settings.sync.listen_port, 2200)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_runtime_logs(state: State<'_, AppState>, limit: Option<usize>) -> Result<Vec<RuntimeLog>, String> {
    Ok(state.sync_engine.logs(limit.unwrap_or(250)))
}

#[tauri::command]
pub fn clear_runtime_logs(state: State<'_, AppState>) -> Result<(), String> {
    state.sync_engine.clear_logs();
    Ok(())
}
