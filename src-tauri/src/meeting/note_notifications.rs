//! The authenticated Web shell verifies recording ownership before showing and opening note results.
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use notify_rust::Notification;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

static HANDLER: Mutex<Option<String>> = Mutex::new(None);
static SHOWN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
pub(crate) const REQUEST_EVENT: &str = "meeting-note-result-requested";
const OPEN_EVENT: &str = "meeting-note-notification-open";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NoteNotificationTarget {
    recording_id: String,
    scope_key: String,
    origin: String,
    tab: String,
    outcome: String,
    key: String,
}

pub(crate) fn set_handler_ready(enabled: bool, handler_id: String) {
    if let Ok(mut handler) = HANDLER.lock() {
        if enabled {
            *handler = Some(handler_id);
        } else if handler.as_ref() == Some(&handler_id) {
            *handler = None;
        }
    }
}

pub(crate) fn reset_handler() {
    if let Ok(mut handler) = HANDLER.lock() {
        *handler = None;
    }
}

/// Legacy Web versions retain their existing notification path until a listener registers.
pub(crate) fn request_owned_result(app: &AppHandle, recording_id: &str, outcome: &str) -> bool {
    if !HANDLER.lock().is_ok_and(|handler| handler.is_some()) {
        return false;
    }
    let manager = app.state::<super::MeetingManagerState>();
    let Some(task) = manager
        .store
        .load_task_record(recording_id)
        .filter(|task| task.defer_transcription)
    else {
        return false;
    };
    app.emit_to(
        "main",
        REQUEST_EVENT,
        serde_json::json!({
            "recordingId": recording_id, "tab": "transcript", "outcome": outcome,
            "key": format!("{}:{}:{}", recording_id, outcome, task.updated_at),
        }),
    )
    .is_ok()
}

/// Web passes only an owner-verified target; click events must be verified again by Web.
pub(crate) fn show(app: AppHandle, target: NoteNotificationTarget) -> Result<(), String> {
    validate(&target)?;
    let window = app
        .get_webview_window("main")
        .ok_or("main webview is unavailable")?;
    if window
        .url()
        .map_err(|error| error.to_string())?
        .origin()
        .ascii_serialization()
        != target.origin
    {
        return Err("notification origin changed".into());
    }
    let key = format!("{}:{}", target.scope_key, target.key);
    if !SHOWN
        .lock()
        .map_err(|_| "notification lock unavailable")?
        .insert(key.clone())
    {
        return Ok(());
    }
    tauri::async_runtime::spawn_blocking(move || {
        super::notifications::configure_notification_identity(&app);
        let (title, body) = notification_text(&target);
        let result = Notification::new()
            .summary(title)
            .body(body)
            .action("open-note", "查看笔记")
            .show();
        let Ok(handle) = result else {
            if let Ok(mut shown) = SHOWN.lock() {
                shown.remove(&key);
            }
            return;
        };
        handle.wait_for_action(move |action| {
            if action == "__closed" {
                return;
            }
            let Some(window) = app.get_webview_window("main") else {
                return;
            };
            if !window
                .url()
                .is_ok_and(|url| url.origin().ascii_serialization() == target.origin)
            {
                return;
            }
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
            let _ = app.emit_to("main", OPEN_EVENT, &target);
        });
    });
    Ok(())
}

fn validate(target: &NoteNotificationTarget) -> Result<(), String> {
    if target.recording_id.is_empty()
        || target.recording_id.len() > 200
        || !target.recording_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return Err("invalid recording identifier".into());
    }
    let origin = tauri::Url::parse(&target.origin).map_err(|_| "invalid notification origin")?;
    if !crate::web::is_allowed_web_origin(&origin)
        || origin.origin().ascii_serialization() != target.origin
        || target.scope_key.is_empty()
        || target.scope_key.len() > 2048
        || target.key.is_empty()
        || target.key.len() > 1024
        || !["transcript", "summary"].contains(&target.tab.as_str())
        || !["success", "failure", "save_failed"].contains(&target.outcome.as_str())
    {
        return Err("invalid note notification target".into());
    }
    Ok(())
}

fn notification_text(target: &NoteNotificationTarget) -> (&'static str, &'static str) {
    match (target.tab.as_str(), target.outcome.as_str()) {
        (_, "save_failed") => ("录音保存失败", "点击返回录音笔记检查并恢复录音。"),
        ("summary", "success") => ("会议纪要已生成", "点击查看这场会议的纪要。"),
        ("summary", _) => ("会议纪要生成失败", "点击返回录音笔记重试。"),
        (_, "success") => ("音频转写已完成", "点击查看这场会议的原文。"),
        _ => ("音频转写失败", "点击返回录音笔记重试。"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> NoteNotificationTarget {
        NoteNotificationTarget {
            recording_id: "rec-1".into(),
            scope_key: "owner-scope".into(),
            origin: "http://localhost:3015".into(),
            tab: "transcript".into(),
            outcome: "success".into(),
            key: "result-1".into(),
        }
    }
    #[test]
    fn rejects_untrusted_routes_and_unsupported_result_kinds() {
        let mut value = target();
        assert!(validate(&value).is_ok());
        value.tab = "../account".into();
        assert!(validate(&value).is_err());
        value = target();
        value.origin = "https://untrusted.example".into();
        assert!(validate(&value).is_err());
        value = target();
        value.scope_key.clear();
        assert!(validate(&value).is_err());
    }
    #[test]
    fn stale_handler_cleanup_cannot_disable_its_replacement() {
        set_handler_ready(true, "old".into());
        set_handler_ready(true, "replacement".into());
        set_handler_ready(false, "old".into());
        assert_eq!(HANDLER.lock().unwrap().as_deref(), Some("replacement"));
        reset_handler();
        assert!(HANDLER.lock().unwrap().is_none());
    }
    #[test]
    fn distinguishes_recording_save_failures_from_asr_failures() {
        let mut value = target();
        value.outcome = "save_failed".into();
        assert_eq!(notification_text(&value).0, "录音保存失败");
        value.outcome = "failure".into();
        assert_eq!(notification_text(&value).0, "音频转写失败");
    }
}
