use std::sync::atomic::{AtomicBool, Ordering};

use tauri::AppHandle;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_notification::NotificationExt;

use super::state::MeetingStore;

static QUICK_RECORDING_PENDING: AtomicBool = AtomicBool::new(false);

pub(crate) fn register_saved_shortcut(app: &AppHandle, store: &MeetingStore) -> Result<(), String> {
    let shortcut = store.load_settings().shortcut;
    let shortcuts = app.global_shortcut();
    if shortcuts.is_registered(shortcut.as_str()) {
        return Ok(());
    }
    shortcuts
        .register(shortcut.as_str())
        .map_err(|error| format!("无法注册快捷键: {error}"))
}

pub(crate) fn replace_shortcut(app: &AppHandle, previous: &str, next: &str) -> Result<(), String> {
    if previous == next {
        return Ok(());
    }
    let shortcuts = app.global_shortcut();
    let previous_registered = shortcuts.is_registered(previous);
    if shortcuts.is_registered(next) {
        unregister_if_registered(shortcuts, previous, previous_registered)?;
        return Ok(());
    }
    unregister_if_registered(shortcuts, previous, previous_registered)?;
    if let Err(error) = shortcuts.register(next) {
        if previous_registered {
            let _ = shortcuts.register(previous);
        }
        return Err(format!("快捷键已被占用或格式不正确: {error}"));
    }
    Ok(())
}

fn unregister_if_registered(
    shortcuts: &tauri_plugin_global_shortcut::GlobalShortcut<tauri::Wry>,
    shortcut: &str,
    registered: bool,
) -> Result<(), String> {
    if !registered {
        return Ok(());
    }
    shortcuts
        .unregister(shortcut)
        .map_err(|error| format!("无法释放原快捷键: {error}"))
}

pub(crate) fn handle_shortcut(app: &AppHandle, state: ShortcutState) {
    if state == ShortcutState::Pressed {
        request_quick_recording(app);
    }
}

pub(crate) fn request_quick_recording(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(message) = request_quick_recording_and_wait(app.clone()).await {
            notify_quick_recording_error(&app, &message);
        }
    });
}

/// Runs the quick-recording flow to its real startup result. Webview callers use
/// this variant so capture and permission failures can be shown in the page
/// instead of being hidden behind an asynchronous system notification.
pub(crate) async fn request_quick_recording_and_wait(app: AppHandle) -> Result<(), String> {
    if QUICK_RECORDING_PENDING.swap(true, Ordering::SeqCst) {
        return Err("录音正在启动，请稍候".to_string());
    }
    // The native entry first checks existing permission state. Fully
    // authorized users start immediately without entering a request flow.
    let result = super::start_quick_recording(app).await;
    QUICK_RECORDING_PENDING.store(false, Ordering::SeqCst);
    result
}

fn notify_quick_recording_error(app: &AppHandle, message: &str) {
    let _ = app
        .notification()
        .builder()
        .title("Snack 会议录音未开始")
        .body(message)
        .show();
}
