#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::Arc;

use tauri::{AppHandle, Manager, UserAttentionType, WebviewWindow};

#[cfg(any(target_os = "macos", windows))]
use crate::constants::{TRAY_ATTENTION_ICON, TRAY_DEFAULT_ICON, TRAY_ID};

#[cfg(windows)]
pub(crate) struct DesktopAttentionState {
    pub(crate) active: AtomicBool,
}

pub(crate) fn update_desktop_attention(window: &WebviewWindow, unread_count: u32, level: &str) {
    let badge_count = match unread_count {
        0 => None,
        1..=99 => Some(i64::from(unread_count)),
        _ => Some(99),
    };
    let _ = window.set_badge_count(badge_count);
    set_badge_label(window, unread_count);
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
