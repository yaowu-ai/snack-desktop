#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;
use tauri::{AppHandle, Manager, Url, UserAttentionType, WebviewWindow, WebviewWindowBuilder};
#[cfg(any(target_os = "macos", windows))]
use tauri::include_image;

#[cfg(any(target_os = "macos", windows))]
const TRAY_ID: &str = "main-tray";
#[cfg(any(target_os = "macos", windows))]
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
            let _ = tray.set_icon(Some(TRAY_DEFAULT_ICON));
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
    use tauri::tray::TrayIconBuilder;

    let attention_state = Arc::new(DesktopAttentionState {
        active: AtomicBool::new(false),
    });
    app.manage(attention_state.clone());

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TRAY_DEFAULT_ICON)
        .tooltip("Snack")
        .build(app)?;

    let handle = app.handle().clone();
    std::thread::spawn(move || {
        let mut attention_frame = false;
        loop {
            std::thread::sleep(Duration::from_millis(800));
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

#[cfg(target_os = "macos")]
fn setup_macos_status_menu(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::tray::TrayIconBuilder;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TRAY_DEFAULT_ICON)
        .tooltip("Snack")
        .build(app)?;

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

            WebviewWindowBuilder::from_config(app, window_config)?
                .user_agent(&user_agent)
                .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Snack desktop client");
}
