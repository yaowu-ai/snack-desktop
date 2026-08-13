//! Native recording overlay window.
//!
//! The overlay is a small always-on-top window served from a custom URI
//! scheme backed by embedded HTML. It does not depend on the remote web page:
//! even if the network drops or the main window is hidden, the user can still
//! end the recording from the overlay.

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
const OVERLAY_EXPANDED_HEIGHT: f64 = 228.0;
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
    pub(crate) projects: Vec<RecordingProjectOption>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingProjectOption {
    pub(crate) project_id: String,
    pub(crate) project_name: String,
}

pub(crate) struct RecordingOverlayState {
    pub(crate) recording_id: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) mic_active: bool,
    pub(crate) system_audio_active: bool,
    pub(crate) paused: bool,
    pub(crate) display_name: String,
    pub(crate) auto_generate_notes_enabled: bool,
    pub(crate) notes_project_id: Option<String>,
    pub(crate) notes_project_name: Option<String>,
    pub(crate) projects: Vec<RecordingProjectOption>,
}

impl OverlayState {
    pub(crate) fn recording(state: RecordingOverlayState) -> Self {
        Self {
            phase: OverlayPhase::Recording,
            elapsed_ms: state.elapsed_ms,
            mic_active: state.mic_active,
            system_audio_active: state.system_audio_active,
            recording_id: state.recording_id,
            paused: state.paused,
            display_name: state.display_name,
            auto_generate_notes_enabled: state.auto_generate_notes_enabled,
            notes_project_id: state.notes_project_id,
            notes_project_name: state.notes_project_name,
            projects: state.projects,
        }
    }
}

/// Serve the embedded overlay HTML over the custom protocol.
pub(crate) fn serve_overlay_request(
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let path = request.uri().path();
    let body = match path {
        "/" | "/index.html" => OVERLAY_HTML.as_bytes().to_vec(),
        "/reminder.html" => REMINDER_HTML.as_bytes().to_vec(),
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
        return serve_overlay_request(request);
    }

    if webview_label != OVERLAY_LABEL {
        return text_response(403, "仅录音浮层可以结束录音");
    }
    if request.method() != tauri::http::Method::POST {
        return text_response(405, "请使用 POST 结束录音");
    }

    match super::stop_recording_from_overlay(app.clone()) {
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
                .map_err(|_| "录音浮层地址无效".to_string())?,
        ),
    )
    .title("Snack 会议录音")
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
    .map_err(|error| format!("无法创建录音浮层: {error}"))?;

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
    let application_json = serde_json::to_string(application_name)
        .map_err(|error| format!("无法显示录音提醒: {error}"))?;
    if let Some(window) = app.get_webview_window(REMINDER_LABEL) {
        let _ = window.eval(format!(
            "window.setSnackReminderApplication && window.setSnackReminderApplication({application_json});"
        ));
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
                .map_err(|_| "录音提醒地址无效".to_string())?,
        ),
    )
    .title("Snack 录音提醒")
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
    .initialization_script(format!(
        "window.__SNACK_REMINDER_APPLICATION__ = {application_json};"
    ))
    .build()
    .map_err(|error| format!("无法创建录音提醒: {error}"))?;

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
        .ok_or_else(|| "录音浮窗不存在".to_string())?;
    window.unminimize().map_err(|error| error.to_string())?;
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())
}

/// True when the given window is the overlay (used by origin checks).
pub(crate) fn is_overlay_window(window: &tauri::WebviewWindow) -> bool {
    window.label() == OVERLAY_LABEL
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
        return Err("只有录音提醒可以执行此操作".to_string());
    }
    super::start_quick_recording(app).await?;
    let _ = window.close();
    Ok(())
}

#[tauri::command]
pub(crate) fn dismiss_recording_reminder(window: tauri::WebviewWindow) -> Result<(), String> {
    if !is_reminder_window(&window) {
        return Err("只有录音提醒可以执行此操作".to_string());
    }
    window.close().map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn minimize_overlay(window: tauri::WebviewWindow) -> Result<(), String> {
    if !is_overlay_window(&window) {
        return Err("只有录音浮窗可以执行此操作".to_string());
    }
    // A borderless macOS utility window cannot always be restored after native
    // miniaturization. Hiding preserves the expected compact behavior and lets
    // the recording card reliably bring the overlay back with `show()`.
    window.hide().map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn dismiss_overlay(window: tauri::WebviewWindow) -> Result<(), String> {
    if !is_overlay_window(&window) {
        return Err("只有录音浮窗可以执行此操作".to_string());
    }
    window.hide().map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn set_overlay_expanded(
    window: tauri::WebviewWindow,
    expanded: bool,
) -> Result<(), String> {
    if !is_overlay_window(&window) {
        return Err("只有录音浮窗可以执行此操作".to_string());
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

const REMINDER_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8" />
<title>Snack 录音提醒</title>
<style>
  :root { color-scheme: light; }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  html, body { width: 100%; height: 100%; overflow: hidden; background: transparent; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "PingFang SC", "Microsoft YaHei", sans-serif;
    color: #0f172a;
    user-select: none; -webkit-user-select: none;
  }
  .card {
    position: relative; display: flex; align-items: center; gap: 12px;
    width: 100%; height: 100%; padding: 18px; overflow: hidden;
    border: 1px solid #fee5d0; border-radius: 14px;
    background: rgba(255, 255, 255, .98);
  }
  .icon {
    display: flex; width: 38px; height: 38px; flex: 0 0 auto; align-items: center; justify-content: center;
    border-radius: 12px; background: #fff2e8; color: #fe720a; font-size: 20px;
  }
  .content { min-width: 0; flex: 1; }
  .title { font-size: 14px; font-weight: 700; line-height: 20px; }
  .description { margin-top: 3px; color: #64748b; font-size: 11px; line-height: 16px; }
  .error { display: none; margin-top: 3px; color: #dc2626; font-size: 10px; line-height: 14px; }
  .start {
    flex: 0 0 auto; border: 0; border-radius: 9px; padding: 9px 12px;
    background: #fe720a; color: white; cursor: pointer; font-size: 11px; font-weight: 650;
  }
  .start:hover { background: #e96608; }
  .start:disabled { cursor: default; opacity: .6; }
  .close {
    position: absolute; top: 6px; right: 8px; width: 22px; height: 22px;
    border: 0; background: transparent; color: #94a3b8; cursor: pointer; font-size: 17px;
  }
</style>
</head>
<body>
  <div class="card">
    <div class="icon" aria-hidden="true">●</div>
    <div class="content">
      <div class="title">会议录音提醒</div>
      <div class="description" id="description">检测到会议应用正在播放声音</div>
      <div class="error" id="error"></div>
    </div>
    <button class="start" id="start">开始录音</button>
    <button class="close" id="close" aria-label="关闭">×</button>
  </div>
  <script>
    (function () {
      var startButton = document.getElementById('start');
      var closeButton = document.getElementById('close');
      var description = document.getElementById('description');
      var error = document.getElementById('error');
      var dismissTimer;

      function invoke(command) {
        if (window.__TAURI__ && window.__TAURI__.core) return window.__TAURI__.core.invoke(command);
        if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) return window.__TAURI_INTERNALS__.invoke(command);
        return Promise.reject(new Error('native bridge unavailable'));
      }

      function resetDismissTimer() {
        clearTimeout(dismissTimer);
        dismissTimer = setTimeout(function () {
          invoke('dismiss_recording_reminder').catch(function () {});
        }, 15000);
      }

      window.setSnackReminderApplication = function (applicationName) {
        var name = applicationName || '会议应用';
        description.textContent = '检测到' + name + '正在播放声音';
        error.style.display = 'none';
        startButton.disabled = false;
        startButton.textContent = '开始录音';
        resetDismissTimer();
      };

      startButton.addEventListener('click', function () {
        if (startButton.disabled) return;
        clearTimeout(dismissTimer);
        startButton.disabled = true;
        startButton.textContent = '启动中…';
        error.style.display = 'none';
        invoke('start_recording_from_reminder').catch(function (reason) {
          startButton.disabled = false;
          startButton.textContent = '重试';
          error.textContent = (reason && reason.message) || String(reason || '启动录音失败');
          error.style.display = 'block';
          resetDismissTimer();
        });
      });
      closeButton.addEventListener('click', function () {
        clearTimeout(dismissTimer);
        invoke('dismiss_recording_reminder').catch(function () {});
      });
      window.setSnackReminderApplication(window.__SNACK_REMINDER_APPLICATION__);
    })();
  </script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_html_includes_native_protocol_stop_fallback() {
        let request = tauri::http::Request::builder()
            .uri("snack-overlay://localhost/index.html")
            .body(Vec::new())
            .unwrap();

        let response = serve_overlay_request(request);

        assert_eq!(response.status(), 200);
        let html = String::from_utf8(response.into_body()).unwrap();
        assert!(html.contains("fetch('/stop'"));
        assert!(html.contains("meeting_stop_recording"));
        assert!(html.contains("meeting_set_recording_paused"));
        assert!(html.contains("meeting_set_recording_file_name"));
        assert!(html.contains("meeting_set_recording_auto_notes"));
        assert!(!html.contains("meeting_request_recording_project"));
        assert!(!html.contains("meeting_set_recording_project"));
        assert!(html.contains("return stopViaProtocol();"));
        assert!(!html.contains("meeting_open_notes_in_chat"));
        assert!(!html.contains("正在转写"));
        assert!(!html.contains("转写完成"));
        assert!(!html.contains("生成纪要"));
        assert!(!html.contains("做会议纪要"));
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
        assert!(html.contains("aria-label=\"暂停录音\""));
        assert!(html.contains("aria-label=\"恢复录音\""));
        assert!(html.contains("id=\"pause-action\""));
        assert!(html.contains("id=\"resume-action\""));
        assert!(html.contains("aria-label=\"结束录音\""));
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
        assert!(!html.contains("id=\"project-select\""));
        assert!(!html.contains("新建项目"));
        assert!(!html.contains("stopViaProtocol(),\n          invokeWithTimeout"));
    }

    #[test]
    fn overlay_unknown_asset_returns_not_found() {
        let request = tauri::http::Request::builder()
            .uri("snack-overlay://localhost/missing")
            .body(Vec::new())
            .unwrap();

        let response = serve_overlay_request(request);

        assert_eq!(response.status(), 404);
    }

    #[test]
    fn rounded_overlay_is_inset_from_the_transparent_window_edge() {
        let request = tauri::http::Request::builder()
            .uri("snack-overlay://localhost/index.html")
            .body(Vec::new())
            .unwrap();

        let html = String::from_utf8(serve_overlay_request(request).into_body()).unwrap();

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

        let response = serve_overlay_request(request);

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
            recording_id: "rec-1".to_string(),
            elapsed_ms: 2500,
            mic_active: true,
            system_audio_active: true,
            paused: false,
            display_name: "Snack会议".to_string(),
            auto_generate_notes_enabled: true,
            notes_project_id: Some("101".to_string()),
            notes_project_name: Some("产品项目".to_string()),
            projects: vec![RecordingProjectOption {
                project_id: "101".to_string(),
                project_name: "产品项目".to_string(),
            }],
        }))
        .unwrap();

        assert_eq!(value["phase"], "recording");
        assert_eq!(value["recordingId"], "rec-1");
        assert_eq!(value["elapsedMs"], 2500);
        assert_eq!(value["displayName"], "Snack会议");
        assert_eq!(value["notesProjectId"], "101");
        assert_eq!(value["projects"][0]["projectName"], "产品项目");
    }
}
