use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    App, AppHandle, Manager, Runtime, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

const MENU_SHOW_WINDOW: &str = "show-window";
const MENU_QUIT_APP: &str = "quit-app";
#[cfg(target_os = "macos")]
const MACOS_TRAY_ICON_PNG: &[u8] = include_bytes!("../icons/trayTemplate@2x.png");

pub fn setup<R: Runtime>(app: &mut App<R>) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    register_main_window_behavior(app)?;
    ensure_tray_icon(app.handle())?;
    Ok(())
}

pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = ensure_main_window(app) else {
        return;
    };
    #[cfg(target_os = "windows")]
    let _ = window.set_skip_taskbar(false);
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

pub fn hide_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(target_os = "windows")]
        let _ = window.set_skip_taskbar(true);
        let _ = window.hide();
    }
}

fn register_main_window_behavior<R: Runtime>(app: &App<R>) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };
    attach_main_window_behavior(window);
    Ok(())
}

fn ensure_main_window<R: Runtime>(app: &AppHandle<R>) -> Option<WebviewWindow<R>> {
    if let Some(window) = app.get_webview_window("main") {
        return Some(window);
    }
    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
        .title("lan-clipboard")
        .inner_size(800.0, 600.0)
        .visible(false)
        .build()
        .ok()?;
    attach_main_window_behavior(window.clone());
    Some(window)
}

fn attach_main_window_behavior<R: Runtime>(window: WebviewWindow<R>) {
    let window_for_close = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            #[cfg(target_os = "windows")]
            let _ = window_for_close.set_skip_taskbar(true);
            let _ = window_for_close.hide();
        }
    });
}

pub fn ensure_tray_icon<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    if app.tray_by_id("main-tray").is_some() {
        return Ok(());
    }

    let menu = MenuBuilder::new(app)
        .text(MENU_SHOW_WINDOW, "打开窗口")
        .text(MENU_QUIT_APP, "退出")
        .build()?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .menu(&menu)
        .tooltip("lan-clipboard")
        .show_menu_on_left_click(false)
        .on_menu_event(
            |app, event: tauri::menu::MenuEvent| match event.id().as_ref() {
                MENU_SHOW_WINDOW => show_main_window(app),
                MENU_QUIT_APP => app.exit(0),
                _ => {}
            },
        )
        .on_tray_icon_event(|tray: &tauri::tray::TrayIcon<R>, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    #[cfg(target_os = "macos")]
    {
        if let Ok(icon) = tauri::image::Image::from_bytes(MACOS_TRAY_ICON_PNG) {
            builder = builder.icon(icon);
        } else if let Some(icon) = app.default_window_icon().cloned() {
            builder = builder.icon(icon);
        }
        builder = builder.icon_as_template(true);
    }

    #[cfg(not(target_os = "macos"))]
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}
