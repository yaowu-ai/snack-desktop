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
    if QUICK_RECORDING_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = super::start_quick_recording(app.clone()).await;
        QUICK_RECORDING_PENDING.store(false, Ordering::SeqCst);
        if let Err(message) = result {
            notify_quick_recording_error(&app, &message);
        }
    });
}

fn notify_quick_recording_error(app: &AppHandle, message: &str) {
    let _ = app
        .notification()
        .builder()
        .title("Snack 会议录音未开始")
        .body(message)
        .show();
}
