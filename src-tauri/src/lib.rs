#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;
#[cfg(any(target_os = "macos", windows))]
use tauri::include_image;
use tauri::{AppHandle, Manager, Url, UserAttentionType, WebviewWindow, WebviewWindowBuilder};

#[cfg(any(target_os = "macos", windows))]
const TRAY_ID: &str = "main-tray";
#[cfg(any(target_os = "macos", windows))]
const TRAY_MENU_SHOW_ID: &str = "show";
#[cfg(any(target_os = "macos", windows))]
const TRAY_MENU_QUIT_ID: &str = "quit";
#[cfg(target_os = "macos")]
const TRAY_DEFAULT_ICON: tauri::image::Image<'_> = include_image!("./icons/white.png");
#[cfg(windows)]
const TRAY_DEFAULT_ICON: tauri::image::Image<'_> = include_image!("./icons/32x32.png");
#[cfg(any(target_os = "macos", windows))]
const TRAY_ATTENTION_ICON: tauri::image::Image<'_> = include_image!("./icons/tray-attention.png");

const ALLOWED_WEB_ORIGINS: &[&str] = &[
    "https://snack.mechlabs.cn",
    "https://qasnack.mechlabs.cn",
    "http://localhost:3000",
    "http://127.0.0.1:3000",
];

#[cfg(windows)]
struct DesktopAttentionState {
    active: AtomicBool,
}

#[tauri::command]
fn exit_after_force_update_cancel(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to close the desktop shell".to_string());
    }

    app.exit(0);
    Ok(())
}

#[tauri::command]
fn set_desktop_attention(
    window: WebviewWindow,
    unread_count: u32,
    level: String,
) -> Result<(), String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to update desktop attention".to_string());
    }

    let badge_count = match unread_count {
        0 => None,
        1..=99 => Some(i64::from(unread_count)),
        _ => Some(99),
    };
    let _ = window.set_badge_count(badge_count);
    set_badge_label(&window, unread_count);
    update_tray_attention(&window.app_handle(), unread_count);

    if unread_count > 0 && should_request_window_attention() {
        let request_type = if level == "critical" || level == "mention" {
            UserAttentionType::Critical
        } else {
            UserAttentionType::Informational
        };
        let _ = window.request_user_attention(Some(request_type));
    } else {
        let _ = window.request_user_attention(None);
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn should_request_window_attention() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn should_request_window_attention() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn set_badge_label(window: &WebviewWindow, unread_count: u32) {
    let badge_label = if unread_count == 0 {
        None
    } else if unread_count > 99 {
        Some("99+".to_string())
    } else {
        Some(unread_count.to_string())
    };
    let _ = window.set_badge_label(badge_label);
}

#[cfg(not(target_os = "macos"))]
fn set_badge_label(_window: &WebviewWindow, _unread_count: u32) {}

#[cfg(target_os = "macos")]
fn update_tray_attention(app: &AppHandle, unread_count: u32) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if unread_count > 0 {
            let _ = tray.set_icon(Some(TRAY_ATTENTION_ICON));
            let _ = tray.set_tooltip(Some("Snack - 有未读消息"));
        } else {
            let _ = tray.set_icon_with_as_template(Some(TRAY_DEFAULT_ICON), true);
            let _ = tray.set_tooltip(Some("Snack"));
        }
    }
}

#[cfg(windows)]
fn update_tray_attention(app: &AppHandle, unread_count: u32) {
    let active = unread_count > 0;
    if let Some(state) = app.try_state::<Arc<DesktopAttentionState>>() {
        state.active.store(active, Ordering::Relaxed);
    }
    if !active {
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            let _ = tray.set_icon(Some(TRAY_DEFAULT_ICON));
            let _ = tray.set_tooltip(Some("Snack"));
        }
    } else if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let label = format_unread_count(unread_count);
        let _ = tray.set_tooltip(Some(format!("Snack - {label} 条未读")));
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn update_tray_attention(_app: &AppHandle, _unread_count: u32) {}

#[cfg(windows)]
fn format_unread_count(unread_count: u32) -> String {
    if unread_count > 99 {
        "99+".to_string()
    } else {
        unread_count.to_string()
    }
}

fn is_allowed_web_origin(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };

    let origin = match url.port() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    };

    ALLOWED_WEB_ORIGINS.contains(&origin.as_str())
}

fn desktop_user_agent() -> String {
    format!(
        "{} SnackDesktop/{}/{}",
        env!("SNACK_DESKTOP_BASE_UA").trim(),
        env!("SNACK_DESKTOP_ARCH"),
        env!("SNACK_DESKTOP_VERSION")
    )
}

#[cfg(windows)]
fn setup_windows_tray(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let attention_state = Arc::new(DesktopAttentionState {
        active: AtomicBool::new(false),
    });
    app.manage(attention_state.clone());

    let show = MenuItem::with_id(app, TRAY_MENU_SHOW_ID, "显示 Snack", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TRAY_DEFAULT_ICON)
        .tooltip("Snack")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};

            let should_show = match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => true,
                _ => false,
            };

            if should_show {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    register_status_menu_events(app);

    let handle = app.handle().clone();
    std::thread::spawn(move || {
        let mut attention_frame = false;
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let active = attention_state.active.load(Ordering::Relaxed);
            let Some(tray) = handle.tray_by_id(TRAY_ID) else {
                continue;
            };
            if active {
                attention_frame = !attention_frame;
                let icon = if attention_frame {
                    TRAY_ATTENTION_ICON
                } else {
                    TRAY_DEFAULT_ICON
                };
                let _ = tray.set_icon(Some(icon));
            } else if attention_frame {
                attention_frame = false;
                let _ = tray.set_icon(Some(TRAY_DEFAULT_ICON));
            }
        }
    });

    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(target_os = "macos")]
fn should_show_window_on_reopen(has_visible_windows: bool) -> bool {
    !has_visible_windows
}

#[cfg(any(target_os = "macos", windows))]
fn register_status_menu_events(app: &mut tauri::App) {
    app.on_menu_event(|app, event| match event.id().as_ref() {
        TRAY_MENU_SHOW_ID => show_main_window(app),
        TRAY_MENU_QUIT_ID => app.exit(0),
        _ => {}
    });
}

#[cfg(any(target_os = "macos", windows))]
fn install_close_to_status_menu(window: &WebviewWindow) {
    let close_window = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = close_window.hide();
        }
    });
}

#[cfg(not(any(target_os = "macos", windows)))]
fn install_close_to_status_menu(_window: &WebviewWindow) {}

#[cfg(target_os = "macos")]
fn setup_macos_status_menu(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let show = MenuItem::with_id(app, TRAY_MENU_SHOW_ID, "显示 Snack", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TRAY_DEFAULT_ICON)
        .icon_as_template(true)
        .tooltip("Snack")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .build(app)?;

    register_status_menu_events(app);

    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            exit_after_force_update_cancel,
            set_desktop_attention
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            setup_macos_status_menu(app)?;

            #[cfg(windows)]
            setup_windows_tray(app)?;

            let window_config = app
                .config()
                .app
                .windows
                .first()
                .expect("missing main window config");

            let user_agent = desktop_user_agent();

            let window = WebviewWindowBuilder::from_config(app, window_config)?
                .user_agent(&user_agent)
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
                if should_show_window_on_reopen(has_visible_windows) {
                    show_main_window(app);
                }
            }
        });
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::should_show_window_on_reopen;

    #[test]
    fn reopens_hidden_main_window_from_dock() {
        assert!(should_show_window_on_reopen(false));
    }

    #[test]
    fn does_not_steal_focus_when_reopen_finds_visible_windows() {
        assert!(!should_show_window_on_reopen(true));
    }
}
