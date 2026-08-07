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
pub(crate) mod quick_access;
mod reminder;
#[cfg(target_os = "macos")]
mod reminder_macos;
mod state;
mod transcribe;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use capture::{check_capture_permissions, LiveCaptureStatus, Recorder};
use catalog::{catalog, platform_label, CatalogModel};
use install::InstallManager;
use permissions::{request_mac_permissions, PermissionAccess};
use state::{
    normalize_transcript_file_name, now_rfc3339, unix_millis, MeetingSettings, MeetingStore,
    MeetingTask, ResourceState, ResourceStatus, TaskState, Transcript,
};

const STATE_EVENT: &str = "meeting-state";
const TRANSCRIPTION_PROGRESS_EVENT: &str = "meeting-transcription-progress";
const MIN_RECORDING_DISK_BYTES: u64 = 200 * 1024 * 1024; // 200 MB headroom

pub(crate) struct MeetingManagerState {
    pub(crate) store: MeetingStore,
    pub(crate) manager: Arc<InstallManager>,
    pub(crate) recorder: Mutex<Option<Recorder>>,
    pub(crate) reminder: reminder::RecordingReminderMonitor,
}

static RECORDING_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    platform: String,
    resource: ResourceStatus,
    task: Option<MeetingTask>,
    tasks: Vec<MeetingTask>,
    settings: MeetingSettings,
    catalog: Vec<CatalogInfo>,
    permissions: Option<PermissionStatus>,
    disk_free_bytes: Option<u64>,
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
        reminder,
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
        if normalize_chat_handoff_state(&mut task) {
            let _ = store.save_task(&task);
        }
        let _ = store.save_task_record(&task);
    }
    for mut task in store.load_task_records() {
        if normalize_chat_handoff_state(&mut task) {
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
    state.store.prune_recording_audio(10)?;
    state.store.clear_task_records()?;
    if !current_task_active {
        overlay::hide_overlay(&app);
    }
    emit_state(&app, &state.store);
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
    let Some(file) = rfd::AsyncFileDialog::new()
        .set_title("选择需要本地转写的音频")
        .add_filter(
            "音频文件",
            &["wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "wma"],
        )
        .pick_file()
        .await
    else {
        return Ok(None);
    };
    let source = file.path();
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|value| state::is_supported_audio_extension(value))
        .ok_or_else(|| "请选择 WAV、MP3、M4A、AAC、FLAC、OGG、OPUS 或 WMA 音频文件".to_string())?;
    let source_bytes = fs::metadata(source)
        .map_err(|error| format!("无法读取所选音频: {error}"))?
        .len();
    match install::free_disk_bytes(&store) {
        Ok(free) if free < source_bytes.saturating_add(MIN_RECORDING_DISK_BYTES) => {
            return Err("磁盘空间不足，无法导入音频".to_string());
        }
        Err(message) => return Err(format!("无法检查磁盘空间: {message}")),
        _ => {}
    }

    let audio_directory_created = store.ensure_audio_directory()?;
    let recording_id = generate_recording_id();
    let destination = store.imported_audio_path(&recording_id, &extension);
    if let Err(error) = fs::copy(source, &destination) {
        cleanup_failed_audio_import(&store, &destination, audio_directory_created);
        return Err(format!("无法导入所选音频: {error}"));
    }

    let now = now_rfc3339();
    let mut task = MeetingTask::new(recording_id.clone(), "zh".to_string());
    task.display_name = source
        .file_stem()
        .and_then(|name| normalize_transcript_file_name(&name.to_string_lossy()).ok());
    task.state = TaskState::TranscribingLocal;
    task.started_at = Some(now.clone());
    task.ended_at = Some(now);
    task.audio_path = Some(destination.to_string_lossy().into_owned());
    task.audio_bytes = Some(source_bytes);
    if let Err(error) = store.save_task_record(&task) {
        cleanup_failed_audio_import(&store, &destination, audio_directory_created);
        return Err(error);
    }
    if let Err(error) = store.prune_recording_audio(10) {
        crate::logging::write_app_log(
            &app,
            "warn",
            "meeting-import",
            "old meeting audio could not be pruned",
            Some(&serde_json::json!({ "error": error })),
        );
    }
    emit_state(&app, &store);
    spawn_transcription(app, store, recording_id.clone());
    Ok(Some(recording_id))
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
            overlay::show_overlay(
                &app,
                overlay::OverlayState::recording(recording_id.clone(), 0, false, false),
            )?;
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
    if let Some(mut task) = store.load_task() {
        let recording_id = task.recording_id.clone();
        task.state = TaskState::FinalizeFailed;
        task.error = Some(message.clone());
        let _ = store.save_task(&task);
        emit_state(app, store);
        notifications::notify_transcript_failed(app, &recording_id);
    }
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
        .map(|recorder| LiveCaptureStatus::from_shared(&recorder.shared, unix_millis())))
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

fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 20
        && session_id.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn open_notes_from_notification(
    app: &AppHandle,
    recording_id: &str,
) -> Result<(), String> {
    open_notes_in_chat(app, recording_id, true)
}

fn handoff_completed_transcript(app: &AppHandle, recording_id: &str) {
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
                "automatic notes handoff failed; notification fallback is available",
                Some(&serde_json::json!({ "recordingId": recording_id, "reason": error })),
            );
            notifications::notify_transcript_ready(app, recording_id);
        }
    }
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
        )?;
    } else {
        crate::record_import::open_prefill_with_attachment(
            app,
            settings.notes_prompt,
            transcript_name,
            transcript_text,
            false,
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

    if let Some(current_path) = task.transcript_path.as_deref().map(PathBuf::from) {
        let next_path = current_path.with_file_name(&display_name);
        if next_path != current_path {
            if next_path.exists() {
                return Err("同名转写文件已存在，请换一个名称".to_string());
            }
            fs::rename(&current_path, &next_path)
                .map_err(|error| format!("无法重命名本地转写文件: {error}"))?;
            task.transcript_path = Some(next_path.to_string_lossy().into_owned());
        }
    }
    task.display_name = Some(display_name);
    task.updated_at = now_rfc3339();
    store.save_task_progress(&task)?;
    emit_state(&app, &store);
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
    std::thread::Builder::new()
        .name("snack-transcribe".to_string())
        .spawn(move || {
            let resource = store.load_resource();
            let model_key = match resource
                .model_key
                .as_deref()
                .and_then(catalog::ModelKey::parse)
            {
                Some(key) => key,
                None => {
                    let mut task = store.load_task_record(&recording_id).unwrap();
                    let message = "本地模型未安装".to_string();
                    task.state = TaskState::TranscriptionFailed;
                    task.error = Some(message.clone());
                    let _ = store.save_task_progress(&task);
                    emit_state(&app, &store);
                    notifications::notify_transcript_failed(&app, &recording_id);
                    return;
                }
            };
            let model_path = match install::installed_model_path(&store, &resource) {
                Ok(path) => path,
                Err(message) => {
                    let mut task = store.load_task_record(&recording_id).unwrap();
                    task.state = TaskState::TranscriptionFailed;
                    task.error = Some(message.clone());
                    let _ = store.save_task_progress(&task);
                    emit_state(&app, &store);
                    notifications::notify_transcript_failed(&app, &recording_id);
                    return;
                }
            };
            let Some(model_dir) = model_path.parent().map(PathBuf::from) else {
                let mut task = store.load_task_record(&recording_id).unwrap();
                let message = "本地模型路径无效".to_string();
                task.state = TaskState::TranscriptionFailed;
                task.error = Some(message.clone());
                let _ = store.save_task_progress(&task);
                emit_state(&app, &store);
                notifications::notify_transcript_failed(&app, &task.recording_id);
                return;
            };

            let mut task = store.load_task_record(&recording_id).unwrap();
            if let Err(message) = store.ensure_transcript_output_directory(&task) {
                let message = format!("无法创建当天转写目录: {message}");
                task.state = TaskState::TranscriptionFailed;
                task.error = Some(message.clone());
                let _ = store.save_task_progress(&task);
                emit_state(&app, &store);
                notifications::notify_transcript_failed(&app, &recording_id);
                return;
            }
            let wav_path = match task.audio_path.clone().map(PathBuf::from) {
                Some(path) if path.exists() => path,
                _ => {
                    let message = "录音文件缺失，无法转写".to_string();
                    task.state = TaskState::TranscriptionFailed;
                    task.error = Some(message.clone());
                    let _ = store.save_task_progress(&task);
                    emit_state(&app, &store);
                    notifications::notify_transcript_failed(&app, &recording_id);
                    return;
                }
            };
            let language = task.language.clone();
            drop(task);

            let app_for_progress = app.clone();
            let store_for_progress = store.clone_for_task();
            let recording_id_for_progress = recording_id.clone();
            let outcome = transcribe::transcribe_file(
                model_key,
                &model_dir,
                &wav_path,
                &language,
                move |progress| {
                    let _ = app_for_progress.emit(
                        TRANSCRIPTION_PROGRESS_EVENT,
                        serde_json::json!({
                            "recordingId": recording_id_for_progress,
                            "percent": progress.percent,
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
                Ok(outcome) => Transcript {
                    text: outcome.text,
                    language: outcome.language,
                    segments: outcome.segments,
                    model_key: model_key.as_str().to_string(),
                    engine: format!("FunASR ModelScope {}", env!("CARGO_PKG_VERSION")),
                    generated_at: now_rfc3339(),
                },
                Err(message) => {
                    let mut task = store.load_task_record(&recording_id).unwrap();
                    task.state = TaskState::TranscriptionFailed;
                    task.error = Some(message.clone());
                    let _ = store.save_task_progress(&task);
                    emit_state(&app, &store);
                    notifications::notify_transcript_failed(&app, &recording_id);
                    crate::logging::write_app_log(
                        &app,
                        "error",
                        "meeting-transcribe",
                        "local transcription failed",
                        Some(
                            &serde_json::json!({ "recordingId": recording_id, "reason": message }),
                        ),
                    );
                    return;
                }
            };

            // Atomically persist the transcript and retain the user-owned audio.
            let mut task = match store.load_task_record(&recording_id) {
                Some(task) => task,
                None => return,
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
        .expect("failed to spawn transcription thread");
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
                    Some(recorder) => {
                        LiveCaptureStatus::from_shared(&recorder.shared, unix_millis())
                    }
                    None => return,
                }
            };
            let state = app.state::<MeetingManagerState>();
            if !state.store.load_task().is_some_and(|task| {
                task.recording_id == recording_id && task.state == TaskState::Recording
            }) {
                return;
            }
            overlay::update_overlay(
                &window,
                overlay::OverlayState::recording(
                    recording_id.clone(),
                    status.elapsed_ms,
                    status.mic_active,
                    status.system_audio_active,
                ),
            );
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

fn cleanup_failed_audio_import(
    store: &MeetingStore,
    destination: &std::path::Path,
    audio_directory_created: bool,
) {
    let _ = fs::remove_file(destination);
    if audio_directory_created {
        let _ = fs::remove_dir(store.audio_directory());
    }
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
        can_attempt_recording_without_permission_request, is_valid_session_id,
        normalize_chat_handoff_state, MeetingTask, TaskState, Transcript,
    };

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
    fn meeting_notification_accepts_only_numeric_session_ids() {
        assert!(is_valid_session_id("343806935252082688"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("session-1"));
        assert!(!is_valid_session_id("123456789012345678901"));
    }
}
