mod clipboard;
mod commands;
mod net;
mod protocol;
mod settings;
mod state;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .manage(state::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::set_settings,
            commands::read_clipboard_snapshot,
            commands::write_clipboard_item,
            commands::start_sync,
            commands::stop_sync,
            commands::sync_status,
            commands::list_devices,
            commands::discover_devices,
            commands::cached_devices,
            commands::list_network_interfaces,
            commands::get_runtime_logs,
            commands::get_transfer_progress,
            commands::clear_runtime_logs,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
