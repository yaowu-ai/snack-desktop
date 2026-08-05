//! Native recording overlay window.
//!
//! The overlay is a small always-on-top window served from a custom URI
//! scheme backed by embedded HTML. It does not depend on the remote web page:
//! even if the network drops or the main window is hidden, the user can still
//! end the recording from the overlay.

use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder,
};

pub(crate) const OVERLAY_LABEL: &str = "record_overlay";
pub(crate) const OVERLAY_SCHEME: &str = "snack-overlay";
const OVERLAY_STATE_EVENT: &str = "meeting-overlay-state";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OverlayPhase {
    Recording,
    Transcribing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverlayState {
    pub(crate) phase: OverlayPhase,
    pub(crate) elapsed_ms: u64,
    pub(crate) mic_active: bool,
    pub(crate) system_audio_active: bool,
    pub(crate) recording_id: String,
    pub(crate) progress_percent: u8,
    pub(crate) message: Option<String>,
}

impl OverlayState {
    pub(crate) fn recording(
        recording_id: String,
        elapsed_ms: u64,
        mic_active: bool,
        system_audio_active: bool,
    ) -> Self {
        Self {
            phase: OverlayPhase::Recording,
            elapsed_ms,
            mic_active,
            system_audio_active,
            recording_id,
            progress_percent: 0,
            message: None,
        }
    }

    pub(crate) fn transcribing(recording_id: String, progress_percent: u8) -> Self {
        Self {
            phase: OverlayPhase::Transcribing,
            elapsed_ms: 0,
            mic_active: false,
            system_audio_active: false,
            recording_id,
            progress_percent,
            message: None,
        }
    }

    pub(crate) fn ready(recording_id: String) -> Self {
        Self {
            phase: OverlayPhase::Ready,
            elapsed_ms: 0,
            mic_active: false,
            system_audio_active: false,
            recording_id,
            progress_percent: 100,
            message: None,
        }
    }

    pub(crate) fn failed(recording_id: String, message: String) -> Self {
        Self {
            phase: OverlayPhase::Failed,
            elapsed_ms: 0,
            mic_active: false,
            system_audio_active: false,
            recording_id,
            progress_percent: 0,
            message: Some(message),
        }
    }
}

/// Serve the embedded overlay HTML over the custom protocol.
pub(crate) fn serve_overlay_request(
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let path = request.uri().path();
    let body = if path == "/" || path == "/index.html" {
        OVERLAY_HTML.as_bytes().to_vec()
    } else {
        Vec::new()
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
        let _ = window.set_size(LogicalSize::new(392.0, 116.0));
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
    .inner_size(392.0, 116.0)
    .resizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .shadow(false)
    .visible(false)
    .build()
    .map_err(|error| format!("无法创建录音浮层: {error}"))?;

    if let Ok(Some(monitor)) = window.current_monitor() {
        let area = monitor.work_area();
        let window_width = window.outer_size().map(|size| size.width).unwrap_or(392) as i32;
        let x = area.position.x + area.size.width as i32 - window_width - 24;
        let y = area.position.y + 24;
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }

    update_overlay(&window, state);
    let _ = window.show();
    Ok(())
}

pub(crate) fn update_overlay(window: &tauri::WebviewWindow, state: OverlayState) {
    let _ = window.emit(OVERLAY_STATE_EVENT, state);
}

pub(crate) fn hide_overlay(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = window.hide();
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

const OVERLAY_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8" />
<title>Snack 会议录音</title>
<style>
  :root { color-scheme: light; }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  html, body { height: 100%; overflow: hidden; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "PingFang SC", "Microsoft YaHei", sans-serif;
    background: rgba(255, 255, 255, 0.98); border: 1px solid #fee5d0;
    border-radius: 14px; color: #0f172a;
    user-select: none; -webkit-user-select: none;
  }
  .bar {
    position: relative; display: flex; align-items: center; gap: 12px;
    height: 116px; padding: 22px 16px 12px; cursor: grab;
  }
  .bar:active { cursor: grabbing; }
  .window-actions { position: absolute; right: 7px; top: 5px; display: flex; gap: 2px; }
  .window-action {
    width: 22px; height: 18px; border: 0; border-radius: 5px; background: transparent;
    color: #94a3b8; cursor: pointer; font-size: 15px; line-height: 16px;
  }
  .window-action:hover { background: #fff2e8; color: #fe720a; }
  .window-action.close:hover { background: #ffedd5; color: #c2410c; }
  .status-icon {
    display: flex; width: 26px; height: 26px; flex-shrink: 0; align-items: center;
    justify-content: center; border-radius: 50%; background: #fff2e8;
    color: #fe720a; font-size: 13px; font-weight: 700;
  }
  .status-icon.recording { color: #fe720a; animation: pulse 1.2s infinite; }
  .status-icon.transcribing { border: 2px solid #fed7aa; border-top-color: #fe720a; background: #fff; animation: spin .8s linear infinite; }
  .status-icon.ready { background: #ecfdf3; color: #16a34a; }
  .status-icon.failed { background: #fef2f2; color: #dc2626; }
  @keyframes pulse { 0%,100% { opacity: 1; } 50% { opacity: .35; } }
  @keyframes spin { to { transform: rotate(360deg); } }
  .info { flex: 1; min-width: 0; }
  .title { font-size: 13px; font-weight: 650; }
  .metric { margin-top: 3px; font-size: 20px; font-weight: 700; font-variant-numeric: tabular-nums; }
  .detail { display: none; margin-top: 5px; overflow: hidden; color: #64748b; font-size: 11px; line-height: 16px; text-overflow: ellipsis; white-space: nowrap; }
  .sources { display: flex; gap: 9px; margin-top: 4px; color: #64748b; font-size: 11px; }
  .source { display: inline-flex; align-items: center; gap: 4px; }
  .src-dot { width: 7px; height: 7px; border-radius: 50%; background: #cbd5e1; }
  .src-dot.on { background: #22c55e; }
  .progress { display: none; height: 4px; margin-top: 8px; overflow: hidden; border-radius: 999px; background: #ffedd5; }
  .progress-fill { height: 100%; border-radius: inherit; background: #fe720a; transition: width .25s ease; }
  .primary-action {
    flex-shrink: 0; border: none; border-radius: 10px; padding: 9px 14px;
    background: #fe720a; color: #fff; cursor: pointer; font-size: 12px; font-weight: 650;
  }
  .primary-action:hover { background: #e96608; }
  .primary-action:disabled { cursor: default; opacity: .6; }
  .confirm-layer {
    display: none; position: fixed; inset: 0; z-index: 10; align-items: center;
    gap: 12px; padding: 16px; background: rgba(255, 255, 255, 0.995);
  }
  .confirm-layer.visible { display: flex; }
  .confirm-copy { flex: 1; min-width: 0; }
  .confirm-title { font-size: 13px; font-weight: 650; }
  .confirm-detail { margin-top: 4px; color: #64748b; font-size: 11px; }
  .confirm-actions { display: flex; gap: 8px; flex-shrink: 0; }
  .confirm-button { border: 1px solid #e2e8f0; border-radius: 8px; padding: 8px 11px; background: #f8fafc; color: #475569; cursor: pointer; font-size: 12px; font-weight: 600; }
  .confirm-button:hover { background: #f1f5f9; }
  .confirm-button.danger { border-color: #fe720a; background: #fe720a; color: #fff; }
</style>
</head>
<body data-phase="recording">
  <div class="bar" data-tauri-drag-region>
    <div class="window-actions">
      <button class="window-action" id="minimize" aria-label="最小化">−</button>
      <button class="window-action close" id="close" aria-label="关闭">×</button>
    </div>
    <div class="status-icon recording" id="status-icon">●</div>
    <div class="info">
      <div class="title" id="title">正在录音</div>
      <div class="metric" id="metric">00:00</div>
      <div class="detail" id="detail"></div>
      <div class="sources" id="sources">
        <span class="source"><span class="src-dot" id="mic"></span>麦克风</span>
        <span class="source"><span class="src-dot" id="sys"></span>系统音频</span>
      </div>
      <div class="progress" id="progress"><div class="progress-fill" id="progress-fill"></div></div>
    </div>
    <button class="primary-action" id="primary-action">结束</button>
  </div>
  <div class="confirm-layer" id="close-confirm" role="dialog" aria-modal="true" aria-labelledby="close-confirm-title">
    <div class="confirm-copy">
      <div class="confirm-title" id="close-confirm-title">确定结束录音？</div>
      <div class="confirm-detail">录音会立即停止，并继续在本地转写</div>
    </div>
    <div class="confirm-actions">
      <button class="confirm-button" id="cancel-close">取消</button>
      <button class="confirm-button danger" id="confirm-close">结束录音</button>
    </div>
  </div>
  <script>
    (function () {
      var current = { phase: 'recording', elapsedMs: 0, recordingId: '', progressPercent: 0 };
      var stopPending = false;
      var actionPending = false;
      var stopError = '';
      var iconEl = document.getElementById('status-icon');
      var titleEl = document.getElementById('title');
      var metricEl = document.getElementById('metric');
      var detailEl = document.getElementById('detail');
      var sourcesEl = document.getElementById('sources');
      var micEl = document.getElementById('mic');
      var sysEl = document.getElementById('sys');
      var progressEl = document.getElementById('progress');
      var progressFillEl = document.getElementById('progress-fill');
      var actionEl = document.getElementById('primary-action');
      var closeConfirmEl = document.getElementById('close-confirm');
      var cancelCloseEl = document.getElementById('cancel-close');

      function invoke(command, args) {
        if (window.__TAURI__ && window.__TAURI__.core) return window.__TAURI__.core.invoke(command, args || {});
        if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) return window.__TAURI_INTERNALS__.invoke(command, args || {});
        return Promise.reject(new Error('native bridge unavailable'));
      }

      function withTimeout(promise, timeoutMs) {
        return Promise.race([promise, new Promise(function (_, reject) {
          setTimeout(function () { reject(new Error('操作超时')); }, timeoutMs);
        })]);
      }

      function stopViaProtocol() {
        return withTimeout(fetch('/stop', { method: 'POST', cache: 'no-store' }), 4000).then(function (response) {
          if (response.ok) return;
          return response.text().then(function (message) { throw new Error(message || '结束录音失败'); });
        });
      }

      function formatDuration(ms) {
        var total = Math.floor(ms / 1000);
        return String(Math.floor(total / 60)).padStart(2, '0') + ':' + String(total % 60).padStart(2, '0');
      }

      function resetView() {
        metricEl.style.display = 'none';
        detailEl.style.display = 'none';
        sourcesEl.style.display = 'none';
        progressEl.style.display = 'none';
        actionEl.style.display = 'none';
        actionEl.disabled = false;
        iconEl.textContent = '';
        iconEl.className = 'status-icon ' + current.phase;
        document.body.dataset.phase = current.phase;
      }

      function renderRecording() {
        iconEl.textContent = '●';
        titleEl.textContent = '正在录音';
        metricEl.textContent = formatDuration(current.elapsedMs || 0);
        metricEl.style.display = 'block';
        sourcesEl.style.display = 'flex';
        micEl.className = 'src-dot' + (current.micActive ? ' on' : '');
        sysEl.className = 'src-dot' + (current.systemAudioActive ? ' on' : '');
        actionEl.textContent = stopPending ? '结束中…' : '结束';
        actionEl.disabled = stopPending;
        actionEl.style.display = 'block';
        if (stopError) {
          detailEl.textContent = stopError;
          detailEl.style.display = 'block';
        }
      }

      function renderTranscribing() {
        titleEl.textContent = '正在转写';
        metricEl.textContent = String(current.progressPercent || 0) + '%';
        metricEl.style.display = 'block';
        detailEl.textContent = current.message || '录音已保存，正在本地生成转写文本';
        detailEl.style.display = 'block';
        progressFillEl.style.width = String(current.progressPercent || 0) + '%';
        progressEl.style.display = 'block';
      }

      function renderReady() {
        iconEl.textContent = '✓';
        titleEl.textContent = '转写完成';
        detailEl.textContent = current.message || '录音和转写已保存，可以直接生成会议纪要';
        detailEl.style.display = 'block';
        actionEl.textContent = actionPending ? '正在打开…' : '生成纪要';
        actionEl.disabled = actionPending;
        actionEl.style.display = 'block';
      }

      function renderFailed() {
        iconEl.textContent = '!';
        titleEl.textContent = '转写失败';
        detailEl.textContent = current.message || '请在 Snack 的我的录音中查看并重试';
        detailEl.style.display = 'block';
      }

      function render() {
        resetView();
        if (current.phase === 'recording') renderRecording();
        else if (current.phase === 'transcribing') renderTranscribing();
        else if (current.phase === 'ready') renderReady();
        else renderFailed();
      }

      function apply(next) {
        if (!next) return;
        var nextPhase = next.phase || current.phase;
        var sameRecording = next.recordingId && next.recordingId === current.recordingId;
        if (current.phase !== 'recording' && nextPhase === 'recording' && sameRecording) return;
        var changedRecording = next.recordingId && next.recordingId !== current.recordingId;
        current = Object.assign({}, current, next, { phase: nextPhase });
        if (changedRecording && nextPhase === 'recording') {
          stopPending = false;
          actionPending = false;
          stopError = '';
        }
        render();
      }

      function showStopError(error) {
        stopPending = false;
        current.phase = 'recording';
        stopError = (error && error.message) || '结束失败，请重试';
        render();
      }

      function stopRecording() {
        if (stopPending || current.phase !== 'recording') return;
        stopPending = true;
        stopError = '';
        apply({ phase: 'transcribing', recordingId: current.recordingId, progressPercent: 0 });
        withTimeout(invoke('meeting_stop_recording', {}), 4000)
          .catch(function () { return stopViaProtocol(); })
          .catch(showStopError);
      }

      function openNotes() {
        if (actionPending || !current.recordingId) return;
        actionPending = true;
        render();
        withTimeout(invoke('meeting_open_notes_in_chat', { recordingId: current.recordingId }), 6000)
          .catch(function (error) {
            actionPending = false;
            current.message = (error && error.message) || '打开 Snack 失败，请重试';
            render();
          });
      }

      actionEl.addEventListener('click', function () {
        if (current.phase === 'recording') stopRecording();
        else if (current.phase === 'ready') openNotes();
      });
      document.getElementById('minimize').addEventListener('click', function () {
        invoke('minimize_overlay', {}).catch(function () {});
      });
      document.getElementById('close').addEventListener('click', function () {
        if (current.phase !== 'recording') {
          invoke('dismiss_overlay', {}).catch(function () {});
          return;
        }
        closeConfirmEl.classList.add('visible');
        cancelCloseEl.focus();
      });
      cancelCloseEl.addEventListener('click', function () { closeConfirmEl.classList.remove('visible'); });
      document.getElementById('confirm-close').addEventListener('click', function () {
        closeConfirmEl.classList.remove('visible');
        stopRecording();
      });
      document.addEventListener('keydown', function (event) {
        if (event.key === 'Escape') closeConfirmEl.classList.remove('visible');
      });

      render();
      try {
        var eventApi = window.__TAURI__ && window.__TAURI__.event;
        if (eventApi) eventApi.listen('meeting-overlay-state', function (event) { apply(event.payload); });
        invoke('meeting_get_recording_status', {}).then(apply).catch(function () {});
      } catch (error) {}
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
        assert!(html.contains("return stopViaProtocol();"));
        assert!(html.contains("meeting_open_notes_in_chat"));
        assert!(html.contains("正在转写"));
        assert!(html.contains("转写完成"));
        assert!(html.contains("生成纪要"));
        assert!(!html.contains("做会议纪要"));
        assert!(html.contains("color-scheme: light"));
        assert!(html.contains("background: rgba(255, 255, 255, 0.98)"));
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
    fn ready_state_serializes_the_recording_identity() {
        let value = serde_json::to_value(OverlayState::ready("rec-1".to_string())).unwrap();

        assert_eq!(value["phase"], "ready");
        assert_eq!(value["recordingId"], "rec-1");
        assert_eq!(value["progressPercent"], 100);
    }
}
