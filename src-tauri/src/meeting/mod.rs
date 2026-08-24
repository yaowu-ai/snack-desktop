//! Local meeting recording: orchestration, commands, state persistence and
//! crash recovery.
//!
//! Owns the resource state machine (local model install) and the meeting task
//! state machine (recording → local transcription → Snack chat handoff), plus
//! the native recording overlay.

mod audio;
mod capture;
#[cfg(target_os = "macos")]
mod capture_macos;
#[cfg(target_os = "windows")]
mod capture_windows;
mod catalog;
mod install;
mod network;
mod notifications;
pub(crate) mod overlay;
mod permissions;
mod python_runtime;
pub(crate) mod quick_access;
mod reminder;
#[cfg(target_os = "macos")]
mod reminder_macos;
mod state;
mod transcribe;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use capture::{check_capture_permissions, LiveCaptureStatus, Recorder};
use catalog::{catalog, platform_label, CatalogModel};
use install::InstallManager;
use permissions::{request_mac_permissions, PermissionAccess};
use state::{
    normalize_transcript_file_name, now_rfc3339, unix_millis, MeetingSettings, MeetingStore,
    MeetingTask, ResourceState, ResourceStatus, TaskState, Transcript, TranscriptAssetState,
};

const STATE_EVENT: &str = "meeting-state";
const TRANSCRIPTION_PROGRESS_EVENT: &str = "meeting-transcription-progress";
const MIN_RECORDING_DISK_BYTES: u64 = 200 * 1024 * 1024; // 200 MB headroom

pub(crate) struct MeetingManagerState {
    pub(crate) store: MeetingStore,
    pub(crate) manager: Arc<InstallManager>,
    pub(crate) recorder: Mutex<Option<Recorder>>,
    recording_task_update: Mutex<()>,
    recording_projects: Mutex<Vec<overlay::RecordingProjectOption>>,
    pub(crate) reminder: reminder::RecordingReminderMonitor,
    background_notes_active: AtomicBool,
}

static RECORDING_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static ACTIVE_TRANSCRIPTIONS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static PENDING_TRANSCRIPTION_RESTARTS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static TRANSCRIPTION_GATE: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogInfo {
    key: &'static str,
    display_name: &'static str,
    size_bytes: u64,
    installed_size_bytes: u64,
    languages: &'static str,
    capabilities: &'static str,
    default: bool,
}

impl From<&CatalogModel> for CatalogInfo {
    fn from(model: &CatalogModel) -> Self {
        Self {
            key: model.key.as_str(),
            display_name: model.display_name,
            size_bytes: model.size_bytes,
            installed_size_bytes: model.installed_size_bytes(),
            languages: model.languages,
            capabilities: model.capabilities,
            default: model.key == catalog::ModelKey::DEFAULT,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionStatus {
    microphone: &'static str,
    system_audio: &'static str,
}

fn permission_status(
    microphone: PermissionAccess,
    system_audio: PermissionAccess,
) -> PermissionStatus {
    PermissionStatus {
        microphone: microphone.as_str(),
        system_audio: system_audio.as_str(),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeetingSnapshot {
    available: bool,
    supported_platform: bool,
    recording_reminder_supported: bool,
    capabilities: MeetingCapabilities,
    platform: String,
    resource: ResourceStatus,
    task: Option<MeetingTask>,
    tasks: Vec<MeetingTask>,
    settings: MeetingSettings,
    catalog: Vec<CatalogInfo>,
    permissions: Option<PermissionStatus>,
    disk_free_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeetingCapabilities {
    transcript_project_assets: bool,
    transcription_task_controls: bool,
}

impl MeetingCapabilities {
    fn current() -> Self {
        Self {
            transcript_project_assets: true,
            transcription_task_controls: true,
        }
    }
}

pub(crate) fn build_snapshot(store: &MeetingStore) -> MeetingSnapshot {
    let resource = store.load_resource();
    // Permissions are NOT probed here: the ScreenCaptureKit probe can trigger
    // the system permission prompt, and snapshots are emitted frequently
    // (and polled). Explicit checks only (meeting_check_permissions /
    // start_recording).
    MeetingSnapshot {
        available: true,
        supported_platform: cfg!(any(target_os = "macos", target_os = "windows")),
        recording_reminder_supported: reminder::supported(),
        capabilities: MeetingCapabilities::current(),
        platform: platform_label(),
        resource,
        task: store.load_task(),
        tasks: store.load_task_records(),
        settings: store.load_settings(),
        catalog: catalog().iter().map(CatalogInfo::from).collect(),
        permissions: None,
        disk_free_bytes: install::free_disk_bytes(store).ok(),
    }
}

pub(crate) fn emit_state(app: &AppHandle, store: &MeetingStore) {
    let snapshot = build_snapshot(store);
    let _ = app.emit(STATE_EVENT, snapshot);
}

pub(crate) fn desktop_update_block_reason(app: &AppHandle) -> Option<String> {
    let state = app.try_state::<MeetingManagerState>()?;
    if state.recorder.lock().expect("recorder poisoned").is_some() {
        return Some("正在录音，结束录音后才能更新桌面端".to_string());
    }
    if has_desktop_update_blocking_task(&state.store) {
        return Some("会议内容仍在处理中，完成后才能更新桌面端".to_string());
    }
    if !state.store.load_resource().state.is_idle() {
        return Some("本地资源正在安装，完成后才能更新桌面端".to_string());
    }
    None
}

fn has_desktop_update_blocking_task(store: &MeetingStore) -> bool {
    has_desktop_update_blocking_state(store.load_task(), store.load_task_records())
}

fn has_desktop_update_blocking_state(
    current: Option<MeetingTask>,
    records: Vec<MeetingTask>,
) -> bool {
    records.into_iter().any(|task| task.state.is_active())
        || current.is_some_and(|task| task.state.is_active())
}

pub(crate) fn site_switch_block_reason(app: &AppHandle) -> Option<String> {
    let state = app.try_state::<MeetingManagerState>()?;
    if state.recorder.lock().expect("recorder poisoned").is_some() {
        return Some("正在录音，结束录音后再切换站点".to_string());
    }
    if has_site_switch_blocking_task(&state.store) {
        return Some("会议内容仍在处理中，完成后再切换站点".to_string());
    }
    if state.background_notes_active.load(Ordering::Relaxed) {
        return Some("会议纪要正在生成，完成后再切换站点".to_string());
    }
    None
}

fn has_site_switch_blocking_task(store: &MeetingStore) -> bool {
    has_site_switch_blocking_state(store.load_task(), store.load_task_records())
}

fn has_site_switch_blocking_state(current: Option<MeetingTask>, records: Vec<MeetingTask>) -> bool {
    records
        .into_iter()
        .any(|task| task_blocks_site_switch(&task))
        || current.is_some_and(|task| task_blocks_site_switch(&task))
}

fn task_blocks_site_switch(task: &MeetingTask) -> bool {
    task.state.blocks_site_switch() || task.transcript_asset_state.is_active()
}

#[cfg(test)]
fn task_with_state(state: TaskState) -> MeetingTask {
    MeetingTask::new("site-switch-test".to_string(), "zh".to_string()).with_state(state)
}

#[cfg(test)]
fn site_switch_is_blocked_by_state(state: TaskState) -> bool {
    has_site_switch_blocking_state(None, vec![task_with_state(state)])
}

// ---------------------------------------------------------------------------
// Initialization & crash recovery
// ---------------------------------------------------------------------------

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    let store = MeetingStore::open(app)?;
    let manager = Arc::new(InstallManager::new());
    let reminder_enabled = store.load_settings().recording_reminder_enabled;
    let reminder = reminder::RecordingReminderMonitor::new(app.clone());
    app.manage(MeetingManagerState {
        store: store.clone_for_task(),
        manager: Arc::clone(&manager),
        recorder: Mutex::new(None),
        recording_task_update: Mutex::new(()),
        recording_projects: Mutex::new(Vec::new()),
        reminder,
        background_notes_active: AtomicBool::new(false),
    });

    // Resource reconciliation (downloads/verification never survive a crash).
    let resource = install::reconcile_resource(&store, &manager);
    if resource.state == ResourceState::Ready && install::installed_needs_update(&store, &resource)
    {
        let mut resource = store.load_resource();
        resource = resource.with_state(ResourceState::UpdateRequired);
        resource.error = Some("本地模型有可用的新版本，请确认后更新".to_string());
        let _ = store.save_resource(&resource);
    }
    emit_state(app, &store);

    // Task reconciliation.
    reconcile_tasks(app, &store);
    app.state::<MeetingManagerState>()
        .reminder
        .set_enabled(reminder_enabled);
    if let Err(error) = quick_access::register_saved_shortcut(app, &store) {
        crate::logging::write_app_log(
            app,
            "warn",
            "meeting",
            "meeting shortcut registration failed",
            Some(&serde_json::json!({ "error": error })),
        );
    }

    crate::logging::write_app_log(
        app,
        "info",
        "meeting",
        "meeting feature initialized",
        Some(&serde_json::json!({
            "resourceState": store.load_resource().state,
        })),
    );
    Ok(())
}

fn reconcile_tasks(app: &AppHandle, store: &MeetingStore) {
    let current_task_id = store.load_task().map(|task| task.recording_id);
    if let Some(mut task) = store.load_task() {
        if normalize_retained_task(&mut task) {
            let _ = store.save_task(&task);
        }
        let _ = store.save_task_record(&task);
    }
    for mut task in store.load_task_records() {
        if normalize_retained_task(&mut task) {
            let _ = store.save_task_progress(&task);
        }
        if task.state == TaskState::TranscribingLocal
            && current_task_id.as_deref() != Some(task.recording_id.as_str())
        {
            spawn_transcription(
                app.clone(),
                store.clone_for_task(),
                task.recording_id.clone(),
            );
        }
    }
    reconcile_current_task(app, store);
    if let Err(error) = store.prune_recording_audio(10) {
        crate::logging::write_app_log(
            app,
            "warn",
            "meeting",
            "old meeting audio could not be pruned",
            Some(&serde_json::json!({ "error": error })),
        );
    }
}

fn normalize_retained_task(task: &mut MeetingTask) -> bool {
    let chat_changed = normalize_chat_handoff_state(task);
    let asset_changed = normalize_transcript_asset_upload_state(task);
    chat_changed || asset_changed
}

fn normalize_transcript_asset_upload_state(task: &mut MeetingTask) -> bool {
    if task.transcript_asset_state != TranscriptAssetState::Uploading {
        return false;
    }
    task.transcript_asset_state = TranscriptAssetState::Pending;
    task.transcript_asset_error = None;
    task.updated_at = now_rfc3339();
    true
}

/// Meeting notes are now produced by handing the local transcript to Snack
/// chat. Normalize retained tasks left in legacy server-submission states.
fn normalize_chat_handoff_state(task: &mut MeetingTask) -> bool {
    if task.transcript.is_none()
        || !matches!(
            task.state,
            TaskState::WaitingForNetwork
                | TaskState::GeneratingNotes
                | TaskState::Ready
                | TaskState::NotesFailed
        )
    {
        return false;
    }
    task.state = TaskState::TranscriptReady;
    task.error = None;
    task.next_retry_at = None;
    task.submission.last_error = None;
    task.updated_at = now_rfc3339();
    true
}

fn reconcile_current_task(app: &AppHandle, store: &MeetingStore) {
    let Some(mut task) = store.load_task() else {
        return;
    };
    match task.state {
        TaskState::Recording | TaskState::Finalizing | TaskState::Checking => {
            let audio_path = task.audio_path.clone().map(PathBuf::from);
            match audio_path {
                Some(path) if path.exists() => {
                    // The app crashed mid-recording: recover the on-disk WAV
                    // (header repair) and continue with local transcription.
                    match audio::repair_wav_header(&path) {
                        Ok(sample_count) => {
                            let now = now_rfc3339();
                            task.duration_ms = Some(sample_count * 1000 / 16_000);
                            task.ended_at.get_or_insert_with(|| now.clone());
                            task.audio_bytes = fs::metadata(&path).ok().map(|meta| meta.len());
                            task.state = TaskState::TranscribingLocal;
                            task.error =
                                Some("应用在上次录音中退出，已自动恢复录音文件".to_string());
                            task.updated_at = now;
                            let _ = store.save_task(&task);
                            emit_state(app, store);
                            spawn_transcription(
                                app.clone(),
                                store.clone_for_task(),
                                task.recording_id.clone(),
                            );
                        }
                        Err(message) => {
                            task.state = TaskState::CaptureFailed;
                            task.error = Some(format!("录音文件无法恢复: {message}"));
                            task.updated_at = now_rfc3339();
                            let _ = store.save_task(&task);
                            emit_state(app, store);
                        }
                    }
                }
                _ => {
                    // Nothing was recorded; drop the empty task.
                    store.delete_task_file();
                    emit_state(app, store);
                }
            }
        }
        TaskState::TranscribingLocal => {
            task.error = Some("应用在转写中退出，已自动恢复转写".to_string());
            task.updated_at = now_rfc3339();
            let _ = store.save_task(&task);
            emit_state(app, store);
            spawn_transcription(
                app.clone(),
                store.clone_for_task(),
                task.recording_id.clone(),
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn meeting_get_snapshot(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<MeetingSnapshot, String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    Ok(build_snapshot(&state.store))
}

#[tauri::command]
pub(crate) fn meeting_clear_task_records(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let current_task_active = state
        .store
        .load_task()
        .is_some_and(|task| task.state.is_active());
    let cleared = state.store.clear_task_records()?;
    if !current_task_active {
        overlay::hide_overlay(&app);
    }
    emit_state(&app, &state.store);
    crate::logging::write_app_log(
        &app,
        "info",
        "meeting-records",
        "completed meeting task records cleared without deleting local files",
        Some(
            &serde_json::json!({ "cleared": cleared, "activeTaskPreserved": current_task_active }),
        ),
    );
    Ok(())
}

#[tauri::command]
pub(crate) async fn meeting_choose_storage_directory(
    window: WebviewWindow,
) -> Result<Option<String>, String> {
    require_allowed_window(&window)?;
    Ok(rfd::AsyncFileDialog::new()
        .set_title("选择 Snack 会议录音保存位置")
        .pick_folder()
        .await
        .map(|folder| folder.path().to_string_lossy().to_string()))
}

#[tauri::command]
pub(crate) async fn meeting_import_audio(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<Option<String>, String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let store = state.store.clone_for_task();
    if store.load_resource().state != ResourceState::Ready {
        return Err("请先在录音设置中下载并安装本地模型".to_string());
    }
    let Some(source) = pick_import_audio().await else {
        return Ok(None);
    };
    let task = imported_audio_task(&source)?;
    let recording_id = task.recording_id.clone();
    store.save_task_record(&task)?;
    emit_state(&app, &store);
    spawn_transcription(app, store, recording_id.clone());
    Ok(Some(recording_id))
}

async fn pick_import_audio() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("选择需要本地转写的音频")
        .add_filter(
            "音频文件",
            &["wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "wma"],
        )
        .pick_file()
        .await
        .map(|file| file.path().to_path_buf())
}

fn imported_audio_task(source: &std::path::Path) -> Result<MeetingTask, String> {
    let source = source
        .canonicalize()
        .map_err(|error| format!("无法读取所选音频: {error}"))?;
    let source_bytes = validate_imported_audio(&source)?;
    let recording_id = generate_recording_id();
    let now = now_rfc3339();
    let mut task = MeetingTask::new(recording_id, "zh".to_string());
    task.display_name = imported_display_name(&source);
    task.state = TaskState::TranscribingLocal;
    task.started_at = Some(now.clone());
    task.ended_at = Some(now);
    task.audio_path = Some(source.to_string_lossy().into_owned());
    task.audio_bytes = Some(source_bytes);
    task.audio_file_owned = false;
    Ok(task)
}

fn validate_imported_audio(source: &std::path::Path) -> Result<u64, String> {
    source
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|value| state::is_supported_audio_extension(value))
        .ok_or_else(|| "请选择 WAV、MP3、M4A、AAC、FLAC、OGG、OPUS 或 WMA 音频文件".to_string())?;
    let metadata = fs::metadata(source).map_err(|error| format!("无法读取所选音频: {error}"))?;
    if !metadata.is_file() {
        return Err("请选择有效的音频文件".to_string());
    }
    Ok(metadata.len())
}

fn imported_display_name(source: &std::path::Path) -> Option<String> {
    source
        .file_stem()
        .and_then(|name| normalize_transcript_file_name(&name.to_string_lossy()).ok())
}

#[tauri::command]
pub(crate) fn meeting_update_settings(
    app: AppHandle,
    window: WebviewWindow,
    settings: MeetingSettings,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let previous = state.store.load_settings();
    state.store.prepare_settings(&settings)?;
    save_and_activate_settings(&app, &state.store, &previous, &settings)?;
    state
        .reminder
        .set_enabled(settings.recording_reminder_enabled);
    emit_state(&app, &state.store);
    Ok(())
}

fn save_and_activate_settings(
    app: &AppHandle,
    store: &MeetingStore,
    previous: &MeetingSettings,
    settings: &MeetingSettings,
) -> Result<(), String> {
    quick_access::replace_shortcut(app, &previous.shortcut, &settings.shortcut)?;
    if let Err(error) = store.save_settings(settings) {
        let rollback = quick_access::replace_shortcut(app, &settings.shortcut, &previous.shortcut);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(format!("{error}; 恢复原快捷键失败: {rollback_error}")),
        };
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_install_model(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    require_allowed_window(&window)?;
    let (store, manager) = {
        let state = app.state::<MeetingManagerState>();
        (state.store.clone_for_task(), Arc::clone(&state.manager))
    };
    install::start_install(app, store, manager, catalog::ModelKey::DEFAULT)
}

#[tauri::command]
pub(crate) fn meeting_pause_install(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    install::pause_install(&state.store, &state.manager)?;
    emit_state(&app, &state.store);
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_resume_install(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    install::resume_install(&state.store, &state.manager)?;
    emit_state(&app, &state.store);
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_cancel_install(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    install::cancel_install(&state.store, &state.manager)?;
    emit_state(&app, &state.store);
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_uninstall_model(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<serde_json::Value, String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let store = &state.store;

    // The model must stay in place while any capture or local transcription is active.
    if store.load_task_records().iter().any(|task| {
        matches!(
            task.state,
            TaskState::Checking
                | TaskState::Recording
                | TaskState::Finalizing
                | TaskState::TranscribingLocal
        )
    }) {
        return Err("录音或转写进行中，无法卸载模型".to_string());
    }

    let resource = store.load_resource();
    if resource.state != ResourceState::Ready && resource.state != ResourceState::UpdateRequired {
        return Err("当前没有已安装的模型".to_string());
    }
    let freed = install::uninstall_model(store, &resource)?;
    emit_state(&app, store);
    Ok(serde_json::json!({ "freedBytes": freed }))
}

#[tauri::command]
pub(crate) fn meeting_check_permissions(
    _app: AppHandle,
    window: WebviewWindow,
) -> Result<PermissionStatus, String> {
    require_allowed_window(&window)?;
    #[cfg(target_os = "macos")]
    let (microphone, system_audio) = permissions::check_mac_permission_statuses();
    #[cfg(not(target_os = "macos"))]
    let (microphone, system_audio) = {
        let (microphone, system_audio) = check_capture_permissions()?;
        (
            PermissionAccess::from_granted(microphone),
            PermissionAccess::from_granted(system_audio),
        )
    };
    Ok(permission_status(microphone, system_audio))
}

#[tauri::command]
pub(crate) async fn meeting_request_quick_recording(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    quick_access::request_quick_recording_and_wait(app).await
}

#[tauri::command]
pub(crate) async fn meeting_request_permissions(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<PermissionStatus, String> {
    require_allowed_window(&window)?;
    let (microphone, system_audio) = request_capture_permissions(&app).await?;
    Ok(permission_status(microphone, system_audio))
}

#[tauri::command]
pub(crate) fn meeting_open_permission_settings(
    app: AppHandle,
    _window: WebviewWindow,
    permission: String,
) -> Result<(), String> {
    open_permission_settings(&app, &permission)
}

#[tauri::command]
pub(crate) async fn meeting_start_recording(
    app: AppHandle,
    window: WebviewWindow,
    language: Option<String>,
    consented: bool,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    start_recording(app, language, consented)
}

pub(crate) async fn start_quick_recording(app: AppHandle) -> Result<(), String> {
    let (current_microphone, current_system_audio) = current_permission_statuses()?;
    if can_attempt_recording_without_permission_request(current_microphone, current_system_audio) {
        return start_recording(app, Some("zh".to_string()), true);
    }

    // CoreGraphics can report a stale false value after the Screen & System
    // Audio Recording switch is enabled. Let the actual ScreenCaptureKit
    // recorder make the definitive check only after the user presses Record.
    if current_microphone == PermissionAccess::Granted
        && current_system_audio == PermissionAccess::Unknown
    {
        match start_recording(app.clone(), Some("zh".to_string()), true) {
            Ok(()) => return Ok(()),
            Err(error) if is_system_audio_permission_error(&error) => {}
            Err(error) => return Err(error),
        }
    }

    let (microphone, system_audio) = request_capture_permissions(&app).await?;
    ensure_quick_recording_permissions(&app, microphone, system_audio)?;
    start_recording(app, Some("zh".to_string()), true)
}

fn is_system_audio_permission_error(error: &str) -> bool {
    error.contains("系统音频") || error.contains("屏幕与系统录音")
}

fn current_permission_statuses() -> Result<(PermissionAccess, PermissionAccess), String> {
    #[cfg(target_os = "macos")]
    {
        Ok(permissions::check_mac_permission_statuses())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let (microphone, system_audio) =
            check_capture_permissions().map_err(|error| error.to_string())?;
        Ok((
            PermissionAccess::from_granted(microphone),
            PermissionAccess::from_granted(system_audio),
        ))
    }
}

fn can_attempt_recording_without_permission_request(
    microphone: PermissionAccess,
    system_audio: PermissionAccess,
) -> bool {
    microphone == PermissionAccess::Granted && system_audio == PermissionAccess::Granted
}

async fn request_capture_permissions(
    app: &AppHandle,
) -> Result<(PermissionAccess, PermissionAccess), String> {
    #[cfg(target_os = "macos")]
    {
        request_mac_permissions(app).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let (microphone, system_audio) =
            check_capture_permissions().map_err(|error| error.to_string())?;
        Ok((
            PermissionAccess::from_granted(microphone),
            PermissionAccess::from_granted(system_audio),
        ))
    }
}

fn ensure_quick_recording_permissions(
    app: &AppHandle,
    microphone: PermissionAccess,
    system_audio: PermissionAccess,
) -> Result<(), String> {
    if microphone != PermissionAccess::Granted {
        open_permission_settings(app, "microphone")?;
        return Err("请在系统设置中允许 Snack 使用麦克风".to_string());
    }
    if system_audio != PermissionAccess::Granted {
        open_permission_settings(app, "system_audio")?;
        return Err("请在系统设置中允许 Snack 录制屏幕与系统音频".to_string());
    }
    Ok(())
}

fn start_recording(
    app: AppHandle,
    language: Option<String>,
    consented: bool,
) -> Result<(), String> {
    if !consented {
        return Err("请先确认已获得参会者同意".to_string());
    }
    let state = app.state::<MeetingManagerState>();
    let store = &state.store;

    // 1. Model must be ready.
    let resource = store.load_resource();
    if resource.state != ResourceState::Ready {
        return Err("本地模型未就绪，无法开始录音".to_string());
    }

    // 2. Clicking record again while a recording is active restores a minimized overlay.
    if let Some(task) = store.load_task() {
        if task.state == TaskState::Recording
            && state.recorder.lock().expect("recorder poisoned").is_some()
        {
            overlay::restore_overlay(&app)?;
            return Ok(());
        }
        if task.state.blocks_recording() {
            return Err("存在未完成的会议任务".to_string());
        }
    }

    // 3. No active recorder.
    if state.recorder.lock().expect("recorder poisoned").is_some() {
        return Err("已有录音在进行中".to_string());
    }

    // 4. Permissions.
    let (mic, sys) = check_capture_permissions().map_err(|error| error.to_string())?;
    if !mic && !cfg!(target_os = "macos") {
        return Err("麦克风权限未授权，请在系统设置中允许后重试".to_string());
    }
    if !sys && !cfg!(target_os = "macos") {
        return Err("系统音频权限未授权，请在系统设置中允许后重试".to_string());
    }

    // 5. Disk space.
    match install::free_disk_bytes(store) {
        Ok(free) if free < MIN_RECORDING_DISK_BYTES => {
            return Err("磁盘空间不足，无法开始录音".to_string());
        }
        Err(message) => return Err(format!("无法检查磁盘空间: {message}")),
        _ => {}
    }

    let language = language.unwrap_or_else(|| "zh".to_string());
    let recording_id = generate_recording_id();
    let mut task = MeetingTask::new(recording_id.clone(), language);
    task.state = TaskState::Checking;
    task.started_at = Some(now_rfc3339());
    task.auto_generate_notes_enabled = Some(store.load_settings().auto_generate_notes_enabled);
    store.save_task(&task)?;
    emit_state(&app, store);

    let audio_path = store.audio_path(&recording_id);
    let started_millis = unix_millis();
    state.reminder.set_recording_active(true);
    match capture::start_recording(audio_path.clone(), started_millis) {
        Ok(recorder) => {
            *state.recorder.lock().expect("recorder poisoned") = Some(recorder);
            let mut task = store.load_task().ok_or("任务丢失")?;
            task.state = TaskState::Recording;
            task.audio_path = Some(audio_path.to_string_lossy().to_string());
            store.save_task(&task)?;
            emit_state(&app, store);
            let status = current_live_capture_status(&state)?;
            overlay::show_overlay(&app, build_overlay_state(&state, &task, status))?;
            spawn_overlay_updater(app.clone(), recording_id);
            Ok(())
        }
        Err(error) => {
            state.reminder.set_recording_active(false);
            let mut task = store.load_task().ok_or("任务丢失")?;
            task.state = if error.kind == capture::CaptureErrorKind::PermissionDenied {
                TaskState::PermissionDenied
            } else {
                TaskState::CaptureFailed
            };
            task.error = Some(error.message.clone());
            store.save_task(&task)?;
            emit_state(&app, store);
            if error.kind == capture::CaptureErrorKind::PermissionDenied {
                if let Some(permission) = error.missing_permission {
                    let _ = open_permission_settings(&app, permission);
                }
            }
            Err(error.message)
        }
    }
}

fn current_live_capture_status(state: &MeetingManagerState) -> Result<LiveCaptureStatus, String> {
    state
        .recorder
        .lock()
        .expect("recorder poisoned")
        .as_ref()
        .map(|recorder| LiveCaptureStatus::from_shared(&recorder.shared))
        .ok_or_else(|| "录音状态异常".to_string())
}

fn build_overlay_state(
    state: &MeetingManagerState,
    task: &MeetingTask,
    status: LiveCaptureStatus,
) -> overlay::OverlayState {
    overlay::OverlayState::recording(overlay::RecordingOverlayState {
        recording_id: task.recording_id.clone(),
        elapsed_ms: status.elapsed_ms,
        mic_active: status.mic_active,
        system_audio_active: status.system_audio_active,
        paused: status.paused,
        display_name: state.store.transcript_display_stem(task),
        auto_generate_notes_enabled: task.auto_generate_notes_enabled.unwrap_or(false),
        notes_project_id: task.notes_project_id.clone(),
        notes_project_name: task.notes_project_name.clone(),
        transcript_project_id: task.transcript_project_id.clone(),
        transcript_project_name: task.transcript_project_name.clone(),
        projects: state
            .recording_projects
            .lock()
            .expect("recording projects poisoned")
            .clone(),
    })
}

#[tauri::command]
pub(crate) fn meeting_set_recording_paused(
    app: AppHandle,
    window: WebviewWindow,
    paused: bool,
) -> Result<(), String> {
    if !overlay::is_overlay_window(&window) {
        require_allowed_window(&window)?;
    }
    let state = app.state::<MeetingManagerState>();
    let guard = state.recorder.lock().expect("recorder poisoned");
    let recorder = guard.as_ref().ok_or("当前没有进行中的录音")?;
    if paused {
        recorder.pause();
    } else {
        recorder.resume();
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_set_recording_file_name(
    app: AppHandle,
    window: WebviewWindow,
    display_name: String,
) -> Result<(), String> {
    meeting_set_transcript_file_title(app, window, display_name)
}

#[tauri::command]
pub(crate) fn meeting_set_transcript_file_title(
    app: AppHandle,
    window: WebviewWindow,
    display_name: String,
) -> Result<(), String> {
    require_recording_overlay(&window)?;
    update_current_recording_task(&app, |task| {
        task.display_name = normalize_recording_display_name(&display_name)?;
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn meeting_set_recording_auto_notes(
    app: AppHandle,
    window: WebviewWindow,
    enabled: bool,
) -> Result<(), String> {
    require_recording_overlay(&window)?;
    update_current_recording_task(&app, |task| {
        task.auto_generate_notes_enabled = Some(enabled);
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn meeting_request_recording_project(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<(), String> {
    require_recording_overlay(&window)?;
    let recording_id = current_recording_task(&app)?.recording_id;
    app.emit(
        "meeting-recording-project-requested",
        serde_json::json!({ "recordingId": recording_id }),
    )
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn meeting_set_recording_projects(
    app: AppHandle,
    window: WebviewWindow,
    projects: Vec<overlay::RecordingProjectOption>,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let projects = normalize_recording_projects(projects)?;
    *state
        .recording_projects
        .lock()
        .expect("recording projects poisoned") = projects;
    clear_missing_recording_project(&app)?;
    refresh_recording_overlay(&app);
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_set_recording_project(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    project_id: Option<String>,
    project_name: Option<String>,
) -> Result<(), String> {
    if !overlay::is_overlay_window(&window) {
        require_allowed_window(&window)?;
    }
    update_current_recording_task(&app, |task| {
        if task.recording_id != recording_id {
            return Err("录音任务已发生变化，请重新选择".to_string());
        }
        let project_id = project_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if project_id
            .as_deref()
            .is_some_and(|value| !value.chars().all(|character| character.is_ascii_digit()))
        {
            return Err("项目 ID 无效".to_string());
        }
        task.notes_project_name = project_id.as_ref().and_then(|_| {
            project_name
                .map(|value| value.trim().chars().take(100).collect::<String>())
                .filter(|value| !value.is_empty())
        });
        task.notes_project_id = project_id;
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn meeting_set_transcript_project(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    project_id: Option<String>,
    project_name: Option<String>,
) -> Result<(), String> {
    require_main_or_recording_overlay(&window)?;
    let project = normalize_transcript_project(project_id, project_name)?;
    update_task_record(&app, &recording_id, move |task| {
        apply_transcript_project(task, project)
    })
}

#[tauri::command]
pub(crate) fn meeting_claim_transcript_asset_upload(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    project_id: String,
) -> Result<bool, String> {
    require_allowed_window(&window)?;
    let project_id = normalize_numeric_id(&project_id, "项目 ID")?;
    let mut claimed = false;
    update_task_record(&app, &recording_id, |task| {
        claimed = claim_transcript_asset_upload(task, &project_id)?;
        Ok(())
    })?;
    Ok(claimed)
}

#[tauri::command]
pub(crate) fn meeting_mark_transcript_asset_file_uploaded(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    project_id: String,
    file_id: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let project_id = normalize_numeric_id(&project_id, "项目 ID")?;
    let file_id = normalize_numeric_id(&file_id, "文件 ID")?;
    update_task_record(&app, &recording_id, move |task| {
        require_active_asset_upload(task, &project_id)?;
        task.transcript_asset_file_id = Some(file_id);
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn meeting_complete_transcript_asset_upload(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    project_id: String,
    file_id: String,
    asset_id: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let identifiers = normalize_asset_identifiers(project_id, file_id, asset_id)?;
    update_task_record(&app, &recording_id, move |task| {
        complete_transcript_asset_upload(task, identifiers)
    })
}

#[tauri::command]
pub(crate) fn meeting_fail_transcript_asset_upload(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    project_id: String,
    error: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let project_id = normalize_numeric_id(&project_id, "项目 ID")?;
    update_task_record(&app, &recording_id, move |task| {
        require_active_asset_upload(task, &project_id)?;
        task.transcript_asset_state = TranscriptAssetState::Failed;
        task.transcript_asset_error = normalize_asset_error(&error);
        Ok(())
    })
}

fn require_main_or_recording_overlay(window: &WebviewWindow) -> Result<(), String> {
    if overlay::is_overlay_window(window) {
        return Ok(());
    }
    require_allowed_window(window)
}

fn normalize_transcript_project(
    project_id: Option<String>,
    project_name: Option<String>,
) -> Result<Option<(String, String)>, String> {
    let Some(project_id) = project_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let project_id = normalize_numeric_id(&project_id, "项目 ID")?;
    let project_name: String = project_name
        .unwrap_or_default()
        .trim()
        .chars()
        .take(100)
        .collect();
    if project_name.is_empty() {
        return Err("项目名称不能为空".to_string());
    }
    Ok(Some((project_id, project_name)))
}

fn apply_transcript_project(
    task: &mut MeetingTask,
    project: Option<(String, String)>,
) -> Result<(), String> {
    if task.transcript_asset_state == TranscriptAssetState::Saved {
        return Err("转写文件已经保存到项目".to_string());
    }
    let (project_id, project_name) = project.unzip();
    task.transcript_project_id = project_id;
    task.transcript_project_name = project_name;
    task.transcript_asset_state = if task.transcript_project_id.is_some() {
        TranscriptAssetState::Pending
    } else {
        TranscriptAssetState::NotSelected
    };
    task.transcript_asset_file_id = None;
    task.transcript_asset_id = None;
    task.transcript_asset_error = None;
    Ok(())
}

fn claim_transcript_asset_upload(task: &mut MeetingTask, project_id: &str) -> Result<bool, String> {
    require_asset_project(task, project_id)?;
    if task.transcript.is_none() || task.transcript_asset_state == TranscriptAssetState::Saved {
        return Ok(false);
    }
    if task.transcript_asset_state == TranscriptAssetState::NotSelected {
        return Ok(false);
    }
    task.transcript_asset_state = TranscriptAssetState::Uploading;
    task.transcript_asset_error = None;
    Ok(true)
}

fn require_active_asset_upload(task: &MeetingTask, project_id: &str) -> Result<(), String> {
    require_asset_project(task, project_id)?;
    if task.transcript_asset_state != TranscriptAssetState::Uploading {
        return Err("转写资产上传状态已发生变化".to_string());
    }
    Ok(())
}

fn require_asset_project(task: &MeetingTask, project_id: &str) -> Result<(), String> {
    if task.transcript_project_id.as_deref() == Some(project_id) {
        return Ok(());
    }
    Err("转写保存项目已发生变化".to_string())
}

fn normalize_numeric_id(value: &str, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
        return Err(format!("{label} 无效"));
    }
    Ok(value.to_string())
}

type AssetIdentifiers = (String, String, String);

fn normalize_asset_identifiers(
    project_id: String,
    file_id: String,
    asset_id: String,
) -> Result<AssetIdentifiers, String> {
    Ok((
        normalize_numeric_id(&project_id, "项目 ID")?,
        normalize_numeric_id(&file_id, "文件 ID")?,
        normalize_numeric_id(&asset_id, "资产 ID")?,
    ))
}

fn complete_transcript_asset_upload(
    task: &mut MeetingTask,
    identifiers: AssetIdentifiers,
) -> Result<(), String> {
    let (project_id, file_id, asset_id) = identifiers;
    require_active_asset_upload(task, &project_id)?;
    task.transcript_asset_state = TranscriptAssetState::Saved;
    task.transcript_asset_file_id = Some(file_id);
    task.transcript_asset_id = Some(asset_id);
    task.transcript_asset_error = None;
    Ok(())
}

fn normalize_asset_error(error: &str) -> Option<String> {
    let error = error.trim().chars().take(240).collect::<String>();
    (!error.is_empty()).then_some(error)
}

fn update_task_record(
    app: &AppHandle,
    recording_id: &str,
    update: impl FnOnce(&mut MeetingTask) -> Result<(), String>,
) -> Result<(), String> {
    let state = app.state::<MeetingManagerState>();
    let _guard = state
        .recording_task_update
        .lock()
        .expect("recording task update poisoned");
    let mut task = state
        .store
        .load_task_record(recording_id)
        .ok_or("没有找到本地转写记录")?;
    update(&mut task)?;
    task.updated_at = now_rfc3339();
    state.store.save_task_progress(&task)?;
    emit_state(app, &state.store);
    refresh_recording_overlay(app);
    Ok(())
}

fn require_recording_overlay(window: &WebviewWindow) -> Result<(), String> {
    if overlay::is_overlay_window(window) {
        Ok(())
    } else {
        Err("只有录音浮窗可以执行此操作".to_string())
    }
}

fn normalize_recording_display_name(value: &str) -> Result<Option<String>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    normalize_transcript_file_name(value).map(Some)
}

fn normalize_recording_projects(
    projects: Vec<overlay::RecordingProjectOption>,
) -> Result<Vec<overlay::RecordingProjectOption>, String> {
    let mut normalized = Vec::with_capacity(projects.len());
    for project in projects {
        let project = normalize_recording_project(project)?;
        if normalized
            .iter()
            .any(|item: &overlay::RecordingProjectOption| item.project_id == project.project_id)
        {
            continue;
        }
        normalized.push(project);
    }
    Ok(normalized)
}

fn normalize_recording_project(
    project: overlay::RecordingProjectOption,
) -> Result<overlay::RecordingProjectOption, String> {
    let project_id = project.project_id.trim().to_string();
    let project_name = project
        .project_name
        .trim()
        .chars()
        .take(100)
        .collect::<String>();
    if project_id.is_empty() || !project_id.chars().all(|value| value.is_ascii_digit()) {
        return Err("项目 ID 无效".to_string());
    }
    if project_name.is_empty() {
        return Err("项目名称不能为空".to_string());
    }
    Ok(overlay::RecordingProjectOption {
        project_id,
        project_name,
    })
}

fn clear_missing_recording_project(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<MeetingManagerState>();
    let Some(task) = state.store.load_task() else {
        return Ok(());
    };
    if task.state != TaskState::Recording {
        return Ok(());
    }
    let projects = state
        .recording_projects
        .lock()
        .expect("recording projects poisoned");
    let project_is_missing = |project_id: Option<&str>| {
        project_id.is_some_and(|project_id| {
            !projects
                .iter()
                .any(|project| project.project_id == project_id)
        })
    };
    let notes_project_missing = project_is_missing(task.notes_project_id.as_deref());
    let transcript_project_missing = project_is_missing(task.transcript_project_id.as_deref());
    drop(projects);
    if !notes_project_missing && !transcript_project_missing {
        return Ok(());
    }
    update_current_recording_task(app, |task| {
        if notes_project_missing {
            task.notes_project_id = None;
            task.notes_project_name = None;
        }
        if transcript_project_missing {
            apply_transcript_project(task, None)?;
        }
        Ok(())
    })
}

fn refresh_recording_overlay(app: &AppHandle) {
    let Some(window) = app.get_webview_window(overlay::OVERLAY_LABEL) else {
        return;
    };
    let state = app.state::<MeetingManagerState>();
    let Some(task) = state.store.load_task() else {
        return;
    };
    let Ok(status) = current_live_capture_status(&state) else {
        return;
    };
    overlay::update_overlay(&window, build_overlay_state(&state, &task, status));
}

fn current_recording_task(app: &AppHandle) -> Result<MeetingTask, String> {
    app.state::<MeetingManagerState>()
        .store
        .load_task()
        .filter(|task| task.state == TaskState::Recording)
        .ok_or_else(|| "当前没有进行中的录音".to_string())
}

fn update_current_recording_task(
    app: &AppHandle,
    update: impl FnOnce(&mut MeetingTask) -> Result<(), String>,
) -> Result<(), String> {
    let state = app.state::<MeetingManagerState>();
    let _update_guard = state
        .recording_task_update
        .lock()
        .expect("recording task update poisoned");
    let mut task = current_recording_task(app)?;
    update(&mut task)?;
    task.updated_at = now_rfc3339();
    state.store.save_task(&task)?;
    emit_state(app, &state.store);
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_stop_recording(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    // The overlay is a local window; the main webview must pass origin checks.
    if !overlay::is_overlay_window(&window) {
        require_allowed_window(&window)?;
    }
    stop_recording(app, window.label())
}

pub(crate) fn stop_recording_from_overlay(app: AppHandle) -> Result<(), String> {
    stop_recording(app, overlay::OVERLAY_LABEL)
}

fn stop_recording(app: AppHandle, source: &str) -> Result<(), String> {
    let state = app.state::<MeetingManagerState>();
    let store = state.store.clone_for_task();
    let recorder = state.recorder.lock().expect("recorder poisoned").take();
    let Some(recorder) = recorder else {
        if stop_was_already_requested(&store) {
            return Ok(());
        }
        return Err("当前没有进行中的录音".to_string());
    };
    crate::logging::write_app_log(
        &app,
        "info",
        "meeting-finalize",
        "stop recording command received",
        Some(&serde_json::json!({ "window": source })),
    );

    let mut task = store.load_task().ok_or("任务丢失")?;
    if task.state != TaskState::Recording {
        *state.recorder.lock().expect("recorder poisoned") = Some(recorder);
        return Err("录音状态异常".to_string());
    }
    task.state = TaskState::Finalizing;
    store.save_task(&task)?;
    state.reminder.set_recording_active(false);
    emit_state(&app, &store);
    overlay::hide_overlay(&app);

    crate::logging::write_app_log(
        &app,
        "info",
        "meeting-finalize",
        "recording moved to background finalization",
        Some(&serde_json::json!({ "recordingId": task.recording_id })),
    );

    tauri::async_runtime::spawn_blocking(move || finalize_recording(app, store, recorder));
    Ok(())
}

fn stop_was_already_requested(store: &MeetingStore) -> bool {
    store.load_task().is_some_and(|task| {
        matches!(
            task.state,
            TaskState::Finalizing | TaskState::TranscribingLocal | TaskState::TranscriptReady
        )
    })
}

fn finalize_recording(app: AppHandle, store: MeetingStore, recorder: Recorder) {
    // Stop capture, drain, finalize and validate the WAV.
    if let Err(message) = recorder.stop() {
        fail_finalize(&app, &store, message);
        return;
    }

    let Some(mut task) = store.load_task() else {
        return;
    };
    let Some(audio_path) = task.audio_path.clone().map(PathBuf::from) else {
        fail_finalize(&app, &store, "音频路径丢失".to_string());
        return;
    };
    let duration_ms = match audio::read_wav_i16(&audio_path) {
        Ok((_, duration_ms)) => duration_ms,
        Err(message) => {
            fail_finalize(&app, &store, format!("录音文件校验失败: {message}"));
            return;
        }
    };

    task.state = TaskState::TranscribingLocal;
    task.duration_ms = Some(duration_ms);
    task.ended_at = Some(now_rfc3339());
    task.audio_bytes = fs::metadata(&audio_path).ok().map(|meta| meta.len());
    task.error = None;
    if let Err(message) = store.save_task_record(&task) {
        fail_finalize(&app, &store, message);
        return;
    }
    store.delete_task_file();
    if let Err(message) = store.prune_recording_audio(10) {
        crate::logging::write_app_log(
            &app,
            "warn",
            "meeting-finalize",
            "old meeting audio could not be pruned",
            Some(&serde_json::json!({ "error": message })),
        );
    }
    emit_state(&app, &store);
    crate::logging::write_app_log(
        &app,
        "info",
        "meeting-finalize",
        "recording finalized and transcription started",
        Some(&serde_json::json!({ "recordingId": task.recording_id })),
    );
    spawn_transcription(app, store, task.recording_id);
}

fn fail_finalize(app: &AppHandle, store: &MeetingStore, message: String) {
    let Some(mut task) = store.load_task() else {
        crate::logging::write_app_log(app, "error", "meeting-finalize", &message, None);
        return;
    };
    let recording_id = task.recording_id.clone();
    crate::logging::write_app_log(
        app,
        "error",
        "meeting-finalize",
        "recording finalization failed",
        Some(&serde_json::json!({
            "recordingId": recording_id,
            "audioFileExists": task.audio_path.as_deref().is_some_and(|path| PathBuf::from(path).is_file()),
            "reason": message,
        })),
    );
    task.state = TaskState::FinalizeFailed;
    task.error = Some(message);
    let _ = store.save_task(&task);
    emit_state(app, store);
    notifications::notify_transcript_failed(app, &recording_id);
}

#[tauri::command]
pub(crate) fn meeting_get_recording_status(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<Option<LiveCaptureStatus>, String> {
    if !overlay::is_overlay_window(&window) {
        require_allowed_window(&window)?;
    }
    let state = app.state::<MeetingManagerState>();
    let guard = state.recorder.lock().expect("recorder poisoned");
    Ok(guard
        .as_ref()
        .map(|recorder| LiveCaptureStatus::from_shared(&recorder.shared)))
}

#[tauri::command]
pub(crate) fn meeting_get_recording_overlay_state(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<overlay::OverlayState, String> {
    require_recording_overlay(&window)?;
    let state = app.state::<MeetingManagerState>();
    let task = current_recording_task(&app)?;
    let status = current_live_capture_status(&state)?;
    Ok(build_overlay_state(&state, &task, status))
}

/// Retry the local pipeline after a finalize/transcription failure.
/// The raw audio is retained on failure, so no re-recording is needed.
#[tauri::command]
pub(crate) fn meeting_retry_pipeline(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let store = state.store.clone_for_task();
    let mut task = store
        .load_task()
        .ok_or_else(|| "没有会议任务".to_string())?;
    match task.state {
        TaskState::FinalizeFailed => {
            let audio_path = task
                .audio_path
                .clone()
                .map(PathBuf::from)
                .ok_or_else(|| "音频文件丢失".to_string())?;
            if !audio_path.exists() {
                return Err("音频文件不存在".to_string());
            }
            let (_, duration_ms) = audio::read_wav_i16(&audio_path)
                .map_err(|message| format!("录音文件无法恢复: {message}"))?;
            task.state = TaskState::TranscribingLocal;
            task.duration_ms = Some(duration_ms);
            task.error = None;
            store.save_task(&task)?;
            emit_state(&app, &store);
            spawn_transcription(app, store, task.recording_id);
            Ok(())
        }
        TaskState::TranscriptionFailed => {
            task.state = TaskState::TranscribingLocal;
            task.error = None;
            store.save_task(&task)?;
            emit_state(&app, &store);
            spawn_transcription(app, store, task.recording_id);
            Ok(())
        }
        _ => Err("当前状态不允许重试".to_string()),
    }
}

#[tauri::command]
pub(crate) fn meeting_retry_submit(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let store = state.store.clone_for_task();
    let task = store
        .load_task()
        .ok_or_else(|| "没有会议任务".to_string())?;
    generate_notes(app, store, task.recording_id)
}

#[tauri::command]
pub(crate) fn meeting_generate_notes(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    generate_notes(app.clone(), state.store.clone_for_task(), recording_id)
}

#[tauri::command]
pub(crate) fn meeting_open_notes_in_chat(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    open_notes_in_chat(&app, &recording_id, false)
}

#[tauri::command]
pub(crate) fn meeting_notify_notes_completed(
    app: AppHandle,
    window: WebviewWindow,
    session_id: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    if !is_valid_session_id(&session_id) {
        return Err("无效的会话 ID".to_string());
    }
    notifications::notify_completed_session(&app, &session_id);
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_set_notes_activity(
    app: AppHandle,
    window: WebviewWindow,
    active: bool,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    app.state::<MeetingManagerState>()
        .background_notes_active
        .store(active, Ordering::Relaxed);
    Ok(())
}

fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 20
        && session_id.bytes().all(|byte| byte.is_ascii_digit())
}

fn handoff_completed_transcript(app: &AppHandle, recording_id: &str) {
    notifications::notify_transcript_ready(app);
    let manager = app.state::<MeetingManagerState>();
    let settings = manager.store.load_settings();
    let task = manager.store.load_task_record(recording_id);
    let enabled = task
        .as_ref()
        .and_then(|task| task.auto_generate_notes_enabled)
        .unwrap_or_else(|| should_automatically_generate_notes(&settings));
    if !enabled {
        log_automatic_notes_skipped(app, recording_id);
        return;
    }
    match open_notes_in_chat(app, recording_id, true) {
        Ok(()) => crate::logging::write_app_log(
            app,
            "info",
            "meeting-notes-handoff",
            "transcript queued for background notes generation",
            Some(&serde_json::json!({ "recordingId": recording_id })),
        ),
        Err(error) => {
            crate::logging::write_app_log(
                app,
                "warn",
                "meeting-notes-handoff",
                "automatic notes handoff failed; transcript remains available in meeting tasks",
                Some(&serde_json::json!({ "recordingId": recording_id, "reason": error })),
            );
        }
    }
}

fn should_automatically_generate_notes(settings: &MeetingSettings) -> bool {
    settings.auto_generate_notes_enabled
}

fn log_automatic_notes_skipped(app: &AppHandle, recording_id: &str) {
    crate::logging::write_app_log(
        app,
        "info",
        "meeting-notes-handoff",
        "automatic notes handoff skipped by user preference",
        Some(&serde_json::json!({ "recordingId": recording_id })),
    );
}

fn open_notes_in_chat(
    app: &AppHandle,
    recording_id: &str,
    auto_submit: bool,
) -> Result<(), String> {
    let state = app.state::<MeetingManagerState>();
    let task = state
        .store
        .load_task_record(recording_id)
        .ok_or_else(|| "没有找到本地转写记录".to_string())?;
    let transcript = task
        .transcript
        .as_ref()
        .ok_or_else(|| "本地转写尚未完成".to_string())?;
    let settings = state.store.load_settings();
    let transcript_name = task
        .transcript_path
        .as_deref()
        .and_then(|path| {
            PathBuf::from(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| format!("Snack会议-{}.txt", task.recording_id));
    let transcript_text = state::transcript_text(transcript);
    if auto_submit {
        crate::record_import::queue_background_with_attachment(
            app,
            settings.notes_prompt,
            transcript_name,
            transcript_text,
            task.notes_project_id.clone(),
            task.notes_project_name.clone(),
        )?;
    } else {
        crate::record_import::open_prefill_with_attachment(
            app,
            settings.notes_prompt,
            transcript_name,
            transcript_text,
            false,
            task.notes_project_id.clone(),
            task.notes_project_name.clone(),
        )?;
    }
    overlay::hide_overlay(app);
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_open_local_file(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    kind: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let task = state
        .store
        .load_task_record(&recording_id)
        .ok_or_else(|| "没有找到本地录音".to_string())?;
    let path = match kind.as_str() {
        "transcript" => task.transcript_path.map(PathBuf::from),
        "audio" => task.audio_path.map(PathBuf::from),
        _ => return Err("不支持的本地文件类型".to_string()),
    }
    .ok_or_else(|| "本地文件不存在".to_string())?;
    if !path.exists() {
        return Err("本地文件不存在".to_string());
    }
    if kind == "transcript" {
        crate::platform::reveal_path(&path)
    } else {
        crate::platform::open_path(&path)
    }
}

#[tauri::command]
pub(crate) fn meeting_rename_task_record(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    display_name: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let store = state.store.clone_for_task();
    let mut task = store
        .load_task_record(&recording_id)
        .ok_or_else(|| "没有找到本地转写记录".to_string())?;
    let display_name = normalize_transcript_file_name(&display_name)?;
    rename_transcript_text_file(&mut task, &display_name)?;
    task.display_name = Some(display_name);
    task.updated_at = now_rfc3339();
    store.save_task_progress(&task)?;
    emit_state(&app, &store);
    Ok(())
}

fn rename_transcript_text_file(task: &mut MeetingTask, display_name: &str) -> Result<(), String> {
    let Some(current_path) = task.transcript_path.as_deref().map(PathBuf::from) else {
        return Ok(());
    };
    let next_path = current_path.with_file_name(display_name);
    if next_path == current_path {
        return Ok(());
    }
    if next_path.exists() {
        return Err("同名转写文件已存在，请换一个名称".to_string());
    }
    fs::rename(&current_path, &next_path)
        .map_err(|error| format!("无法重命名本地转写文件: {error}"))?;
    task.transcript_path = Some(next_path.to_string_lossy().into_owned());
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_set_transcription_paused(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
    paused: bool,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let store = state.store.clone_for_task();
    let mut task = store
        .load_task_record(&recording_id)
        .ok_or_else(|| "没有找到本地转写任务".to_string())?;
    task.state = next_transcription_pause_state(task.state, paused)?;
    task.error = None;
    task.updated_at = now_rfc3339();
    store.save_task_progress(&task)?;
    emit_state(&app, &store);
    if !paused {
        spawn_transcription(app, store, recording_id);
    }
    Ok(())
}

fn next_transcription_pause_state(state: TaskState, paused: bool) -> Result<TaskState, String> {
    match (state, paused) {
        (TaskState::TranscribingLocal, true) => Ok(TaskState::TranscriptionPaused),
        (TaskState::TranscriptionPaused, false) => Ok(TaskState::TranscribingLocal),
        (TaskState::TranscriptionPaused, true) | (TaskState::TranscribingLocal, false) => Ok(state),
        _ => Err("当前任务不能切换转写状态".to_string()),
    }
}

#[tauri::command]
pub(crate) fn meeting_delete_transcription_task(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let task = state
        .store
        .load_task_record(&recording_id)
        .ok_or_else(|| "没有找到本地转写任务".to_string())?;
    if !matches!(
        task.state,
        TaskState::TranscribingLocal | TaskState::TranscriptionPaused
    ) {
        return Err("只能移除进行中或已暂停的转写任务".to_string());
    }
    state.store.delete_task_record(&recording_id)?;
    emit_state(&app, &state.store);
    Ok(())
}

#[tauri::command]
pub(crate) fn meeting_retranscribe(
    app: AppHandle,
    window: WebviewWindow,
    recording_id: String,
) -> Result<(), String> {
    require_allowed_window(&window)?;
    let state = app.state::<MeetingManagerState>();
    let store = state.store.clone_for_task();
    let mut task = store
        .load_task_record(&recording_id)
        .ok_or_else(|| "没有找到本地录音".to_string())?;
    let audio_path = task
        .audio_path
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .ok_or_else(|| "源录音文件不存在".to_string())?;
    let duration_ms = if audio_path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("wav"))
    {
        Some(
            audio::read_wav_i16(&audio_path)
                .map_err(|message| format!("源录音文件无法读取: {message}"))?
                .1,
        )
    } else {
        task.duration_ms
    };
    task.state = TaskState::TranscribingLocal;
    task.duration_ms = duration_ms;
    task.error = None;
    task.updated_at = now_rfc3339();
    store.save_task_progress(&task)?;
    emit_state(&app, &store);
    spawn_transcription(app, store, recording_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Transcription orchestration
// ---------------------------------------------------------------------------

fn spawn_transcription(app: AppHandle, store: MeetingStore, recording_id: String) {
    if !register_transcription(&recording_id) {
        return;
    }
    let failure_app = app.clone();
    let failure_store = store.clone_for_task();
    let failure_recording_id = recording_id.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("snack-transcribe".to_string())
        .spawn(move || {
            let _registration = TranscriptionRegistration::new(
                app.clone(),
                store.clone_for_task(),
                recording_id.clone(),
            );
            let _gate = TRANSCRIPTION_GATE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !store
                .load_task_record(&recording_id)
                .is_some_and(|task| task.state == TaskState::TranscribingLocal)
            {
                return;
            }
            let resource = store.load_resource();
            let model_key = match resource
                .model_key
                .as_deref()
                .and_then(catalog::ModelKey::parse)
            {
                Some(key) => key,
                None => {
                    fail_transcription_task(
                        &app,
                        &store,
                        &recording_id,
                        "本地模型未安装".to_string(),
                    );
                    return;
                }
            };
            let model_path = match install::installed_model_path(&store, &resource) {
                Ok(path) => path,
                Err(message) => {
                    fail_transcription_task(&app, &store, &recording_id, message);
                    return;
                }
            };
            let Some(model_dir) = model_path.parent().map(PathBuf::from) else {
                fail_transcription_task(
                    &app,
                    &store,
                    &recording_id,
                    "本地模型路径无效".to_string(),
                );
                return;
            };

            let Some(mut task) = store.load_task_record(&recording_id) else {
                return;
            };
            if let Err(message) = store.ensure_transcript_output_directory(&task) {
                let message = format!("无法创建当天转写目录: {message}");
                fail_transcription_task(&app, &store, &recording_id, message);
                return;
            }
            let wav_path = match prepare_transcription_audio(&store, &mut task) {
                Ok(path) => path,
                Err(message) => {
                    fail_transcription_task(&app, &store, &recording_id, message);
                    return;
                }
            };
            let language = task.language.clone();
            drop(task);

            let app_for_progress = app.clone();
            let store_for_progress = store.clone_for_task();
            let recording_id_for_progress = recording_id.clone();
            let runtime_dir = model_dir.join("runtime");
            let ensure_runtime = || {
                let on_stage = |_| {};
                python_runtime::ensure_ready(python_runtime::RuntimeSetup {
                    app: &app,
                    runtime_dir: &runtime_dir,
                    on_stage: &on_stage,
                })
                .map(drop)
            };
            let outcome = transcribe::transcribe_file(
                transcribe::TranscriptionRequest {
                    model_key,
                    model_dir: &model_dir,
                    wav_path: &wav_path,
                    language: &language,
                },
                ensure_runtime,
                move |progress| {
                    let _ = app_for_progress.emit(
                        TRANSCRIPTION_PROGRESS_EVENT,
                        serde_json::json!({
                            "recordingId": recording_id_for_progress,
                            "percent": progress.percent,
                            "remainingSeconds": progress.remaining_seconds,
                            "currentText": progress.current_text,
                            "segmentCount": progress.segment_count,
                        }),
                    );
                    // Abort if the task is no longer in transcribing state.
                    store_for_progress
                        .load_task_record(&recording_id_for_progress)
                        .map(|task| task.state == TaskState::TranscribingLocal)
                        .unwrap_or(false)
                },
            );

            let transcript = match outcome {
                Ok(transcribe::TranscriptionOutcome::NoAudioDetected) => {
                    finish_without_audio(&app, &store, &recording_id);
                    return;
                }
                Ok(transcribe::TranscriptionOutcome::Detected {
                    segments,
                    text,
                    language,
                }) => Transcript {
                    text,
                    language,
                    segments,
                    model_key: model_key.as_str().to_string(),
                    engine: format!("FunASR ModelScope {}", env!("CARGO_PKG_VERSION")),
                    generated_at: now_rfc3339(),
                },
                Err(message) => {
                    fail_transcription_task(&app, &store, &recording_id, message);
                    return;
                }
            };

            // Atomically persist the transcript and retain the user-owned audio.
            let mut task = match store.load_task_record(&recording_id) {
                Some(task) if task.state == TaskState::TranscribingLocal => task,
                None => return,
                Some(_) => return,
            };
            if let Err(message) = network::persist_transcript(&store, &mut task, transcript) {
                task.state = TaskState::TranscriptionFailed;
                task.error = Some(message.clone());
                let _ = store.save_task_progress(&task);
                emit_state(&app, &store);
                notifications::notify_transcript_failed(&app, &recording_id);
                return;
            }
            task.state = TaskState::TranscriptReady;
            if task.duration_ms.is_none() {
                task.duration_ms = task
                    .transcript
                    .as_ref()
                    .and_then(|value| value.segments.iter().map(|segment| segment.end_ms).max());
            }
            task.error = None;
            task.updated_at = now_rfc3339();
            let _ = store.save_task_progress(&task);
            emit_state(&app, &store);
            handoff_completed_transcript(&app, &recording_id);
        })
    {
        ACTIVE_TRANSCRIPTIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&failure_recording_id);
        PENDING_TRANSCRIPTION_RESTARTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&failure_recording_id);
        fail_transcription_task(
            &failure_app,
            &failure_store,
            &failure_recording_id,
            format!("无法启动本地转写线程: {error}"),
        );
    }
}

fn fail_transcription_task(
    app: &AppHandle,
    store: &MeetingStore,
    recording_id: &str,
    message: String,
) {
    let Some(mut task) = store.load_task_record(recording_id) else {
        return;
    };
    if task.state != TaskState::TranscribingLocal {
        return;
    }
    task.state = TaskState::TranscriptionFailed;
    task.error = Some(message.clone());
    let _ = store.save_task_progress(&task);
    emit_state(app, store);
    notifications::notify_transcript_failed(app, recording_id);
    crate::logging::write_app_log(
        app,
        "error",
        "meeting-transcribe",
        "local transcription failed",
        Some(&serde_json::json!({ "recordingId": recording_id, "reason": message })),
    );
}

fn register_transcription(recording_id: &str) -> bool {
    let inserted = ACTIVE_TRANSCRIPTIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(recording_id.to_string());
    if !inserted {
        PENDING_TRANSCRIPTION_RESTARTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(recording_id.to_string());
    }
    inserted
}

struct TranscriptionRegistration {
    app: AppHandle,
    recording_id: String,
    store: MeetingStore,
}

impl TranscriptionRegistration {
    fn new(app: AppHandle, store: MeetingStore, recording_id: String) -> Self {
        Self {
            app,
            recording_id,
            store,
        }
    }
}

impl Drop for TranscriptionRegistration {
    fn drop(&mut self) {
        ACTIVE_TRANSCRIPTIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.recording_id);
        let should_restart = PENDING_TRANSCRIPTION_RESTARTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.recording_id)
            && self
                .store
                .load_task_record(&self.recording_id)
                .is_some_and(|task| task.state == TaskState::TranscribingLocal);
        if should_restart {
            spawn_transcription(
                self.app.clone(),
                self.store.clone_for_task(),
                self.recording_id.clone(),
            );
        }
    }
}

fn prepare_transcription_audio(
    store: &MeetingStore,
    task: &mut MeetingTask,
) -> Result<PathBuf, String> {
    let source = task
        .audio_path
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .ok_or_else(|| "录音文件缺失，无法转写".to_string())?;
    if !has_txt_extension(&source) {
        return Ok(source);
    }
    let (recovered, duration_ms, audio_bytes) =
        recover_txt_audio_file(&source, &task.recording_id)?;
    if !store
        .load_task_record(&task.recording_id)
        .is_some_and(|current| current.state == TaskState::TranscribingLocal)
    {
        fs::remove_file(&recovered).ok();
        return Err("本地转写已停止".to_string());
    }
    task.audio_path = Some(recovered.to_string_lossy().into_owned());
    task.audio_bytes = Some(audio_bytes);
    task.duration_ms = Some(duration_ms);
    task.updated_at = now_rfc3339();
    store.save_task_progress(task)?;
    if task.audio_file_owned {
        fs::remove_file(&source).map_err(|error| format!("无法移除错误的 TXT 录音: {error}"))?;
    }
    Ok(recovered)
}

fn recover_txt_audio_file(
    source: &std::path::Path,
    recording_id: &str,
) -> Result<(PathBuf, u64, u64), String> {
    let (_, duration_ms) = audio::read_wav_i16(source)
        .map_err(|message| format!("TXT 录音无法恢复为 WAV: {message}"))?;
    let recovered = available_recovered_wav_path(source, recording_id);
    let audio_bytes =
        fs::copy(source, &recovered).map_err(|error| format!("无法恢复 WAV 录音: {error}"))?;
    Ok((recovered, duration_ms, audio_bytes))
}

fn has_txt_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("txt"))
}

fn available_recovered_wav_path(source: &std::path::Path, recording_id: &str) -> PathBuf {
    let preferred = source.with_extension("wav");
    if !preferred.exists() {
        return preferred;
    }
    let recovered = source.with_file_name(format!("{recording_id}-recovered.wav"));
    if !recovered.exists() {
        return recovered;
    }
    for index in 2..=10_000 {
        let candidate = source.with_file_name(format!("{recording_id}-recovered-{index}.wav"));
        if !candidate.exists() {
            return candidate;
        }
    }
    source.with_file_name(format!("{recording_id}-recovered-{}.wav", unix_millis()))
}

fn finish_without_audio(app: &AppHandle, store: &MeetingStore, recording_id: &str) {
    let Some(mut task) = store.load_task_record(recording_id) else {
        return;
    };
    task.state = TaskState::NoAudioDetected;
    task.transcript = None;
    task.transcript_path = None;
    task.error = None;
    task.updated_at = now_rfc3339();
    if let Err(message) = store.save_task_progress(&task) {
        crate::logging::write_app_log(
            app,
            "error",
            "meeting-transcribe",
            "no-audio state could not be persisted; notification skipped",
            Some(&serde_json::json!({ "recordingId": recording_id, "reason": message })),
        );
        return;
    }
    emit_state(app, store);
    crate::logging::write_app_log(
        app,
        "info",
        "meeting-transcribe",
        "no audio detected; transcription notification skipped",
        Some(&serde_json::json!({ "recordingId": recording_id })),
    );
}

fn generate_notes(app: AppHandle, store: MeetingStore, recording_id: String) -> Result<(), String> {
    let mut task = store
        .load_task_record(&recording_id)
        .ok_or_else(|| "没有找到本地转写记录".to_string())?;
    if !matches!(
        task.state,
        TaskState::TranscriptReady | TaskState::WaitingForNetwork | TaskState::NotesFailed
    ) {
        return Err("当前记录不能生成会议纪要".to_string());
    }
    task.state = TaskState::GeneratingNotes;
    task.error = None;
    task.updated_at = now_rfc3339();
    store.save_task_progress(&task)?;
    emit_state(&app, &store);
    tauri::async_runtime::spawn(async move {
        if let Err(message) = network::run_submission_pipeline(&app, &store, &recording_id).await {
            if let Some(mut task) = store.load_task_record(&recording_id) {
                if task.state == TaskState::GeneratingNotes {
                    task.state = TaskState::WaitingForNetwork;
                    task.error = Some(message);
                    let _ = store.save_task_progress(&task);
                    emit_state(&app, &store);
                }
            }
        }
    });
    Ok(())
}

fn spawn_overlay_updater(app: AppHandle, recording_id: String) {
    tauri::async_runtime::spawn(async move {
        loop {
            let Some(window) = app.get_webview_window(overlay::OVERLAY_LABEL) else {
                return;
            };
            let status = {
                let state = app.state::<MeetingManagerState>();
                let guard = state.recorder.lock().expect("recorder poisoned");
                match guard.as_ref() {
                    Some(recorder) => LiveCaptureStatus::from_shared(&recorder.shared),
                    None => return,
                }
            };
            let state = app.state::<MeetingManagerState>();
            if !state.store.load_task().is_some_and(|task| {
                task.recording_id == recording_id && task.state == TaskState::Recording
            }) {
                return;
            }
            let Some(task) = state.store.load_task() else {
                return;
            };
            overlay::update_overlay(&window, build_overlay_state(&state, &task, status));
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_allowed_window(window: &WebviewWindow) -> Result<(), String> {
    if overlay::is_overlay_window(window) {
        return Ok(());
    }
    let url = window.url().map_err(|error| error.to_string())?;
    if crate::web::is_allowed_web_origin(&url) {
        Ok(())
    } else {
        Err("origin is not allowed to access meeting features".to_string())
    }
}

fn generate_recording_id() -> String {
    let counter = RECORDING_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    let process = std::process::id();
    format!("rec-{}-{}-{}", unix_millis(), process, counter)
}

fn open_permission_settings(_app: &AppHandle, permission: &str) -> Result<(), String> {
    let url = match permission {
        "microphone" => {
            if cfg!(target_os = "macos") {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            } else {
                "ms-settings:privacy-microphone"
            }
        }
        "system_audio" => {
            if cfg!(target_os = "macos") {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            } else {
                "ms-settings:sound"
            }
        }
        _ => return Err("未知的权限类型".to_string()),
    };
    let parsed = url
        .parse::<tauri::Url>()
        .map_err(|_| "无法打开系统设置".to_string())?;
    crate::platform::open_external_url(&parsed)
}

#[cfg(test)]
mod tests {
    use super::permissions::PermissionAccess;
    use super::{
        apply_transcript_project, can_attempt_recording_without_permission_request,
        claim_transcript_asset_upload, complete_transcript_asset_upload,
        has_desktop_update_blocking_state, has_site_switch_blocking_state, imported_audio_task,
        is_valid_session_id, next_transcription_pause_state, normalize_chat_handoff_state,
        normalize_recording_projects, normalize_transcript_asset_upload_state,
        recover_txt_audio_file, rename_transcript_text_file, should_automatically_generate_notes,
        site_switch_is_blocked_by_state, task_with_state, MeetingCapabilities, MeetingSettings,
        MeetingTask, TaskState, Transcript, TranscriptAssetState,
    };
    use crate::meeting::audio::WavWriter;
    use std::fs;

    #[test]
    fn snapshot_capabilities_are_explicit_even_for_local_versioned_builds() {
        let value = serde_json::to_value(MeetingCapabilities::current()).unwrap();

        assert_eq!(value["transcriptionTaskControls"], true);
        assert_eq!(value["transcriptProjectAssets"], true);
    }

    #[test]
    fn quick_recording_requests_any_permission_that_is_not_preflight_granted() {
        assert!(can_attempt_recording_without_permission_request(
            PermissionAccess::Granted,
            PermissionAccess::Granted
        ));
        assert!(!can_attempt_recording_without_permission_request(
            PermissionAccess::Granted,
            PermissionAccess::Unknown
        ));
        assert!(!can_attempt_recording_without_permission_request(
            PermissionAccess::Unknown,
            PermissionAccess::Granted
        ));
        assert!(!can_attempt_recording_without_permission_request(
            PermissionAccess::Granted,
            PermissionAccess::Denied
        ));
    }

    #[test]
    fn legacy_server_states_return_to_local_transcript_ready() {
        for state in [
            TaskState::WaitingForNetwork,
            TaskState::GeneratingNotes,
            TaskState::Ready,
            TaskState::NotesFailed,
        ] {
            let mut task = MeetingTask::new("recording-1".to_string(), "zh".to_string());
            task.state = state;
            task.error = Some("legacy server error".to_string());
            task.next_retry_at = Some("2026-08-04T14:00:00+08:00".to_string());
            task.submission.last_error = Some("database unavailable".to_string());
            task.transcript = Some(Transcript {
                text: "会议转写".to_string(),
                language: "zh".to_string(),
                segments: Vec::new(),
                model_key: "small".to_string(),
                engine: "whisper.cpp".to_string(),
                generated_at: "2026-08-04T14:00:00+08:00".to_string(),
            });

            assert!(normalize_chat_handoff_state(&mut task));
            assert_eq!(task.state, TaskState::TranscriptReady);
            assert!(task.error.is_none());
            assert!(task.next_retry_at.is_none());
            assert!(task.submission.last_error.is_none());
        }
    }

    #[test]
    fn active_local_task_is_not_rewritten() {
        let mut task = MeetingTask::new("recording-1".to_string(), "zh".to_string());
        task.state = TaskState::TranscribingLocal;
        assert!(!normalize_chat_handoff_state(&mut task));
        assert_eq!(task.state, TaskState::TranscribingLocal);
    }

    #[test]
    fn transcript_project_upload_progress_is_persistable_and_idempotent() {
        let mut task = task_with_transcript();
        apply_transcript_project(&mut task, Some(("101".to_string(), "桌面迭代".to_string())))
            .unwrap();
        assert_eq!(task.transcript_asset_state, TranscriptAssetState::Pending);
        assert!(claim_transcript_asset_upload(&mut task, "101").unwrap());
        complete_transcript_asset_upload(
            &mut task,
            ("101".to_string(), "201".to_string(), "301".to_string()),
        )
        .unwrap();
        assert_eq!(task.transcript_asset_state, TranscriptAssetState::Saved);
        assert_eq!(task.transcript_asset_file_id.as_deref(), Some("201"));
        assert!(!claim_transcript_asset_upload(&mut task, "101").unwrap());
    }

    #[test]
    fn interrupted_transcript_asset_upload_returns_to_pending() {
        let mut task = task_with_transcript();
        task.transcript_asset_state = TranscriptAssetState::Uploading;
        assert!(normalize_transcript_asset_upload_state(&mut task));
        assert_eq!(task.transcript_asset_state, TranscriptAssetState::Pending);
    }

    fn task_with_transcript() -> MeetingTask {
        let mut task = MeetingTask::new("recording-asset".to_string(), "zh".to_string());
        task.transcript = Some(Transcript {
            text: "会议转写".to_string(),
            language: "zh".to_string(),
            segments: Vec::new(),
            model_key: "small".to_string(),
            engine: "FunASR".to_string(),
            generated_at: "2026-08-24T10:00:00+08:00".to_string(),
        });
        task
    }

    #[test]
    fn automatic_notes_handoff_respects_the_saved_preference() {
        let mut settings = MeetingSettings::default();
        assert!(!should_automatically_generate_notes(&settings));

        settings.auto_generate_notes_enabled = true;
        assert!(should_automatically_generate_notes(&settings));
    }

    #[test]
    fn site_switch_guard_allows_local_transcription_but_blocks_recording_and_notes() {
        for state in [
            TaskState::Checking,
            TaskState::Recording,
            TaskState::Finalizing,
            TaskState::GeneratingNotes,
        ] {
            assert!(site_switch_is_blocked_by_state(state));
        }
        assert!(!site_switch_is_blocked_by_state(
            TaskState::TranscribingLocal
        ));
        assert!(!site_switch_is_blocked_by_state(TaskState::TranscriptReady));
        assert!(!site_switch_is_blocked_by_state(TaskState::Ready));

        let mut asset_upload = task_with_state(TaskState::TranscriptReady);
        asset_upload.transcript_asset_state = TranscriptAssetState::Pending;
        assert!(has_site_switch_blocking_state(None, vec![asset_upload]));
    }

    #[test]
    fn desktop_update_guard_blocks_background_task_records() {
        assert!(has_desktop_update_blocking_state(
            None,
            vec![task_with_state(TaskState::TranscribingLocal)],
        ));
        assert!(!has_desktop_update_blocking_state(
            None,
            vec![task_with_state(TaskState::TranscriptReady)],
        ));
    }

    #[test]
    fn imported_audio_task_references_the_selected_file_without_copying_it() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-reference-import-{}",
            super::unix_millis()
        ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("产品周会.mp3");
        fs::write(&source, b"audio-data").unwrap();

        let task = imported_audio_task(&source).unwrap();
        let files = fs::read_dir(&root).unwrap().collect::<Vec<_>>();

        assert_eq!(
            task.audio_path.as_deref(),
            source.canonicalize().unwrap().to_str()
        );
        assert_eq!(task.audio_bytes, Some(10));
        assert!(!task.audio_file_owned);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_ref().unwrap().path(), source);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transcript_title_rename_never_renames_the_audio_file() {
        let root =
            std::env::temp_dir().join(format!("snack-transcript-title-{}", super::unix_millis()));
        fs::create_dir_all(&root).unwrap();
        let audio_path = root.join("recording.wav");
        let transcript_path = root.join("before.txt");
        fs::write(&audio_path, b"audio").unwrap();
        fs::write(&transcript_path, b"transcript").unwrap();
        let mut task = MeetingTask::new("rec-title".to_string(), "zh".to_string());
        task.audio_path = Some(audio_path.to_string_lossy().into_owned());
        task.transcript_path = Some(transcript_path.to_string_lossy().into_owned());

        rename_transcript_text_file(&mut task, "after.txt").unwrap();

        assert_eq!(fs::read(&audio_path).unwrap(), b"audio");
        assert!(!transcript_path.exists());
        assert_eq!(fs::read(root.join("after.txt")).unwrap(), b"transcript");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mislabeled_txt_recording_is_recovered_as_wav() {
        let root =
            std::env::temp_dir().join(format!("snack-txt-audio-recovery-{}", super::unix_millis()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("产品周会.txt");
        let mut writer = WavWriter::create(&source).unwrap();
        writer.write_samples(&vec![120; 16_000]).unwrap();
        writer.finalize().unwrap();

        let (recovered, duration_ms, audio_bytes) =
            recover_txt_audio_file(&source, "rec-recovered").unwrap();

        assert_eq!(recovered, root.join("产品周会.wav"));
        assert_eq!(duration_ms, 1_000);
        assert_eq!(audio_bytes, fs::metadata(&recovered).unwrap().len());
        assert!(crate::meeting::audio::read_wav_i16(&recovered).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transcription_can_pause_resume_and_reject_completed_tasks() {
        assert_eq!(
            next_transcription_pause_state(TaskState::TranscribingLocal, true).unwrap(),
            TaskState::TranscriptionPaused
        );
        assert_eq!(
            next_transcription_pause_state(TaskState::TranscriptionPaused, false).unwrap(),
            TaskState::TranscribingLocal
        );
        assert!(next_transcription_pause_state(TaskState::TranscriptReady, true).is_err());
    }

    #[test]
    fn meeting_notification_accepts_only_numeric_session_ids() {
        assert!(is_valid_session_id("343806935252082688"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("session-1"));
        assert!(!is_valid_session_id("123456789012345678901"));
    }

    #[test]
    fn recording_projects_are_trimmed_and_deduplicated() {
        let projects = normalize_recording_projects(vec![
            super::overlay::RecordingProjectOption {
                project_id: " 101 ".to_string(),
                project_name: " 产品项目 ".to_string(),
            },
            super::overlay::RecordingProjectOption {
                project_id: "101".to_string(),
                project_name: "重复项目".to_string(),
            },
        ])
        .unwrap();

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].project_id, "101");
        assert_eq!(projects[0].project_name, "产品项目");
    }

    #[test]
    fn recording_projects_accept_an_empty_list() {
        assert!(normalize_recording_projects(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn recording_projects_reject_invalid_identity_or_name() {
        let invalid_id = vec![super::overlay::RecordingProjectOption {
            project_id: "project-1".to_string(),
            project_name: "产品项目".to_string(),
        }];
        let missing_name = vec![super::overlay::RecordingProjectOption {
            project_id: "101".to_string(),
            project_name: "  ".to_string(),
        }];

        assert!(normalize_recording_projects(invalid_id).is_err());
        assert!(normalize_recording_projects(missing_name).is_err());
    }
}
