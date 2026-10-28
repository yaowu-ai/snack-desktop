//! Native presentation for owner-verified standard notes. Web owns all note business logic.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::window::Color;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

use super::{
    capture::LiveCaptureStatus,
    note_overlay_lifecycle::{Activity, Lifecycle},
    state::{MeetingTask, TaskState},
    MeetingManagerState,
};

pub(crate) const LABEL: &str = "note_record_overlay";
const STATE_EVENT: &str = "meeting-note-overlay-state";
const ACTION_EVENT: &str = "meeting-note-overlay-action";
const REFRESH_EVENT: &str = "meeting-note-overlay-refresh";
// Transparent margins preserve the Figma card's shadow without changing its 197 × 72 geometry.
const WINDOW_WIDTH: f64 = 263.0;
const WINDOW_HEIGHT: f64 = 148.0;
static ACTION_COUNTER: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
static BACKDROP_REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Recording,
    Ended,
    Transcribing,
    TranscriptionPaused,
    Completed,
    Failed,
    RecordingFailed,
    SummaryGenerating,
    SummaryCompleted,
    SummaryFailed,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SyncRequest {
    scope_key: String,
    recording_id: Option<String>,
    phase: Phase,
    note_visible: bool,
    #[serde(default)]
    summary_source_version: Option<String>,
    /// API v3: Web confirms that saving and all automatic follow-up work have completed.
    #[serde(default)]
    completion_ready: bool,
    /// Stable across refreshes and phase changes; a deliberate retry creates a new attempt.
    #[serde(default)]
    result_attempt: Option<String>,
    #[serde(default)]
    acknowledged_action_id: Option<String>,
}

#[derive(Debug, Clone)]
struct Binding {
    scope_key: String,
    recording_id: String,
    origin: String,
    phase: Phase,
    note_visible: bool,
    native_task_updated_at: String,
    summary_source_version: Option<String>,
    completion_ready: bool,
    result_attempt: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Action {
    id: String,
    scope_key: String,
    recording_id: String,
    origin: String,
    action: ActionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionKind {
    Open,
    Retry,
    RetrySummary,
    Dismiss,
}

#[derive(Debug, Default)]
struct RuntimeData {
    binding: Option<Binding>,
    pending_action: Option<Action>,
}

#[derive(Default)]
struct Runtime {
    data: Mutex<RuntimeData>,
    lifecycles: Mutex<LifecycleLedger>,
    watching: AtomicBool,
    refresh_pending: AtomicBool,
    reset_position_pending: AtomicBool,
    window_creating: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleKey {
    origin: String,
    scope_key: String,
    recording_id: String,
    result_attempt: String,
}

impl Binding {
    fn lifecycle_key(&self) -> LifecycleKey {
        LifecycleKey {
            origin: self.origin.clone(),
            scope_key: self.scope_key.clone(),
            recording_id: self.recording_id.clone(),
            result_attempt: self.result_attempt.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedLifecycle {
    key: LifecycleKey,
    processing_dismissed: bool,
    terminal_closed: bool,
    success_presented_at_ms: Option<u64>,
}

impl SavedLifecycle {
    fn state(&self) -> Lifecycle {
        Lifecycle {
            processing_dismissed: self.processing_dismissed,
            terminal_closed: self.terminal_closed,
            success_presented_at_ms: self.success_presented_at_ms,
        }
    }

    fn new(key: LifecycleKey, state: Lifecycle) -> Self {
        Self {
            key,
            processing_dismissed: state.processing_dismissed,
            terminal_closed: state.terminal_closed,
            success_presented_at_ms: state.success_presented_at_ms,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LifecycleLedger {
    #[serde(default)]
    entries: Vec<SavedLifecycle>,
}

const LIFECYCLE_FILE: &str = "note-overlay-lifecycle.json";

/// Serialize only meaningful transitions, under one lock, using the meeting store's atomic writer.
/// Binding cleanup deliberately does not clear the ledger: a page reload/login cannot resurrect it.
fn update_lifecycle<T>(
    app: &AppHandle,
    binding: &Binding,
    update: impl FnOnce(&mut Lifecycle) -> T,
) -> Result<T, String> {
    let runtime = app.state::<Runtime>();
    let mut ledger = runtime
        .lifecycles
        .lock()
        .map_err(|_| "recording overlay lifecycle lock unavailable")?;
    let key = binding.lifecycle_key();
    let index = ledger.entries.iter().position(|entry| entry.key == key);
    let before = index.map_or_else(Lifecycle::default, |index| ledger.entries[index].state());
    let mut after = before;
    let result = update(&mut after);
    if after != before {
        let mut next = ledger.clone();
        let entry = SavedLifecycle::new(key, after);
        match index {
            Some(index) => next.entries[index] = entry,
            None => next.entries.push(entry),
        }
        super::state::persist_json_atomic(
            &app.state::<MeetingManagerState>()
                .store
                .root()
                .join(LIFECYCLE_FILE),
            &next,
        )?;
        *ledger = next;
    }
    Ok(result)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_action: Option<Action>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DisplayState {
    recording_id: String,
    result_attempt: String,
    phase: Phase,
    elapsed_ms: u64,
    paused: bool,
    can_dismiss: bool,
}

pub(crate) fn initialize(app: &AppHandle) {
    let lifecycles = std::fs::read(
        app.state::<MeetingManagerState>()
            .store
            .root()
            .join(LIFECYCLE_FILE),
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    .unwrap_or_default();
    app.manage(Runtime {
        lifecycles: Mutex::new(lifecycles),
        ..Runtime::default()
    });
}

fn validate(request: &SyncRequest) -> Result<(), String> {
    if request.scope_key.trim().is_empty() || request.scope_key.len() > 2048 {
        return Err("invalid recording overlay scope".into());
    }
    if request.recording_id.as_ref().is_some_and(|id| {
        id.is_empty()
            || id.len() > 200
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }) {
        return Err("invalid recording identifier".into());
    }
    if request
        .result_attempt
        .as_ref()
        .is_some_and(|attempt| attempt.trim().is_empty() || attempt.len() > 200)
    {
        return Err("invalid recording result attempt".into());
    }
    Ok(())
}

/// Called only through the bridge kernel's authenticated main-webview entry.
pub(crate) fn sync(
    app: &AppHandle,
    main: &WebviewWindow,
    request: SyncRequest,
) -> Result<SyncResponse, String> {
    validate(&request)?;
    if main.label() != "main" {
        return Err("recording overlay requires main webview".into());
    }
    let url = main.url().map_err(|error| error.to_string())?;
    if !crate::web::is_allowed_web_origin(&url) {
        return Err("recording overlay origin is not allowed".into());
    }
    let origin = url.origin().ascii_serialization();
    let runtime = app.state::<Runtime>();
    let mut data = runtime
        .data
        .lock()
        .map_err(|_| "recording overlay lock unavailable")?;
    if let Some(id) = request.acknowledged_action_id.as_deref() {
        // An acknowledgement may carry the pre-navigation visibility snapshot. Never let it
        // rebind the overlay or overwrite a newer phase/route; acknowledge the matching owner only.
        if data.binding.as_ref().is_some_and(|binding| {
            binding.origin == origin
                && binding.scope_key == request.scope_key
                && request.recording_id.as_deref() == Some(binding.recording_id.as_str())
        }) {
            crate::logging::write_app_log(
                app,
                "info",
                "note-overlay",
                "action acknowledged",
                Some(&serde_json::json!({ "actionId": id })),
            );
            acknowledge_action(&mut data, Some(id));
            return Ok(SyncResponse {
                pending_action: data.pending_action.clone(),
            });
        }
        return Ok(SyncResponse {
            pending_action: None,
        });
    }
    let Some(recording_id) = request.recording_id else {
        // Web serializes scope cleanup and updates; a fresh login must clear a previous owner's capsule.
        if data
            .binding
            .as_ref()
            .is_some_and(|binding| binding.origin == origin)
        {
            *data = RuntimeData::default();
            drop(data);
            refresh(app, false)?;
        }
        return Ok(SyncResponse {
            pending_action: None,
        });
    };
    let manager = app.state::<MeetingManagerState>();
    let active = active_recording_id(&manager);
    if active.as_ref().is_some_and(|id| id != &recording_id) {
        return Err("a newer recording is active".into());
    }
    let task = manager
        .store
        .load_task()
        .filter(|task| task.recording_id == recording_id)
        .or_else(|| manager.store.load_task_record(&recording_id))
        .filter(|task| task.defer_transcription)
        .ok_or("standard-note recording is unavailable")?;
    let changed = !data.binding.as_ref().is_some_and(|binding| {
        binding.scope_key == request.scope_key
            && binding.recording_id == recording_id
            && binding.origin == origin
    });
    if changed
        || data.binding.as_ref().is_some_and(|binding| {
            binding.result_attempt != request.result_attempt.as_deref().unwrap_or("capture")
        })
    {
        data.pending_action = None;
    }
    // Visibility/focus syncs must not erase evidence of native work that progressed while
    // Web was suspended. Reset this marker only for a new binding or a new requested phase.
    let native_task_updated_at = data
        .binding
        .as_ref()
        .filter(|binding| !changed && binding.phase == request.phase)
        .map_or_else(
            || task.updated_at.clone(),
            |binding| binding.native_task_updated_at.clone(),
        );
    data.binding = Some(Binding {
        scope_key: request.scope_key,
        recording_id,
        origin,
        phase: request.phase,
        note_visible: request.note_visible,
        native_task_updated_at,
        summary_source_version: request.summary_source_version,
        completion_ready: request.completion_ready,
        result_attempt: request.result_attempt.unwrap_or_else(|| "capture".into()),
    });
    // Keep the action until Web has resolved the owned note and accepted the action.
    // A sync response may race listener registration, a reload, or a suspended page.
    let pending_action = data.pending_action.clone();
    drop(data);
    // A standard-note binding never displays the legacy recording/settings card.
    super::overlay::hide_overlay(app);
    refresh(app, changed)?;
    ensure_watcher(app);
    Ok(SyncResponse { pending_action })
}

fn binding(app: &AppHandle) -> Option<Binding> {
    app.state::<Runtime>().data.lock().ok()?.binding.clone()
}

fn acknowledge_action(data: &mut RuntimeData, id: Option<&str>) {
    if data
        .pending_action
        .as_ref()
        .is_some_and(|action| Some(action.id.as_str()) == id)
    {
        data.pending_action = None;
    }
}

fn main_matches(main: &WebviewWindow, binding: &Binding) -> bool {
    main.url()
        .is_ok_and(|url| url.origin().ascii_serialization() == binding.origin)
}

fn active_recording_id(manager: &MeetingManagerState) -> Option<String> {
    manager
        .store
        .load_task()
        .filter(|task| {
            task.state == TaskState::Recording
                && manager
                    .recorder
                    .lock()
                    .is_ok_and(|recorder| recorder.is_some())
        })
        .map(|task| task.recording_id)
}

fn should_show(note_visible: bool, main_visible: bool, main_minimized: bool) -> bool {
    !note_visible || !main_visible || main_minimized
}

fn display_state(app: &AppHandle, binding: &Binding) -> DisplayState {
    let manager = app.state::<MeetingManagerState>();
    let task = manager
        .store
        .load_task()
        .filter(|task| task.recording_id == binding.recording_id)
        .or_else(|| manager.store.load_task_record(&binding.recording_id));
    let status = task
        .as_ref()
        .filter(|task| task.state == TaskState::Recording)
        .and_then(|_| {
            manager.recorder.lock().ok().and_then(|guard| {
                guard
                    .as_ref()
                    .map(|recorder| LiveCaptureStatus::from_shared(&recorder.shared))
            })
        });
    let phase = resolve_phase(
        binding.phase,
        task.as_ref(),
        status.is_some(),
        task.as_ref()
            .is_some_and(|task| task.updated_at != binding.native_task_updated_at),
        task.as_ref()
            .and_then(|task| task.transcript.as_ref())
            .is_some_and(|transcript| {
                Some(&transcript.generated_at) == binding.summary_source_version.as_ref()
            }),
    );
    DisplayState {
        recording_id: binding.recording_id.clone(),
        result_attempt: binding.result_attempt.clone(),
        phase,
        elapsed_ms: status.as_ref().map_or(0, |status| status.elapsed_ms),
        paused: status.as_ref().is_some_and(|status| status.paused),
        can_dismiss: phase != Phase::Recording,
    }
}

fn lifecycle_activity(binding: &Binding, state: &DisplayState) -> Activity {
    match state.phase {
        Phase::Recording => Activity::Recording,
        Phase::Failed | Phase::RecordingFailed | Phase::SummaryFailed => Activity::Failure,
        Phase::Ended | Phase::Completed | Phase::SummaryCompleted
            if binding.completion_ready && binding.phase == state.phase =>
        {
            Activity::Success
        }
        // Native transcription may finish before Web has queued the automatic summary or saved
        // the note. Only a current API-v3 Web confirmation can start a success countdown.
        _ => Activity::Processing,
    }
}

/// Native capture/transcription remains authoritative while a background WebView is throttled.
/// Web owns the decision to enqueue automatic transcription, including the short-recording rule.
fn resolve_phase(
    bound: Phase,
    task: Option<&MeetingTask>,
    capturing: bool,
    progressed: bool,
    same_transcript: bool,
) -> Phase {
    if capturing {
        return Phase::Recording;
    }
    let state = task.map(|task| task.state);
    // Web may have just enqueued a retry while the native task still contains its old failure
    // or transcript. Preserve that new request until native work progresses or Web resolves it.
    if bound == Phase::Transcribing
        && !progressed
        && matches!(
            state,
            Some(
                TaskState::TranscriptionFailed
                    | TaskState::TranscriptReady
                    | TaskState::Ready
                    | TaskState::NotesFailed
            )
        )
    {
        return Phase::Transcribing;
    }
    match state {
        Some(TaskState::Finalizing | TaskState::TranscribingLocal) => Phase::Transcribing,
        Some(TaskState::TranscriptionPaused) => Phase::TranscriptionPaused,
        Some(
            TaskState::TranscriptReady
            | TaskState::WaitingForNetwork
            | TaskState::GeneratingNotes
            | TaskState::Ready
            | TaskState::NotesFailed,
        ) => {
            if task
                .and_then(|task| task.transcript.as_ref())
                .is_some_and(|transcript| {
                    !transcript.text.trim().is_empty()
                        || transcript
                            .segments
                            .iter()
                            .any(|segment| !segment.text.trim().is_empty())
                })
            {
                // Only Web knows whether a standard-note summary was submitted or completed.
                // Keep its state while it still describes this exact transcript version.
                if is_summary_phase(bound) && same_transcript {
                    bound
                } else {
                    Phase::Completed
                }
            } else {
                Phase::Failed
            }
        }
        Some(TaskState::TranscriptionFailed) => Phase::Failed,
        Some(
            TaskState::FinalizeFailed
            | TaskState::CaptureFailed
            | TaskState::NoAudioDetected
            | TaskState::PermissionDenied,
        ) => Phase::RecordingFailed,
        Some(TaskState::AwaitingTranscription) if is_summary_phase(bound) => Phase::Ended,
        // stop_recording takes the recorder before publishing Finalizing. Never turn that gap
        // into a completed/ended card; wait for the native task or Web's explicit ended decision.
        _ if bound == Phase::Recording => Phase::Transcribing,
        _ => bound,
    }
}

fn is_summary_phase(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::SummaryGenerating | Phase::SummaryCompleted | Phase::SummaryFailed
    )
}

/// Wake only the bound summary observer; native code never performs business API requests.
fn request_summary_refresh(app: &AppHandle) {
    let Some(binding) = binding(app) else {
        return;
    };
    let Some(main) = app.get_webview_window("main") else {
        return;
    };
    if !main_matches(&main, &binding)
        || display_state(app, &binding).phase != Phase::SummaryGenerating
    {
        return;
    }
    let _ = main.emit(
        REFRESH_EVENT,
        serde_json::json!({
            "scopeKey": binding.scope_key, "recordingId": binding.recording_id,
        }),
    );
}

fn ensure_watcher(app: &AppHandle) {
    let runtime = app.state::<Runtime>();
    if runtime.watching.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut summary_refresh = std::time::Instant::now();
        loop {
            // Window visibility is observed natively even when the minimized Web page throttles timers.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if binding(&app).is_none() {
                let runtime = app.state::<Runtime>();
                runtime.watching.store(false, Ordering::SeqCst);
                // Avoid losing a sync that arrived between observing None and releasing the watcher.
                if binding(&app).is_some() {
                    ensure_watcher(&app);
                }
                return;
            }
            if let Err(message) = refresh(&app, false) {
                crate::logging::write_app_log(&app, "warn", "note-overlay", &message, None);
            }
            if summary_refresh.elapsed() >= std::time::Duration::from_secs(5) {
                request_summary_refresh(&app);
                summary_refresh = std::time::Instant::now();
            }
        }
    });
}

fn refresh(app: &AppHandle, reset_position: bool) -> Result<(), String> {
    // Tauri window getters synchronously await the UI thread when called from a watcher.
    // Never hold Runtime.data on that background thread while waiting for UI: overlay commands
    // also take the lock on the UI thread. Queue the whole operation and read the latest binding
    // there, coalescing ticks while the UI is busy.
    let runtime = app.state::<Runtime>();
    if reset_position {
        runtime.reset_position_pending.store(true, Ordering::SeqCst);
    }
    if runtime.refresh_pending.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let handle = app.clone();
    let scheduled = app.run_on_main_thread(move || {
        let runtime = handle.state::<Runtime>();
        // Once this job starts, allow one successor to be queued. In particular, a concurrent
        // binding=None cleanup must not be swallowed after this job read the old binding:
        // its watcher stops as soon as it sees None and would never hide that old window.
        runtime.refresh_pending.store(false, Ordering::SeqCst);
        let reset_position = runtime.reset_position_pending.swap(false, Ordering::SeqCst);
        if let Err(message) = refresh_on_main_thread(&handle, reset_position) {
            crate::logging::write_app_log(&handle, "warn", "note-overlay", &message, None);
        }
    });
    if scheduled.is_err() {
        runtime.refresh_pending.store(false, Ordering::SeqCst);
    }
    scheduled.map_err(|error| error.to_string())
}

fn refresh_on_main_thread(app: &AppHandle, reset_position: bool) -> Result<(), String> {
    // On the UI thread, getters run inline. Keep this binding stable through the final window
    // operation so a previous recording's tick cannot hide or acknowledge a newer recording.
    let runtime = app.state::<Runtime>();
    let mut data = runtime
        .data
        .lock()
        .map_err(|_| "recording overlay lock unavailable")?;
    let Some(binding) = data.binding.clone() else {
        hide(app);
        return Ok(());
    };
    let Some(main) = app.get_webview_window("main") else {
        hide(app);
        return Ok(());
    };
    if !main_matches(&main, &binding) {
        hide(app);
        return Ok(());
    }
    if main
        .url()
        .is_ok_and(|url| url.path() == "/login" || url.path().starts_with("/login/"))
    {
        *data = RuntimeData::default();
        hide(app);
        return Ok(());
    }
    if active_recording_id(&app.state::<MeetingManagerState>())
        .is_some_and(|id| id != binding.recording_id)
    {
        hide(app);
        return Ok(());
    }
    let note_visible = !should_show(
        binding.note_visible,
        main.is_visible().unwrap_or(false),
        main.is_minimized().unwrap_or(false),
    );
    let state = display_state(app, &binding);
    let activity = lifecycle_activity(&binding, &state);
    // A route acknowledgement alone is insufficient. Web must have rendered this phase, and
    // the current note must actually be in the visible, focused main window.
    let result_viewed =
        note_visible && main.is_focused().unwrap_or(false) && binding.phase == state.phase;
    let now = super::state::unix_millis();
    let show = update_lifecycle(app, &binding, |lifecycle| {
        lifecycle.should_show(activity, note_visible, result_viewed, now)
    })?;
    if !show {
        hide(app);
        return Ok(());
    }
    let window = app.get_webview_window(LABEL);
    if runtime.window_creating.load(Ordering::SeqCst) || window.is_none() {
        // Windows/WebView2 cannot build a webview from a synchronous UI/event callback.
        // Release the binding lock before the background builder waits for that UI thread.
        drop(data);
        ensure_window_in_background(app);
        return Ok(());
    }
    let window = window.expect("overlay window checked above");
    update_window_presentation(app, &window, reset_position)?;
    let emitted = window.emit(STATE_EVENT, state).is_ok();
    if !window.is_visible().unwrap_or(false) {
        window.show().map_err(|error| error.to_string())?;
    }
    // A failed native show/emit must not consume the five-second result opportunity.
    if emitted && window.is_visible().unwrap_or(false) {
        update_lifecycle(app, &binding, |lifecycle| {
            lifecycle.presented(activity, now)
        })?;
    }
    Ok(())
}

fn ensure_window_in_background(app: &AppHandle) {
    if app
        .state::<Runtime>()
        .window_creating
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // No Runtime.data lock is held while build/getters await the UI thread. If logout or
        // cleanup won the race, avoid creating a window that is no longer needed.
        let created = if binding(&handle).is_some() {
            create_hidden_window(&handle)
        } else {
            Ok(())
        };
        handle
            .state::<Runtime>()
            .window_creating
            .store(false, Ordering::SeqCst);
        match created {
            Ok(()) => {
                // Re-read the current owner/recording on UI; never show the binding that asked
                // for creation. A newly created window still needs its initial positioning even
                // if an earlier refresh already consumed reset_position_pending.
                if let Err(message) = refresh(&handle, true) {
                    crate::logging::write_app_log(&handle, "warn", "note-overlay", &message, None);
                }
            }
            Err(message) => {
                // Reset singleflight on failure, but let the 500 ms watcher retry rather than
                // immediately queueing an unbounded create/fail loop.
                crate::logging::write_app_log(&handle, "warn", "note-overlay", &message, None);
            }
        }
    });
}

/// Called only by the background builder above. The fresh window never shows or takes focus.
fn create_hidden_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(LABEL).is_some() {
        return Ok(());
    }
    let url = format!("{}://localhost/note.html", super::overlay::OVERLAY_SCHEME)
        .parse()
        .map_err(|_| "invalid note overlay URL")?;
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::CustomProtocol(url))
        .title(crate::i18n::t(app, "desktop.overlay.windowTitle"))
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .transparent(true)
        .background_color(Color(0, 0, 0, 0))
        .accept_first_mouse(true)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    install_native_backdrop(&window)?;
    #[cfg(not(target_os = "macos"))]
    let _ = window;
    Ok(())
}

fn update_window_presentation(
    app: &AppHandle,
    window: &WebviewWindow,
    reset_position: bool,
) -> Result<(), String> {
    if reset_position {
        if let Ok(Some(monitor)) = app
            .get_webview_window("main")
            .map_or_else(|| window.current_monitor(), |main| main.current_monitor())
        {
            let area = monitor.work_area();
            let scale = monitor.scale_factor();
            let width = window
                .outer_size()
                .map_or((WINDOW_WIDTH * scale) as u32, |size| size.width)
                as i32;
            // Capsule right/top sit 24 logical px from the current monitor's work area.
            let _ = window.set_position(PhysicalPosition::new(
                area.position.x + area.size.width as i32 - width + (9.0 * scale) as i32,
                area.position.y + (11.0 * scale) as i32,
            ));
        }
    }
    #[cfg(target_os = "macos")]
    refresh_native_backdrop(window)?;
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn native_backdrop_opacity(reduce_transparency: bool) -> f64 {
    if reduce_transparency {
        1.0
    } else {
        0.8
    }
}

#[cfg(target_os = "macos")]
fn apply_native_backdrop_opacity(backdrop: &objc2_app_kit::NSVisualEffectView) {
    let reduce = objc2_app_kit::NSWorkspace::sharedWorkspace()
        .accessibilityDisplayShouldReduceTransparency();
    let opacity = native_backdrop_opacity(reduce);
    if backdrop.alphaValue() != opacity {
        backdrop.setAlphaValue(opacity);
    }
}

/// Reuse the visible overlay's 500 ms watcher; keep at most one main-thread update queued.
#[cfg(target_os = "macos")]
fn refresh_native_backdrop(window: &WebviewWindow) -> Result<(), String> {
    if BACKDROP_REFRESH_PENDING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let handle = window.clone();
    let result = window.run_on_main_thread(move || {
        if let Ok(pointer) = handle.ns_view() {
            let content = unsafe { &*pointer.cast::<objc2_app_kit::NSView>() };
            for view in content.subviews().iter() {
                if let Some(backdrop) = view.downcast_ref::<objc2_app_kit::NSVisualEffectView>() {
                    apply_native_backdrop_opacity(backdrop);
                }
            }
        }
        BACKDROP_REFRESH_PENDING.store(false, Ordering::SeqCst);
    });
    if result.is_err() {
        BACKDROP_REFRESH_PENDING.store(false, Ordering::SeqCst);
    }
    result.map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn install_native_backdrop(window: &WebviewWindow) -> Result<(), String> {
    let handle = window.clone();
    window
        .run_on_main_thread(move || {
            use objc2::MainThreadOnly;
            use objc2_app_kit::{
                NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSColor, NSView,
                NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
                NSVisualEffectView, NSWindow, NSWindowOrderingMode,
            };
            use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let Ok(pointer) = handle.ns_view() else {
                return;
            };
            // CSS backdrop-filter only samples inside the WKWebView; it cannot blur desktop pixels.
            // Put AppKit's behind-window material under precisely the 197 × 72 Figma capsule,
            // preserving transparent margins for the CSS shadow rather than frosting the full window.
            unsafe {
                let content = &*pointer.cast::<NSView>();
                if let Ok(pointer) = handle.ns_window() {
                    let window = &*pointer.cast::<NSWindow>();
                    window.setOpaque(false);
                    window.setBackgroundColor(Some(&NSColor::clearColor()));
                }
                let y = if content.isFlipped() {
                    13.0
                } else {
                    content.bounds().size.height - 13.0 - 72.0
                };
                let backdrop = NSVisualEffectView::initWithFrame(
                    NSVisualEffectView::alloc(mtm),
                    NSRect::new(NSPoint::new(33.0, y), NSSize::new(197.0, 72.0)),
                );
                backdrop.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
                backdrop
                    .setAppearance(NSAppearance::appearanceNamed(NSAppearanceNameAqua).as_deref());
                backdrop.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
                backdrop.setState(NSVisualEffectState::Active);
                // Keep one light layer. A second 80% CSS white fill compounds the native tint.
                // Reduce Transparency makes the system material solid; preserve it at full opacity.
                // AppKit owns desktop blur; its public API does not expose a CSS-style blur radius.
                apply_native_backdrop_opacity(&backdrop);
                backdrop.setWantsLayer(true);
                if let Some(layer) = backdrop.layer() {
                    // CALayer's public setter is gated behind an otherwise unused crate feature.
                    let (): () = objc2::msg_send![&*layer, setCornerRadius: 36.0_f64];
                    layer.setMasksToBounds(true);
                }
                content.addSubview_positioned_relativeTo(
                    &backdrop,
                    NSWindowOrderingMode::Below,
                    None,
                );
            }
        })
        .map_err(|error| error.to_string())
}

fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        }
    }
}

fn require_overlay(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == LABEL {
        Ok(())
    } else {
        Err("note overlay action requires note overlay window".into())
    }
}

#[tauri::command]
pub(crate) fn meeting_get_note_overlay_state(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<DisplayState, String> {
    require_overlay(&window)?;
    let binding = binding(&app).ok_or("note overlay is no longer bound")?;
    Ok(display_state(&app, &binding))
}

#[tauri::command]
pub(crate) fn meeting_note_overlay_action(
    app: AppHandle,
    window: WebviewWindow,
    expected_recording_id: String,
    expected_result_attempt: Option<String>,
    expected_phase: Option<Phase>,
    action: ActionKind,
) -> Result<(), String> {
    require_overlay(&window)?;
    // Resolve UI-owned data before taking the runtime lock, even when this command is moved to
    // a background executor in the future.
    let main = app
        .get_webview_window("main")
        .ok_or("main webview is unavailable")?;
    let main_origin = main
        .url()
        .map_err(|error| error.to_string())?
        .origin()
        .ascii_serialization();
    let runtime = app.state::<Runtime>();
    let mut data = runtime
        .data
        .lock()
        .map_err(|_| "recording overlay lock unavailable")?;
    let binding = data
        .binding
        .clone()
        .ok_or("note overlay is no longer bound")?;
    let active = active_recording_id(&app.state::<MeetingManagerState>());
    verify_action_recording(&binding, &expected_recording_id, active.as_deref())?;
    verify_action_attempt(&binding, expected_result_attempt.as_deref(), action)?;
    if main_origin != binding.origin {
        return Err("note overlay origin changed".into());
    }
    let state = display_state(&app, &binding);
    if action == ActionKind::Dismiss && !dismiss_matches_phase(expected_phase, state.phase) {
        // A click rendered while processing must not consume a result that finished before IPC
        // arrived. Refresh the card and let the user dismiss that actual result if they want to.
        drop(data);
        refresh(&app, false)?;
        return Err("note overlay phase changed before closing".into());
    }
    if !action_allowed(action, state.phase) {
        return Err("note is not awaiting the requested retry".into());
    }
    if action == ActionKind::Dismiss {
        // Closing the reminder is entirely local and never pauses/cancels the actual work.
        // Require the new identity field so a delayed click cannot dismiss a later result.
        let activity = lifecycle_activity(&binding, &state);
        update_lifecycle(&app, &binding, |lifecycle| lifecycle.dismiss(activity))?
            .map_err(str::to_string)?;
        drop(data);
        refresh(&app, false)?;
        return Ok(());
    }
    let event = Action {
        id: format!(
            "overlay-{}-{}",
            super::state::unix_millis(),
            ACTION_COUNTER.fetch_add(1, Ordering::SeqCst)
        ),
        scope_key: binding.scope_key,
        recording_id: binding.recording_id,
        origin: binding.origin,
        action,
    };
    data.pending_action = Some(event.clone());
    drop(data);
    crate::logging::write_app_log(
        &app,
        "info",
        "note-overlay",
        "action received",
        Some(
            &serde_json::json!({ "actionId": event.id, "action": event.action,
            "mainVisible": main.is_visible().ok(), "mainMinimized": main.is_minimized().ok() }),
        ),
    );
    restore_main_and_deliver(&app, &event)?;
    retry_unacknowledged_action(app, event);
    Ok(())
}

fn action_allowed(action: ActionKind, phase: Phase) -> bool {
    match action {
        ActionKind::Open => true,
        ActionKind::Retry => phase == Phase::Failed,
        ActionKind::RetrySummary => phase == Phase::SummaryFailed,
        ActionKind::Dismiss => phase != Phase::Recording,
    }
}

fn dismiss_matches_phase(expected: Option<Phase>, actual: Phase) -> bool {
    expected == Some(actual) && actual != Phase::Recording
}

fn verify_action_attempt(
    binding: &Binding,
    expected_attempt: Option<&str>,
    action: ActionKind,
) -> Result<(), String> {
    if expected_attempt.is_some_and(|attempt| attempt != binding.result_attempt) {
        return Err("note overlay result attempt changed".into());
    }
    if action == ActionKind::Dismiss && expected_attempt.is_none() {
        return Err("closing the overlay requires the current result attempt".into());
    }
    Ok(())
}

/// Restore and dispatch on the native UI loop. A minimized WebView can wake after the first
/// event; the same action ID is delivered again only while Web has not acknowledged it.
fn restore_main_and_deliver(app: &AppHandle, action: &Action) -> Result<(), String> {
    let handle = app.clone();
    let action = action.clone();
    app.run_on_main_thread(move || {
        let pending = handle.state::<Runtime>().data.lock().is_ok_and(|data| {
            data.pending_action
                .as_ref()
                .is_some_and(|pending| pending.id == action.id)
        });
        if !pending {
            return;
        }
        let Some(main) = handle.get_webview_window("main") else {
            return;
        };
        if !main
            .url()
            .is_ok_and(|url| url.origin().ascii_serialization() == action.origin)
        {
            return;
        }
        let restored = main.unminimize().and_then(|_| main.show()).and_then(|_| {
            crate::window_state::recover_if_unreachable(&main);
            main.set_focus()
        });
        let delivered = handle.emit_to("main", ACTION_EVENT, &action);
        crate::logging::write_app_log(
            &handle,
            "info",
            "note-overlay",
            "action dispatched after window restore",
            Some(
                &serde_json::json!({ "actionId": action.id, "restoreOk": restored.is_ok(),
                "dispatchOk": delivered.is_ok(), "mainVisible": main.is_visible().ok(),
                "mainMinimized": main.is_minimized().ok(), "mainFocused": main.is_focused().ok() }),
            ),
        );
    })
    .map_err(|error| error.to_string())
}

fn retry_unacknowledged_action(app: AppHandle, action: Action) {
    tauri::async_runtime::spawn(async move {
        // Cover wakeup/animation and listener reattachment without polling indefinitely or
        // stealing focus after the action succeeds. A newer action supersedes this retry too.
        for delay in [150, 350, 700] {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            let pending = app.state::<Runtime>().data.lock().is_ok_and(|data| {
                data.pending_action
                    .as_ref()
                    .is_some_and(|pending| pending.id == action.id)
            });
            if !pending {
                return;
            }
            if let Err(error) = restore_main_and_deliver(&app, &action) {
                crate::logging::write_app_log(&app, "warn", "note-overlay", &error, None);
            }
        }
    });
}

fn verify_action_recording(
    binding: &Binding,
    expected_recording_id: &str,
    active_recording_id: Option<&str>,
) -> Result<(), String> {
    if binding.recording_id != expected_recording_id
        || active_recording_id.is_some_and(|id| id != expected_recording_id)
    {
        return Err("note overlay recording changed".into());
    }
    Ok(())
}

fn require_active_binding(app: &AppHandle, recording_id: &str) -> Result<(), String> {
    let binding = binding(app).ok_or("note overlay is no longer bound")?;
    let active = active_recording_id(&app.state::<MeetingManagerState>());
    verify_action_recording(&binding, recording_id, active.as_deref())?;
    let main = app
        .get_webview_window("main")
        .ok_or("main webview is unavailable")?;
    if !main_matches(&main, &binding) {
        return Err("note overlay origin changed".into());
    }
    if !app
        .state::<MeetingManagerState>()
        .store
        .load_task()
        .is_some_and(|task| {
            task.recording_id == binding.recording_id && task.state == TaskState::Recording
        })
    {
        return Err("recording has changed or already ended".into());
    }
    Ok(())
}

pub(crate) fn stop_from_protocol(app: AppHandle, recording_id: &str) -> Result<(), String> {
    require_active_binding(&app, recording_id)?;
    super::stop_recording(app, LABEL)
}

#[tauri::command]
pub(crate) fn meeting_note_overlay_control(
    app: AppHandle,
    window: WebviewWindow,
    expected_recording_id: String,
    paused: Option<bool>,
) -> Result<(), String> {
    require_overlay(&window)?;
    require_active_binding(&app, &expected_recording_id)?;
    match paused {
        Some(paused) => super::meeting_set_recording_paused(app, window, paused),
        None => super::stop_recording(app, LABEL),
    }
}

pub(crate) fn asset(path: &str, locale: &serde_json::Value) -> Option<(&'static str, Vec<u8>)> {
    let (mime, bytes): (&str, &[u8]) = match path {
        "/note.html" => {
            return Some(("text/html; charset=utf-8", render_html(locale).into_bytes()))
        }
        "/note-assets/logo.png" => ("image/png", include_bytes!("note-overlay-assets/logo.png")),
        "/note-assets/SourceHanSansCN-VF.woff2" => (
            "font/woff2",
            include_bytes!("note-overlay-assets/SourceHanSansCN-VF.woff2"),
        ),
        "/note-assets/open-note.svg" => (
            "image/svg+xml",
            include_bytes!("note-overlay-assets/open-note.svg"),
        ),
        "/note-assets/recording-dot.svg" => (
            "image/svg+xml",
            include_bytes!("note-overlay-assets/recording-dot.svg"),
        ),
        "/note-assets/resume.svg" => (
            "image/svg+xml",
            include_bytes!("note-overlay-assets/resume.svg"),
        ),
        "/note-assets/transcribing.svg" => (
            "image/svg+xml",
            include_bytes!("note-overlay-assets/transcribing.svg"),
        ),
        "/note-assets/open-note-processing.svg" => (
            "image/svg+xml",
            include_bytes!("note-overlay-assets/open-note-processing.svg"),
        ),
        "/note-assets/success.svg" => (
            "image/svg+xml",
            include_bytes!("note-overlay-assets/success.svg"),
        ),
        "/note-assets/failure.svg" => (
            "image/svg+xml",
            include_bytes!("note-overlay-assets/failure.svg"),
        ),
        _ => return None,
    };
    Some((mime, bytes.to_vec()))
}

fn render_html(locale: &serde_json::Value) -> String {
    let html = include_str!("note_overlay.html");
    // Mark before the first paint, only for the platform that installs the native backdrop.
    let html = if cfg!(target_os = "macos") {
        html.replace("<html>", "<html data-backdrop=\"native\">")
    } else {
        html.to_owned()
    };
    super::overlay_locale::render(&html, locale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_transparency_preserves_an_opaque_material_fallback() {
        assert_eq!(native_backdrop_opacity(true), 1.0);
        assert_eq!(native_backdrop_opacity(false), 0.8);
    }

    #[test]
    fn note_visibility_yields_to_native_minimization_and_hiding() {
        assert!(!should_show(true, true, false));
        assert!(should_show(true, true, true));
        assert!(should_show(true, false, false));
        assert!(should_show(false, true, false));
    }

    #[test]
    fn protocol_rejects_unknown_fields_and_invalid_recordings() {
        let base = serde_json::json!({"scopeKey":"owner", "recordingId":"rec-1", "phase":"recording", "noteVisible":false});
        assert!(validate(&serde_json::from_value(base.clone()).unwrap()).is_ok());
        let mut injection = base.clone();
        injection["origin"] = "https://evil.test".into();
        assert!(serde_json::from_value::<SyncRequest>(injection).is_err());
        let mut invalid = base;
        invalid["recordingId"] = "../recording".into();
        assert!(validate(&serde_json::from_value(invalid).unwrap()).is_err());
    }

    #[test]
    fn a_stale_capsule_cannot_control_or_open_the_newer_recording() {
        let binding = Binding {
            scope_key: "owner".into(),
            recording_id: "rec-b".into(),
            origin: "https://snack.mechlabs.cn".into(),
            phase: Phase::Recording,
            note_visible: false,
            native_task_updated_at: String::new(),
            summary_source_version: None,
            completion_ready: false,
            result_attempt: "attempt-b".into(),
        };
        assert!(verify_action_recording(&binding, "rec-a", Some("rec-b")).is_err());
        assert!(verify_action_recording(&binding, "rec-b", Some("rec-c")).is_err());
        assert!(verify_action_recording(&binding, "rec-b", Some("rec-b")).is_ok());
        // Completed notes remain openable when there is no active capture.
        assert!(verify_action_recording(&binding, "rec-b", None).is_ok());
        assert!(verify_action_attempt(&binding, Some("attempt-a"), ActionKind::Dismiss).is_err());
        assert!(verify_action_attempt(&binding, None, ActionKind::Dismiss).is_err());
        assert!(verify_action_attempt(&binding, Some("attempt-b"), ActionKind::Dismiss).is_ok());
        assert!(verify_action_attempt(&binding, None, ActionKind::Open).is_ok());

        let mut state = DisplayState {
            recording_id: "rec-b".into(),
            result_attempt: "attempt-b".into(),
            phase: Phase::Completed,
            elapsed_ms: 0,
            paused: false,
            can_dismiss: true,
        };
        assert_eq!(lifecycle_activity(&binding, &state), Activity::Processing);
        let mut ready_binding = binding.clone();
        ready_binding.completion_ready = true;
        ready_binding.phase = Phase::Completed;
        assert_eq!(
            lifecycle_activity(&ready_binding, &state),
            Activity::Success
        );
        ready_binding.phase = Phase::SummaryGenerating;
        assert_eq!(
            lifecycle_activity(&ready_binding, &state),
            Activity::Processing
        );
        state.phase = Phase::Failed;
        assert_eq!(
            lifecycle_activity(&ready_binding, &state),
            Activity::Failure
        );
    }

    #[test]
    fn stopping_never_advertises_a_ready_note_before_web_resolves_the_queue() {
        let mut task = MeetingTask::new("rec-a".into(), "zh".into());
        for state in [
            TaskState::Recording,
            TaskState::Finalizing,
            TaskState::AwaitingTranscription,
        ] {
            task.state = state;
            assert_eq!(
                resolve_phase(Phase::Recording, Some(&task), false, true, false),
                Phase::Transcribing
            );
        }
        task.state = TaskState::AwaitingTranscription;
        assert_eq!(
            resolve_phase(Phase::Ended, Some(&task), false, false, false),
            Phase::Ended
        );
        task.state = TaskState::TranscribingLocal;
        assert_eq!(
            resolve_phase(Phase::Ended, Some(&task), false, true, false),
            Phase::Transcribing
        );
    }

    #[test]
    fn new_retry_waits_for_native_progress_and_a_new_failure_is_shown() {
        let mut task = MeetingTask::new("rec-a".into(), "zh".into());
        task.state = TaskState::TranscriptionFailed;
        assert_eq!(
            resolve_phase(Phase::Transcribing, Some(&task), false, false, false),
            Phase::Transcribing
        );
        assert_eq!(
            resolve_phase(Phase::Transcribing, Some(&task), false, true, false),
            Phase::Failed
        );
        task.state = TaskState::TranscriptionPaused;
        assert_eq!(
            resolve_phase(Phase::Transcribing, Some(&task), false, true, false),
            Phase::TranscriptionPaused
        );
        task.state = TaskState::FinalizeFailed;
        assert_eq!(
            resolve_phase(Phase::Transcribing, Some(&task), false, true, false),
            Phase::RecordingFailed
        );
        task.state = TaskState::TranscriptReady;
        assert_eq!(
            resolve_phase(Phase::Transcribing, Some(&task), false, true, false),
            Phase::Failed
        );
    }

    #[test]
    fn only_the_matching_acknowledgement_removes_a_pending_action() {
        let event = Action {
            id: "action-b".into(),
            scope_key: "owner".into(),
            recording_id: "rec-a".into(),
            origin: "https://snack.mechlabs.cn".into(),
            action: ActionKind::Open,
        };
        let mut data = RuntimeData {
            binding: None,
            pending_action: Some(event),
        };
        acknowledge_action(&mut data, None);
        assert!(data.pending_action.is_some());
        acknowledge_action(&mut data, Some("action-a"));
        assert!(data.pending_action.is_some());
        acknowledge_action(&mut data, Some("action-b"));
        assert!(data.pending_action.is_none());
    }

    #[test]
    fn summary_phases_preserve_web_progress_only_for_the_current_transcript() {
        let mut task = MeetingTask::new("rec-a".into(), "zh".into());
        task.state = TaskState::TranscriptReady;
        task.transcript = Some(super::super::state::Transcript {
            text: "Meeting transcript".into(),
            language: "zh".into(),
            segments: vec![],
            model_key: "test".into(),
            engine: "test".into(),
            generated_at: "version-1".into(),
        });
        for phase in [
            Phase::SummaryGenerating,
            Phase::SummaryCompleted,
            Phase::SummaryFailed,
        ] {
            assert_eq!(resolve_phase(phase, Some(&task), false, false, true), phase);
            assert_eq!(resolve_phase(phase, Some(&task), false, true, true), phase);
            assert_eq!(
                resolve_phase(phase, Some(&task), false, true, false),
                Phase::Completed
            );
            assert_eq!(
                resolve_phase(phase, Some(&task), true, true, true),
                Phase::Recording
            );
        }
        task.state = TaskState::TranscribingLocal;
        assert_eq!(
            resolve_phase(Phase::SummaryCompleted, Some(&task), false, true, true),
            Phase::Transcribing
        );
        task.state = TaskState::TranscriptionFailed;
        assert_eq!(
            resolve_phase(Phase::SummaryCompleted, Some(&task), false, true, true),
            Phase::Failed
        );
        task.state = TaskState::AwaitingTranscription;
        assert_eq!(
            resolve_phase(Phase::SummaryCompleted, Some(&task), false, true, true),
            Phase::Ended
        );
        task.state = TaskState::TranscriptReady;
        task.transcript.as_mut().unwrap().text.clear();
        assert_eq!(
            resolve_phase(Phase::SummaryCompleted, Some(&task), false, true, true),
            Phase::Failed
        );
    }

    #[test]
    fn retry_actions_cannot_cross_transcription_and_summary_failures() {
        assert!(action_allowed(
            ActionKind::RetrySummary,
            Phase::SummaryFailed
        ));
        assert!(!action_allowed(ActionKind::RetrySummary, Phase::Failed));
        assert!(!action_allowed(
            ActionKind::RetrySummary,
            Phase::SummaryGenerating
        ));
        assert!(!action_allowed(ActionKind::Retry, Phase::SummaryFailed));
        assert!(action_allowed(ActionKind::Retry, Phase::Failed));
        assert!(action_allowed(ActionKind::Open, Phase::SummaryCompleted));
    }

    #[test]
    fn closing_progress_cannot_consume_a_just_finished_result() {
        assert!(dismiss_matches_phase(
            Some(Phase::Transcribing),
            Phase::Transcribing
        ));
        assert!(!dismiss_matches_phase(
            Some(Phase::Transcribing),
            Phase::Completed
        ));
        assert!(!dismiss_matches_phase(
            Some(Phase::Transcribing),
            Phase::Failed
        ));
        assert!(!dismiss_matches_phase(
            Some(Phase::SummaryGenerating),
            Phase::SummaryCompleted
        ));
        assert!(!dismiss_matches_phase(
            Some(Phase::SummaryGenerating),
            Phase::SummaryFailed
        ));
        assert!(dismiss_matches_phase(
            Some(Phase::SummaryFailed),
            Phase::SummaryFailed
        ));
        assert!(!dismiss_matches_phase(
            Some(Phase::Recording),
            Phase::Recording
        ));
        assert!(!dismiss_matches_phase(None, Phase::Failed));
    }

    #[test]
    fn all_figma_assets_are_packaged_offline_with_the_correct_mime_type() {
        for path in [
            "logo.png",
            "open-note.svg",
            "recording-dot.svg",
            "resume.svg",
            "transcribing.svg",
            "open-note-processing.svg",
            "success.svg",
            "failure.svg",
        ] {
            let (mime, bytes) =
                asset(&format!("/note-assets/{path}"), &serde_json::Value::Null).unwrap();
            assert!(!bytes.is_empty());
            assert_eq!(
                mime,
                if path.ends_with(".png") {
                    "image/png"
                } else {
                    "image/svg+xml"
                }
            );
        }
        assert!(asset("/note-assets/../secret", &serde_json::Value::Null).is_none());
    }
}
