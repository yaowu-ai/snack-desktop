//! Native recording overlay window.
//!
//! The overlay is a small always-on-top window served from a custom URI
//! scheme backed by embedded HTML. It does not depend on the remote web page:
//! even if the network drops or the main window is hidden, the user can still
//! end the recording from the overlay.

use super::overlay_locale;
use crate::i18n;
use tauri::window::Color;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder,
};

pub(crate) const OVERLAY_LABEL: &str = "record_overlay";
pub(crate) const OVERLAY_SCHEME: &str = "snack-overlay";
pub(crate) const REMINDER_LABEL: &str = "recording_reminder";
const OVERLAY_STATE_EVENT: &str = "meeting-overlay-state";
const OVERLAY_WIDTH: f64 = 240.0;
const OVERLAY_HEIGHT: f64 = 108.0;
const OVERLAY_EXPANDED_HEIGHT: f64 = 276.0;
const REMINDER_WIDTH: f64 = 326.0;
const REMINDER_HEIGHT: f64 = 116.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OverlayPhase {
    Recording,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverlayState {
    pub(crate) audio_level_db: Option<f32>,
    pub(crate) phase: OverlayPhase,
    pub(crate) elapsed_ms: u64,
    pub(crate) mic_active: bool,
    pub(crate) system_audio_active: bool,
    pub(crate) recording_id: String,
    pub(crate) paused: bool,
    pub(crate) display_name: String,
    pub(crate) auto_generate_notes_enabled: bool,
    pub(crate) notes_project_id: Option<String>,
    pub(crate) notes_project_name: Option<String>,
    pub(crate) transcript_project_id: Option<String>,
    pub(crate) transcript_project_name: Option<String>,
    pub(crate) projects: Vec<RecordingProjectOption>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingProjectOption {
    pub(crate) project_id: String,
    pub(crate) project_name: String,
}

pub(crate) struct RecordingOverlayState {
    pub(crate) audio_level_db: Option<f32>,
    pub(crate) recording_id: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) mic_active: bool,
    pub(crate) system_audio_active: bool,
    pub(crate) paused: bool,
    pub(crate) display_name: String,
    pub(crate) auto_generate_notes_enabled: bool,
    pub(crate) notes_project_id: Option<String>,
    pub(crate) notes_project_name: Option<String>,
    pub(crate) transcript_project_id: Option<String>,
    pub(crate) transcript_project_name: Option<String>,
    pub(crate) projects: Vec<RecordingProjectOption>,
}

impl OverlayState {
    pub(crate) fn recording(state: RecordingOverlayState) -> Self {
        Self {
            phase: OverlayPhase::Recording,
            audio_level_db: state.audio_level_db,
            elapsed_ms: state.elapsed_ms,
            mic_active: state.mic_active,
            system_audio_active: state.system_audio_active,
            recording_id: state.recording_id,
            paused: state.paused,
            display_name: state.display_name,
            auto_generate_notes_enabled: state.auto_generate_notes_enabled,
            notes_project_id: state.notes_project_id,
            notes_project_name: state.notes_project_name,
            transcript_project_id: state.transcript_project_id,
            transcript_project_name: state.transcript_project_name,
            projects: state.projects,
        }
    }
}

/// Serve the embedded overlay HTML over the custom protocol.
pub(crate) fn serve_overlay_request(
    request: tauri::http::Request<Vec<u8>>,
    locale: &serde_json::Value,
) -> tauri::http::Response<Vec<u8>> {
    let path = request.uri().path();
    if let Some((mime, body)) = super::note_overlay::asset(path, locale) {
        return tauri::http::Response::builder()
            .status(200)
            .header("Content-Type", mime)
            .body(body)
            .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()));
    }
    let body = match path {
        "/" | "/index.html" => overlay_locale::render(OVERLAY_HTML, locale).into_bytes(),
        "/reminder.html" => overlay_locale::render(REMINDER_HTML, locale).into_bytes(),
        "/note-notice.html" => super::note_overlay::render_notice_html(locale).into_bytes(),
        _ => Vec::new(),
    };
    let status = if body.is_empty() { 404 } else { 200 };
    tauri::http::Response::builder()
        .status(status)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(body)
        .unwrap_or_else(|_| {
            tauri::http::Response::builder()
                .status(500)
                .body(Vec::new())
                .unwrap()
        })
}

/// Handle overlay assets and the native stop fallback endpoint.
///
/// The stop button normally uses Tauri IPC. A custom-protocol endpoint keeps
/// stopping available when the injected JavaScript bridge is temporarily
/// unavailable or an IPC promise stalls.
pub(crate) fn handle_overlay_request(
    app: &AppHandle,
    webview_label: &str,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    if request.uri().path() != "/stop" {
        return serve_overlay_request(request, &overlay_locale::app_payload(app));
    }

    if webview_label != OVERLAY_LABEL && webview_label != super::note_overlay::LABEL {
        return text_response(403, &i18n::t(app, "desktop.overlay.overlayOnly"));
    }
    if request.method() != tauri::http::Method::POST {
        return text_response(405, &i18n::t(app, "desktop.overlay.postRequired"));
    }

    let result = if webview_label == super::note_overlay::LABEL {
        std::str::from_utf8(request.body()).map_err(|_| "invalid recording identifier".to_string())
            .and_then(|recording_id| super::note_overlay::stop_from_protocol(app.clone(), recording_id))
    } else {
        super::stop_recording_from_overlay(app.clone())
    };
    match result {
        Ok(()) => text_response(204, ""),
        Err(message) => text_response(409, &message),
    }
}

fn text_response(status: u16, body: &str) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(body.as_bytes().to_vec())
        .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
}

/// Show the overlay window. Idempotent — recreates the window if missing.
pub(crate) fn show_overlay(app: &AppHandle, state: OverlayState) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = window.set_size(LogicalSize::new(OVERLAY_WIDTH, OVERLAY_HEIGHT));
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        update_overlay(&window, state);
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        OVERLAY_LABEL,
        WebviewUrl::CustomProtocol(
            format!("{OVERLAY_SCHEME}://localhost/index.html")
                .parse()
                .map_err(|_| i18n::t(app, "desktop.overlay.invalidOverlayUrl"))?,
        ),
    )
    .title(i18n::t(app, "desktop.overlay.windowTitle"))
    .inner_size(OVERLAY_WIDTH, OVERLAY_HEIGHT)
    .resizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .shadow(false)
    .transparent(true)
    .background_color(Color(0, 0, 0, 0))
    .accept_first_mouse(true)
    .visible(false)
    .build()
    .map_err(|error| {
        i18n::format(
            app,
            "desktop.overlay.createOverlayFailed",
            &[("error", error.to_string())],
        )
    })?;

    if let Ok(Some(monitor)) = window.current_monitor() {
        let area = monitor.work_area();
        let window_width = window.outer_size().map(|size| size.width).unwrap_or(240) as i32;
        let x = area.position.x + area.size.width as i32 - window_width - 24;
        let y = area.position.y + 24;
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }

    update_overlay(&window, state);
    let _ = window.show();
    Ok(())
}

pub(crate) fn update_overlay(window: &tauri::WebviewWindow, state: OverlayState) {
    keep_recording_overlay_visible(window);
    let _ = window.emit(OVERLAY_STATE_EVENT, state);
}

/// Reassert the recording card after notifications or focused windows change
/// the native stacking order. `show` does not steal keyboard focus here.
fn keep_recording_overlay_visible(window: &tauri::WebviewWindow) {
    let _ = window.set_always_on_top(true);
    if matches!(window.is_visible(), Ok(false)) {
        let _ = window.show();
    }
}

pub(crate) fn hide_overlay(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = window.hide();
    }
}

/// Show a Snack-owned recording reminder without stealing focus from the
/// meeting application. The window and its assets are completely local.
pub(crate) fn show_recording_reminder(
    app: &AppHandle,
    application_name: &str,
) -> Result<(), String> {
    overlay_locale::set_reminder_application(application_name);
    overlay_locale::refresh(app);
    if let Some(window) = app.get_webview_window(REMINDER_LABEL) {
        let _ = window
            .eval("window.setSnackReminderApplication && window.setSnackReminderApplication();");
        let _ = window.unminimize();
        window.show().map_err(|error| error.to_string())?;
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        REMINDER_LABEL,
        WebviewUrl::CustomProtocol(
            format!("{OVERLAY_SCHEME}://localhost/reminder.html")
                .parse()
                .map_err(|_| i18n::t(app, "desktop.overlay.invalidReminderUrl"))?,
        ),
    )
    .title(i18n::t(app, "desktop.overlay.reminderWindowTitle"))
    .inner_size(REMINDER_WIDTH, REMINDER_HEIGHT)
    .resizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .shadow(true)
    .transparent(true)
    .background_color(Color(0, 0, 0, 0))
    .accept_first_mouse(true)
    .focused(false)
    .visible(false)
    .build()
    .map_err(|error| {
        i18n::format(
            app,
            "desktop.overlay.createReminderFailed",
            &[("error", error.to_string())],
        )
    })?;

    if let Ok(Some(monitor)) = window.current_monitor() {
        let area = monitor.work_area();
        let window_width = window.outer_size().map(|size| size.width).unwrap_or(326) as i32;
        let x = area.position.x + area.size.width as i32 - window_width - 24;
        let y = area.position.y + 24;
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }

    window.show().map_err(|error| error.to_string())
}

pub(crate) fn hide_recording_reminder(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(REMINDER_LABEL) {
        let _ = window.close();
    }
}

/// Bring an existing recording overlay back after the user minimized it.
pub(crate) fn restore_overlay(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window(OVERLAY_LABEL)
        .ok_or_else(|| i18n::t(app, "desktop.overlay.missingOverlay"))?;
    window.unminimize().map_err(|error| error.to_string())?;
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())
}

/// True when the given window is the overlay (used by origin checks).
pub(crate) fn is_overlay_window(window: &tauri::WebviewWindow) -> bool {
    window.label() == OVERLAY_LABEL || window.label() == super::note_overlay::LABEL
}

fn is_reminder_window(window: &tauri::WebviewWindow) -> bool {
    window.label() == REMINDER_LABEL
}

#[tauri::command]
pub(crate) async fn start_recording_from_reminder(
    app: AppHandle,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    if !is_reminder_window(&window) {
        return Err(i18n::t(window.app_handle(), "desktop.overlay.reminderOnly"));
    }
    super::start_quick_recording(app).await?;
    let _ = window.close();
    Ok(())
}

#[tauri::command]
pub(crate) fn dismiss_recording_reminder(window: tauri::WebviewWindow) -> Result<(), String> {
    if !is_reminder_window(&window) {
        return Err(i18n::t(window.app_handle(), "desktop.overlay.reminderOnly"));
    }
    window.close().map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn minimize_overlay(window: tauri::WebviewWindow) -> Result<(), String> {
    if !is_overlay_window(&window) {
        return Err(i18n::t(window.app_handle(), "desktop.overlay.overlayOnly"));
    }
    // A borderless macOS utility window cannot always be restored after native
    // miniaturization. Hiding preserves the expected compact behavior and lets
    // the recording card reliably bring the overlay back with `show()`.
    window.hide().map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn dismiss_overlay(window: tauri::WebviewWindow) -> Result<(), String> {
    if !is_overlay_window(&window) {
        return Err(i18n::t(window.app_handle(), "desktop.overlay.overlayOnly"));
    }
    window.hide().map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn set_overlay_expanded(
    window: tauri::WebviewWindow,
    expanded: bool,
) -> Result<(), String> {
    if !is_overlay_window(&window) {
        return Err(i18n::t(window.app_handle(), "desktop.overlay.overlayOnly"));
    }
    let height = if expanded {
        OVERLAY_EXPANDED_HEIGHT
    } else {
        OVERLAY_HEIGHT
    };
    window
        .set_size(LogicalSize::new(OVERLAY_WIDTH, height))
        .map_err(|error| error.to_string())
}

const OVERLAY_HTML: &str = include_str!("overlay.html");

const REMINDER_HTML: &str = include_str!("reminder.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_notice_route_is_localized_and_contains_no_recording_controls() {
        let locale = overlay_locale::payload(i18n::Locale::Zh, i18n::Locale::Zh);
        let message = locale["messages"]["backgroundTranscription"]
            .as_str()
            .unwrap();
        assert!(!message.trim().is_empty());
        let request = tauri::http::Request::builder()
            .uri("snack-overlay://localhost/note-notice.html")
            .body(Vec::new())
            .unwrap();
        let response = serve_overlay_request(request, &locale);
        assert_eq!(response.status(), 200);
        let html = String::from_utf8(response.into_body()).unwrap();
        assert!(html.contains(&overlay_locale::serialize(&locale)));
        assert!(html.contains("window.snackText('backgroundTranscription')"));
        assert!(html.contains("meeting_get_note_overlay_state"));
        for control in ["meeting_note_overlay_control", "meeting_note_overlay_action", "fetch('/stop'"] {
            assert!(!html.contains(control), "notice must remain passive: {control}");
        }
    }

    #[test]
    fn overlay_html_includes_native_protocol_stop_fallback() {
        let request = tauri::http::Request::builder()
            .uri("snack-overlay://localhost/index.html")
            .body(Vec::new())
            .unwrap();

        let response = serve_overlay_request(
            request,
            &overlay_locale::payload(i18n::Locale::Zh, i18n::Locale::Zh),
        );

        assert_eq!(response.status(), 200);
        let html = String::from_utf8(response.into_body()).unwrap();
        assert!(html.contains("fetch('/stop'"));
        assert!(html.contains("meeting_stop_recording"));
        assert!(html.contains("meeting_set_recording_paused"));
        assert!(html.contains("meeting_set_transcript_file_title"));
        assert!(html.contains("meeting_set_recording_auto_notes"));
        assert!(html.contains("meeting_set_transcript_project"));
        assert!(html.contains("errorMessage = fallback;"));
        assert!(!html.contains("errorMessage = (error && error.message)"));
        assert!(!html.contains("meeting_request_recording_project"));
        assert!(!html.contains("meeting_set_recording_project"));
        assert!(html.contains("return stopViaProtocol();"));
        assert!(!html.contains("meeting_open_notes_in_chat"));
        // Shared locale payloads also contain note-capsule messages; only the legacy UI stays recording-only.
        assert!(!OVERLAY_HTML.contains("meeting_note_overlay_action"));
        assert!(!OVERLAY_HTML.contains("transcriptionCompleted"));
        assert!(!OVERLAY_HTML.contains("retryTranscription"));
        assert!(!OVERLAY_HTML.contains("current.phase"));
        assert!(html.contains("color-scheme: light"));
        assert!(html.contains(
            "html, body { width: 100%; height: 100%; overflow: hidden; background: transparent; }"
        ));
        assert!(html.contains("grid-template-columns: 38px minmax(0, 1fr) 38px"));
        assert_eq!(OVERLAY_WIDTH, 240.0);
        assert!(html.contains("border: 1px solid #fee5d0; border-radius: 12px"));
        assert!(html.contains("background: rgba(255, 255, 255, .98)"));
        assert!(!html.contains("body {\n    font-family: -apple-system, BlinkMacSystemFont, \"PingFang SC\", \"Microsoft YaHei\", sans-serif;\n    background:"));
        assert!(!html.contains("focus_recording_overlay"));
        assert!(!html.contains("class=\"bar\" data-tauri-drag-region"));
        assert!(html.contains("class=\"drag-region\" data-tauri-drag-region"));
        assert!(html.contains("event.stopPropagation();"));
        assert!(html.contains("拖动可移动"));
        assert!(!html.contains("aria-label=\"最小化\""));
        assert!(!html.contains("aria-label=\"关闭\""));
        assert!(!html.contains(">麦克风<"));
        assert!(!html.contains(">系统音频<"));
        assert!(html.contains("data-i18n-label=\"pause\""));
        assert!(html.contains("data-i18n-label=\"resume\""));
        assert!(html.contains("id=\"pause-action\""));
        assert!(html.contains("id=\"resume-action\""));
        assert!(html.contains("data-i18n-label=\"stop\""));
        assert!(html.contains("转写文件标题"));
        assert!(html.contains("会议自动总结"));
        assert!(!html.contains("会议归属项目"));
        assert!(!html.contains(">修改转写文件标题<"));
        assert!(!html.contains(">会议纪要自动转写<"));
        assert!(!html.contains(">会议纪要归属项目<"));
        assert!(!html.contains(">修改文件名<"));
        assert!(!html.contains(">纪要自动转写<"));
        assert!(!html.contains(">纪要归属项目<"));
        assert!(!html.contains("普通会话"));
        assert!(html.contains("转写保存到项目"));
        assert!(html.contains("id=\"project-select\""));
        assert!(html.contains("new Option(t('unsaved'), '')"));
        assert!(html.contains("text-align-last: right"));
        assert!(!html.contains("新建项目"));
        assert!(!html.contains("stopViaProtocol(),\n          invokeWithTimeout"));
    }

    #[test]
    fn project_asset_commands_are_in_meeting_permission_set() {
        let permissions = include_str!("../../permissions/allow-meeting.json");
        for command in [
            "meeting_set_transcript_project",
            "meeting_claim_transcript_asset_upload",
            "meeting_mark_transcript_asset_file_uploaded",
            "meeting_complete_transcript_asset_upload",
            "meeting_fail_transcript_asset_upload",
        ] {
            assert!(
                permissions.contains(command),
                "missing permission for {command}"
            );
        }
    }

    #[test]
    fn overlay_unknown_asset_returns_not_found() {
        let request = tauri::http::Request::builder()
            .uri("snack-overlay://localhost/missing")
            .body(Vec::new())
            .unwrap();

        let response = serve_overlay_request(
            request,
            &overlay_locale::payload(i18n::Locale::Zh, i18n::Locale::Zh),
        );

        assert_eq!(response.status(), 404);
    }

    #[test]
    fn rounded_overlay_is_inset_from_the_transparent_window_edge() {
        let request = tauri::http::Request::builder()
            .uri("snack-overlay://localhost/index.html")
            .body(Vec::new())
            .unwrap();

        let html = String::from_utf8(
            serve_overlay_request(
                request,
                &overlay_locale::payload(i18n::Locale::Zh, i18n::Locale::Zh),
            )
            .into_body(),
        )
        .unwrap();

        assert!(html.contains("padding: 1px; color: #0f172a"));
        assert!(html.contains("background: transparent;"));
        assert!(html.contains("background-clip: padding-box;"));
        assert!(html.contains("border-radius: 12px;"));
    }

    #[test]
    fn reminder_html_is_local_and_auto_dismisses() {
        let request = tauri::http::Request::builder()
            .uri("snack-overlay://localhost/reminder.html")
            .body(Vec::new())
            .unwrap();

        let response = serve_overlay_request(
            request,
            &overlay_locale::payload(i18n::Locale::Zh, i18n::Locale::Zh),
        );

        assert_eq!(response.status(), 200);
        let html = String::from_utf8(response.into_body()).unwrap();
        assert!(html.contains("start_recording_from_reminder"));
        assert!(html.contains("dismiss_recording_reminder"));
        assert!(html.contains("15000"));
        assert!(html.contains("开始录音"));
        assert!(html.contains("width: 100%; height: 100%; padding: 18px; overflow: hidden"));
        assert!(html.contains("border: 1px solid #fee5d0; border-radius: 14px"));
        assert!(!html.contains("Snack Record"));
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
    }

    #[test]
    fn recording_state_serializes_the_recording_identity() {
        let value = serde_json::to_value(OverlayState::recording(RecordingOverlayState {
            audio_level_db: Some(-24.0),
            recording_id: "rec-1".to_string(),
            elapsed_ms: 2500,
            mic_active: true,
            system_audio_active: true,
            paused: false,
            display_name: "Snack会议".to_string(),
            auto_generate_notes_enabled: true,
            notes_project_id: Some("101".to_string()),
            notes_project_name: Some("产品项目".to_string()),
            transcript_project_id: Some("202".to_string()),
            transcript_project_name: Some("桌面迭代".to_string()),
            projects: vec![RecordingProjectOption {
                project_id: "101".to_string(),
                project_name: "产品项目".to_string(),
            }],
        }))
        .unwrap();

        assert_eq!(value["phase"], "recording");
        assert_eq!(value["audioLevelDb"], -24.0);
        assert_eq!(value["recordingId"], "rec-1");
        assert_eq!(value["elapsedMs"], 2500);
        assert_eq!(value["displayName"], "Snack会议");
        assert_eq!(value["notesProjectId"], "101");
        assert_eq!(value["transcriptProjectId"], "202");
        assert_eq!(value["projects"][0]["projectName"], "产品项目");
    }
}
