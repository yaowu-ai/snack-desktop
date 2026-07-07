mod app_menu;
mod attention;
mod commands;
mod constants;
mod download;
mod logging;
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
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::download_snack_file,
            commands::exit_after_force_update_cancel,
            commands::open_downloaded_file,
            commands::reveal_desktop_log_dir,
            commands::reveal_downloaded_file,
            commands::set_desktop_attention,
            commands::write_desktop_log
        ])
        .setup(|app| {
            logging::write_app_log(
                app.handle(),
                "info",
                "tauri",
                "Snack desktop starting",
                Some(&serde_json::json!({
                    "version": env!("SNACK_DESKTOP_VERSION"),
                    "platform": env!("SNACK_DESKTOP_PLATFORM"),
                    "arch": env!("SNACK_DESKTOP_ARCH"),
                    "logPath": logging::log_path(app.handle()).to_string_lossy(),
                })),
            );

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

            logging::write_app_log(
                app.handle(),
                "info",
                "tauri",
                "Main webview created",
                Some(&serde_json::json!({
                    "userAgent": user_agent,
                    "url": window.url().map(|url| url.to_string()).unwrap_or_default(),
                })),
            );

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
