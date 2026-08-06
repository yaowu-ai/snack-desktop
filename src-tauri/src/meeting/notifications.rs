use notify_rust::Notification;
use tauri::{AppHandle, Manager, Url};

const MEETING_SETTINGS_PATH: &str = "/meeting/settings";
const MEETING_RECORDS_PATH: &str = "/meeting";

/// Notify after the user-initiated local transcription resource install finishes.
/// The notification is created through notify-rust directly because its desktop
/// handle exposes click actions, while the Tauri notification plugin only emits
/// action events on mobile.
pub(crate) fn notify_model_ready(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || show_model_ready_notification(app));
}

/// Notify after a local transcription finishes. Clicking the notification
/// always returns to the audio transcription records tab.
pub(crate) fn notify_transcript_ready(app: &AppHandle, recording_id: &str) {
    let app = app.clone();
    let _ = recording_id;
    tauri::async_runtime::spawn_blocking(move || show_transcript_ready_notification(app));
}

fn show_model_ready_notification(app: AppHandle) {
    configure_notification_identity(&app);
    let result = Notification::new()
        .summary("模型已经下载完成")
        .body("完成设置，马上体验 Snack 会议录音。")
        .action("open-meeting-settings", "完成设置")
        .show();

    let handle = match result {
        Ok(handle) => handle,
        Err(error) => {
            log_notification_error(&app, &error.to_string());
            return;
        }
    };
    handle.wait_for_action(move |action| handle_notification_action(&app, action));
}

fn show_transcript_ready_notification(app: AppHandle) {
    configure_notification_identity(&app);
    let result = Notification::new()
        .summary("音频转写已完成")
        .body("点击查看音频转写记录。")
        .action("open-meeting-records", "查看记录")
        .show();

    let handle = match result {
        Ok(handle) => handle,
        Err(error) => {
            log_notification_error(&app, &error.to_string());
            return;
        }
    };
    handle.wait_for_action(move |action| {
        if should_open_notification(action) {
            if let Err(error) = open_meeting_path(&app, MEETING_RECORDS_PATH) {
                log_notification_error(&app, &error);
            }
        }
    });
}

fn configure_notification_identity(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = notify_rust::set_application(&app.config().identifier);
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

fn handle_notification_action(app: &AppHandle, action: &str) {
    if !should_open_notification(action) {
        return;
    }
    if let Err(error) = open_meeting_path(app, MEETING_SETTINGS_PATH) {
        log_notification_error(app, &error);
    }
}

fn log_notification_error(app: &AppHandle, error: &str) {
    crate::logging::write_app_log(
        app,
        "warn",
        "meeting",
        "meeting model notification failed",
        Some(&serde_json::json!({ "error": error })),
    );
}

fn should_open_notification(action: &str) -> bool {
    action != "__closed"
}

fn open_meeting_path(app: &AppHandle, path: &str) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main webview is unavailable".to_string())?;
    let current_url = window.url().map_err(|error| error.to_string())?;
    if !crate::web::is_allowed_web_origin(&current_url) {
        return Err("main webview origin is not allowed".to_string());
    }

    let target_url = meeting_url(current_url, path);
    let _ = window.show();
    let _ = window.unminimize();
    crate::window_state::recover_if_unreachable(&window);
    let _ = window.set_focus();
    window
        .navigate(target_url)
        .map_err(|error| error.to_string())
}

fn meeting_url(mut url: Url, path: &str) -> Url {
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_click_targets_guided_meeting_settings() {
        let url = meeting_url(
            Url::parse("http://localhost:3000/apps?from=test#section").expect("valid URL"),
            MEETING_SETTINGS_PATH,
        );
        assert_eq!(url.as_str(), "http://localhost:3000/meeting/settings");
        assert!(should_open_notification("default"));
        assert!(should_open_notification("open-meeting-settings"));
        assert!(!should_open_notification("__closed"));
    }

    #[test]
    fn transcription_notification_targets_audio_records() {
        let url = meeting_url(
            Url::parse("http://localhost:3000/meeting/settings?from=notice").expect("valid URL"),
            MEETING_RECORDS_PATH,
        );
        assert_eq!(url.as_str(), "http://localhost:3000/meeting");
    }
}
