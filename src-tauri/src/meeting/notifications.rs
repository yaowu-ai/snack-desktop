use notify_rust::Notification;
use tauri::{AppHandle, Manager, Url};

const MEETING_SETTINGS_PATH: &str = "/meeting/settings";
const MEETING_SETTINGS_QUERY: &str = "guide=record";

/// Notify after the user-initiated local transcription resource install finishes.
/// The notification is created through notify-rust directly because its desktop
/// handle exposes click actions, while the Tauri notification plugin only emits
/// action events on mobile.
pub(crate) fn notify_model_ready(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || show_model_ready_notification(app));
}

fn show_model_ready_notification(app: AppHandle) {
    configure_notification_identity(&app);
    let result = Notification::new()
        .summary("Snack 本地转写资源已就绪")
        .body("现在可以开始会议录音。点击返回 Snack 会议设置页。")
        .action("open-meeting-settings", "返回会议设置")
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

fn configure_notification_identity(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = notify_rust::set_application(&app.config().identifier);
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

fn handle_notification_action(app: &AppHandle, action: &str) {
    if !should_open_meeting_settings(action) {
        return;
    }
    if let Err(error) = open_meeting_settings(app) {
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

fn should_open_meeting_settings(action: &str) -> bool {
    action != "__closed"
}

fn open_meeting_settings(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main webview is unavailable".to_string())?;
    let current_url = window.url().map_err(|error| error.to_string())?;
    if !crate::web::is_allowed_web_origin(&current_url) {
        return Err("main webview origin is not allowed".to_string());
    }

    let target_url = meeting_settings_url(current_url);
    let _ = window.show();
    let _ = window.unminimize();
    crate::window_state::recover_if_unreachable(&window);
    let _ = window.set_focus();
    window
        .navigate(target_url)
        .map_err(|error| error.to_string())
}

fn meeting_settings_url(mut url: Url) -> Url {
    url.set_path(MEETING_SETTINGS_PATH);
    url.set_query(Some(MEETING_SETTINGS_QUERY));
    url.set_fragment(None);
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_click_targets_guided_meeting_settings() {
        let url = meeting_settings_url(
            Url::parse("http://localhost:3000/apps?from=test#section").expect("valid URL"),
        );
        assert_eq!(
            url.as_str(),
            "http://localhost:3000/meeting/settings?guide=record"
        );
        assert!(should_open_meeting_settings("default"));
        assert!(should_open_meeting_settings("open-meeting-settings"));
        assert!(!should_open_meeting_settings("__closed"));
    }
}
