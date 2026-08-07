mod app_menu;
mod attention;
mod commands;
mod constants;
mod download;
mod logging;
mod meeting;
mod navigation;
mod platform;
mod record_import;
mod web;
mod window_state;

use app_menu::install_close_to_status_menu;
#[cfg(target_os = "macos")]
use app_menu::setup_macos_status_menu;
#[cfg(any(target_os = "macos", windows))]
use app_menu::setup_navigation_menu;
#[cfg(windows)]
use app_menu::setup_windows_tray;
use navigation::handle_new_window_request;
use tauri::{webview::PageLoadEvent, WebviewWindowBuilder};
use tauri_plugin_deep_link::DeepLinkExt;
use web::desktop_user_agent;

pub fn run() {
    tauri::Builder::default()
        // Must be registered first so Windows forwards a deep-link CLI launch to the active app.
        .plugin(tauri_plugin_single_instance::init(|_, _, _| {}))
        .register_uri_scheme_protocol(meeting::overlay::OVERLAY_SCHEME, |ctx, request| {
            meeting::overlay::handle_overlay_request(ctx.app_handle(), ctx.webview_label(), request)
        })
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    meeting::quick_access::handle_shortcut(app, event.state());
                })
                .build(),
        )
        .plugin(tauri_plugin_deep_link::init())
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
            commands::write_desktop_log,
            meeting::meeting_cancel_install,
            meeting::meeting_check_permissions,
            meeting::meeting_clear_task_records,
            meeting::meeting_get_recording_status,
            meeting::meeting_get_snapshot,
            meeting::meeting_choose_storage_directory,
            meeting::meeting_generate_notes,
            meeting::meeting_open_local_file,
            meeting::meeting_open_notes_in_chat,
            meeting::meeting_notify_notes_completed,
            meeting::meeting_rename_task_record,
            meeting::meeting_install_model,
            meeting::meeting_import_audio,
            meeting::meeting_open_permission_settings,
            meeting::meeting_pause_install,
            meeting::meeting_resume_install,
            meeting::meeting_request_permissions,
            meeting::meeting_request_quick_recording,
            meeting::meeting_retry_pipeline,
            meeting::meeting_retry_submit,
            meeting::meeting_retranscribe,
            meeting::meeting_start_recording,
            meeting::meeting_stop_recording,
            meeting::meeting_update_settings,
            meeting::meeting_uninstall_model,
            meeting::overlay::dismiss_recording_reminder,
            meeting::overlay::dismiss_overlay,
            meeting::overlay::minimize_overlay,
            meeting::overlay::start_recording_from_reminder,
            record_import::claim_pending_record_import,
            record_import::acknowledge_record_import_prefilled
        ])
        .setup(|app| {
            record_import::initialize(app.handle()).map_err(std::io::Error::other)?;
            meeting::initialize(app.handle()).map_err(std::io::Error::other)?;
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
            let page_load_app_handle = app.handle().clone();

            let window = WebviewWindowBuilder::from_config(app, window_config)?
                .visible(false)
                .user_agent(&user_agent)
                .on_new_window(move |url, _features| handle_new_window_request(&app_handle, url))
                .on_page_load(move |window, payload| {
                    if payload.event() == PageLoadEvent::Finished {
                        record_import::handle_page_load(&page_load_app_handle, &window);
                    }
                })
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

            let app_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    record_import::handle_open_url(&app_handle, &url);
                }
            });

            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for url in urls {
                    record_import::handle_open_url(app.handle(), &url);
                }
            }

            // The hidden webview loads while display detection runs, then becomes visible once.
            window_state::restore_track_and_show(app.handle(), &window);

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
