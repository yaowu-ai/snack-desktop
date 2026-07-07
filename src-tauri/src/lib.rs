mod app_menu;
mod attention;
mod commands;
mod constants;
mod download;
mod navigation;
mod platform;
mod web;

use app_menu::install_close_to_status_menu;
#[cfg(target_os = "macos")]
use app_menu::setup_macos_status_menu;
#[cfg(any(target_os = "macos", windows))]
use app_menu::setup_navigation_menu;
#[cfg(windows)]
use app_menu::setup_windows_tray;
use navigation::handle_new_window_request;
use tauri::WebviewWindowBuilder;
use web::desktop_user_agent;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::download_snack_file,
            commands::exit_after_force_update_cancel,
            commands::open_downloaded_file,
            commands::reveal_downloaded_file,
            commands::set_desktop_attention
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            setup_macos_status_menu(app)?;

            #[cfg(windows)]
            setup_windows_tray(app)?;

            #[cfg(any(target_os = "macos", windows))]
            setup_navigation_menu(app)?;

            let window_config = app
                .config()
                .app
                .windows
                .first()
                .expect("missing main window config");

            let user_agent = desktop_user_agent();
            let app_handle = app.handle().clone();

            let window = WebviewWindowBuilder::from_config(app, window_config)?
                .user_agent(&user_agent)
                .on_new_window(move |url, _features| handle_new_window_request(&app_handle, url))
                .build()?;

            install_close_to_status_menu(&window);

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Snack desktop client")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = event
            {
                if app_menu::should_show_window_on_reopen(has_visible_windows) {
                    app_menu::show_main_window(app);
                }
            }
        });
}
