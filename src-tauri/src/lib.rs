mod clipboard;
mod commands;
mod desktop;
mod net;
mod protocol;
mod settings;
mod state;
mod storage;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let _ = desktop::ensure_tray_icon(app);
            desktop::hide_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None::<Vec<&str>>,
        ))
        .manage(state::AppState::default())
        .setup(|app| {
            desktop::setup(app)?;
            let state = app.state::<state::AppState>();
            commands::initialize_runtime(app.handle(), &state)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::set_settings,
            commands::generate_pairing_key,
            commands::sync_status,
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
