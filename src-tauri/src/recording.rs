use std::{
    ffi::{c_char, CStr},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use tauri::{
    path::BaseDirectory, AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_dialog::{DialogExt, FilePath};
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use tauri_plugin_opener::OpenerExt;
use uuid::Uuid;

use crate::web::is_allowed_web_origin;

const DEFAULT_RECORDING_SHORTCUT: &str = "CommandOrControl+R";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TranscriptionRequest {
    language: String,
    mode: String,
    path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingPreferencesRequest {
    language: String,
    mode: String,
    organize_by_date: bool,
    output_directory: String,
}

impl Default for RecordingPreferencesRequest {
    fn default() -> Self {
        Self {
            language: "zh".into(),
            mode: "fast".into(),
            organize_by_date: true,
            output_directory: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingResourceStatus {
    available: bool,
    detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingStatus {
    active: bool,
    elapsed_ms: u64,
    started_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopRecording {
    created_at: String,
    duration_ms: u64,
    file_name: String,
    file_size_bytes: u64,
    id: String,
    path: String,
    transcript_file_name: Option<String>,
    transcript_path: Option<String>,
    transcript_text: Option<String>,
    transcription_error: Option<String>,
    transcription_progress: Option<f32>,
    transcription_state: TranscriptionState,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TranscriptionState {
    Pending,
    Processing,
    Completed,
    Failed,
}

struct ActiveRecording {
    base_path: PathBuf,
    started_at: DateTime<Utc>,
    started_instant: Instant,
}

pub(crate) struct RecordingStore {
    active: Mutex<Option<ActiveRecording>>,
    directory: PathBuf,
    metadata: Mutex<()>,
    metadata_path: PathBuf,
    preferences: Mutex<RecordingPreferencesRequest>,
}

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("recordings");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    app.manage(RecordingStore {
        active: Mutex::new(None),
        metadata: Mutex::new(()),
        metadata_path: directory.join("recordings.json"),
        preferences: Mutex::new(RecordingPreferencesRequest::default()),
        directory,
    });
    Ok(())
}

#[tauri::command]
pub(crate) fn get_system_audio_recording_status(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<RecordingStatus, String> {
    require_allowed_origin(&window)?;
    let store = app.state::<RecordingStore>();
    let active = store.active.lock().map_err(|_| "录音状态不可用")?;
    Ok(match active.as_ref() {
        Some(item) => RecordingStatus {
            active: true,
            elapsed_ms: item.started_instant.elapsed().as_millis() as u64,
            started_at: Some(item.started_at.to_rfc3339()),
        },
        None => RecordingStatus {
            active: false,
            elapsed_ms: 0,
            started_at: None,
        },
    })
}

#[tauri::command]
pub(crate) async fn start_system_audio_recording(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<RecordingStatus, String> {
    require_allowed_origin(&window)?;
    let store = app.state::<RecordingStore>();
    let base_path = recording_output_directory(&store)?.join(Uuid::new_v4().to_string());
    {
        let mut active = store.active.lock().map_err(|_| "录音状态不可用")?;
        if active.is_some() {
            return Err("已有录音正在进行".into());
        }
        *active = Some(ActiveRecording {
            base_path: base_path.clone(),
            started_at: Utc::now(),
            started_instant: Instant::now(),
        });
    }
    if let Err(error) = start_native_recording(base_path.clone()).await {
        *store.active.lock().map_err(|_| "录音状态不可用")? = None;
        return Err(error);
    }
    let started_at = Utc::now();
    *store.active.lock().map_err(|_| "录音状态不可用")? = Some(ActiveRecording {
        base_path,
        started_at,
        started_instant: Instant::now(),
    });
    show_recording_window(&app, &window);
    set_recording_tray_status(&app, true);
    Ok(RecordingStatus {
        active: true,
        elapsed_ms: 0,
        started_at: Some(started_at.to_rfc3339()),
    })
}

#[tauri::command]
pub(crate) async fn stop_system_audio_recording(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<DesktopRecording, String> {
    require_allowed_origin(&window)?;
    let store = app.state::<RecordingStore>();
    let active = store
        .active
        .lock()
        .map_err(|_| "录音状态不可用")?
        .take()
        .ok_or("当前没有正在进行的录音")?;
    let path = stop_native_recording().await.map_err(|error| {
        cleanup_recording_parts(&active.base_path);
        error
    })?;
    hide_recording_window(&app);
    set_recording_tray_status(&app, false);
    let recording = recording_from_path(path, active.started_at, active.started_instant.elapsed())?;
    let _metadata = store.metadata.lock().map_err(|_| "录音索引不可用")?;
    let mut recordings = load_recordings(&store.metadata_path)?;
    recordings.insert(0, recording.clone());
    persist_recordings(&store.metadata_path, &recordings)?;
    Ok(recording)
}

#[tauri::command]
pub(crate) fn list_system_audio_recordings(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<Vec<DesktopRecording>, String> {
    require_allowed_origin(&window)?;
    let store = app.state::<RecordingStore>();
    let _metadata = store.metadata.lock().map_err(|_| "录音索引不可用")?;
    load_recordings(&store.metadata_path)
}

#[tauri::command]
pub(crate) fn choose_and_import_recording(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<Option<DesktopRecording>, String> {
    require_allowed_origin(&window)?;
    let selected = app
        .dialog()
        .file()
        .add_filter(
            "音频或视频",
            &["wav", "m4a", "mp3", "mp4", "mov", "aac", "flac"],
        )
        .blocking_pick_file();
    let Some(source) = selected.and_then(file_path_to_path) else {
        return Ok(None);
    };
    let store = app.state::<RecordingStore>();
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("audio");
    let target =
        recording_output_directory(&store)?.join(format!("{}.{}", Uuid::new_v4(), extension));
    fs::copy(&source, &target).map_err(|error| format!("导入录音失败：{error}"))?;
    let mut recording = recording_from_path(target, Utc::now(), Duration::ZERO)?;
    recording.file_name = source
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("导入的录音")
        .to_string();
    let _metadata = store.metadata.lock().map_err(|_| "录音索引不可用")?;
    let mut recordings = load_recordings(&store.metadata_path)?;
    recordings.insert(0, recording.clone());
    persist_recordings(&store.metadata_path, &recordings)?;
    Ok(Some(recording))
}

#[tauri::command]
pub(crate) fn delete_system_audio_recording(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
) -> Result<bool, String> {
    require_allowed_origin(&window)?;
    let store = app.state::<RecordingStore>();
    let _metadata = store.metadata.lock().map_err(|_| "录音索引不可用")?;
    let mut recordings = load_recordings(&store.metadata_path)?;
    let Some(index) = recordings.iter().position(|item| item.id == id) else {
        return Ok(false);
    };
    let removed = recordings.remove(index);
    remove_recording_files(&removed);
    persist_recordings(&store.metadata_path, &recordings)?;
    Ok(true)
}

#[tauri::command]
pub(crate) fn delete_system_audio_recordings(
    window: WebviewWindow,
    app: AppHandle,
    ids: Vec<String>,
) -> Result<usize, String> {
    require_allowed_origin(&window)?;
    let store = app.state::<RecordingStore>();
    let _metadata = store.metadata.lock().map_err(|_| "录音索引不可用")?;
    let mut recordings = load_recordings(&store.metadata_path)?;
    let before = recordings.len();
    recordings.retain(|recording| {
        if !ids.contains(&recording.id) {
            return true;
        }
        remove_recording_files(recording);
        false
    });
    persist_recordings(&store.metadata_path, &recordings)?;
    Ok(before - recordings.len())
}

#[tauri::command]
pub(crate) fn open_local_recording_file(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
    kind: String,
) -> Result<(), String> {
    require_allowed_origin(&window)?;
    let store = app.state::<RecordingStore>();
    let recordings = load_recordings(&store.metadata_path)?;
    let recording = recordings
        .iter()
        .find(|recording| recording.id == id)
        .ok_or("未找到录音")?;
    let path = recording_file_path(recording, &kind)?;
    if kind == "audio" {
        app.asset_protocol_scope()
            .allow_file(&path)
            .map_err(|error| format!("无法读取本地录音：{error}"))?;
        return show_audio_player_window(&app, &window, recording);
    }
    app.opener()
        .open_path(path.to_string_lossy(), None::<&str>)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn choose_recording_output_directory(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<Option<String>, String> {
    require_allowed_origin(&window)?;
    Ok(app
        .dialog()
        .file()
        .blocking_pick_folder()
        .and_then(file_path_to_path)
        .map(|path| path.to_string_lossy().into_owned()))
}

#[tauri::command]
pub(crate) fn configure_recording_preferences(
    window: WebviewWindow,
    app: AppHandle,
    request: RecordingPreferencesRequest,
) -> Result<(), String> {
    require_allowed_origin(&window)?;
    let store = app.state::<RecordingStore>();
    if !request.output_directory.is_empty() {
        fs::create_dir_all(&request.output_directory).map_err(|error| error.to_string())?;
    }
    *store.preferences.lock().map_err(|_| "录音配置不可用")? = request;
    Ok(())
}

#[tauri::command]
pub(crate) fn detect_active_meeting_app(window: WebviewWindow) -> Result<Option<String>, String> {
    require_allowed_origin(&window)?;
    Ok(detect_meeting_process())
}

#[tauri::command]
pub(crate) fn configure_recording_shortcut(
    window: WebviewWindow,
    app: AppHandle,
    shortcut: String,
) -> Result<(), String> {
    require_allowed_origin(&window)?;
    let manager = app.global_shortcut();
    manager
        .unregister_all()
        .map_err(|error| error.to_string())?;
    let shortcut = shortcut
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .map_err(|error| error.to_string())?;
    manager
        .register(shortcut)
        .map_err(|error| error.to_string())
}

pub(crate) fn setup_default_shortcut(app: &AppHandle) {
    if let Err(error) = app.global_shortcut().register(DEFAULT_RECORDING_SHORTCUT) {
        crate::logging::write_app_log(
            app,
            "error",
            "recording",
            "Failed to register the default recording shortcut",
            Some(&serde_json::json!({
                "error": error.to_string(),
                "shortcut": DEFAULT_RECORDING_SHORTCUT,
            })),
        );
    }
}

pub(crate) fn start_recording_from_status_menu(app: AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let status = get_system_audio_recording_status(window.clone(), app.clone());
        if status.is_ok_and(|status| status.active) {
            show_recording_window(&app, &window);
            return;
        }
        if let Err(error) = start_system_audio_recording(window, app.clone()).await {
            crate::logging::write_app_log(&app, "error", "recording", &error, None);
        }
    });
}

pub(crate) fn toggle_recording_from_shortcut(app: AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let status = get_system_audio_recording_status(window.clone(), app.clone());
        if status.is_ok_and(|status| status.active) {
            if let Ok(recording) = stop_system_audio_recording(window.clone(), app.clone()).await {
                let preferences = app
                    .state::<RecordingStore>()
                    .preferences
                    .lock()
                    .map(|preferences| preferences.clone())
                    .unwrap_or_default();
                let request = TranscriptionRequest {
                    language: preferences.language,
                    mode: preferences.mode,
                    path: recording.path,
                };
                let _ = transcribe_local_audio(window, app, request).await;
            }
        } else {
            let _ = start_system_audio_recording(window, app).await;
        }
    });
}

#[tauri::command]
pub(crate) async fn transcribe_local_audio(
    window: WebviewWindow,
    app: AppHandle,
    request: TranscriptionRequest,
) -> Result<DesktopRecording, String> {
    require_allowed_origin(&window)?;
    if request.language != "zh" {
        crate::logging::write_app_log(
            &app,
            "info",
            "recording",
            "English transcription requested with the local bilingual model",
            None,
        );
    }
    let store = app.state::<RecordingStore>();
    let paths = resolve_resource_paths(&app)?;
    let (id, input, output, start_time) = begin_transcription(&store, &request.path)?;
    let mode = request.mode.clone();
    let transcription_output = output.clone();
    let result = tokio::task::spawn_blocking(move || {
        run_transcription(&paths, &input, &transcription_output, &start_time, &mode)
    })
    .await
    .map_err(|error| error.to_string())?;
    finish_transcription(&store, &id, output, result)
}

#[tauri::command]
pub(crate) fn get_recording_resource_status(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<RecordingResourceStatus, String> {
    require_allowed_origin(&window)?;
    match resolve_resource_paths(&app) {
        Ok(_) => Ok(RecordingResourceStatus {
            available: true,
            detail: "已连接 Snack Record 本地模型".into(),
        }),
        Err(detail) => Ok(RecordingResourceStatus {
            available: false,
            detail,
        }),
    }
}

#[tauri::command]
pub(crate) async fn install_recording_resources(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<RecordingResourceStatus, String> {
    require_allowed_origin(&window)?;
    let script = app
        .path()
        .resolve(
            "resources/install_recording_resources.sh",
            BaseDirectory::Resource,
        )
        .map_err(|error| error.to_string())?;
    let resource_directory = script
        .parent()
        .ok_or("无法定位录音资源安装程序")?
        .to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        Command::new("/bin/zsh")
            .arg(&script)
            .arg(&resource_directory)
            .output()
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    if !result.status.success() {
        return Err(command_error(&result.stderr, "本地转写资源安装失败"));
    }
    resolve_resource_paths(&app)?;
    Ok(RecordingResourceStatus {
        available: true,
        detail: "本地转写资源已安装".into(),
    })
}

struct ResourcePaths {
    models: PathBuf,
    python: PathBuf,
    script: PathBuf,
}

fn resolve_resource_paths(app: &AppHandle) -> Result<ResourcePaths, String> {
    let home = app.path().home_dir().map_err(|error| error.to_string())?;
    let root = home.join("Library/Application Support/Snack Record");
    let python = root.join("Runtime/venv/bin/python");
    let models = root.join("Models");
    let script = app
        .path()
        .resolve("resources/funasr_transcribe.py", BaseDirectory::Resource)
        .map_err(|error| error.to_string())?;
    if !python.is_file() || !models.join("models").is_dir() {
        return Err("尚未安装本地转写模型，可先继续录音和导入".into());
    }
    if !script.is_file() {
        return Err("桌面端缺少本地转写脚本".into());
    }
    Ok(ResourcePaths {
        models,
        python,
        script,
    })
}

fn run_transcription(
    paths: &ResourcePaths,
    input: &Path,
    output: &Path,
    start_time: &str,
    mode: &str,
) -> Result<String, String> {
    let result = Command::new(&paths.python)
        .arg(&paths.script)
        .arg(input)
        .arg(output)
        .arg(start_time)
        .arg("--mode")
        .arg(mode)
        .env("MODELSCOPE_CACHE", &paths.models)
        .output()
        .map_err(|error| error.to_string())?;
    if !result.status.success() {
        return Err(String::from_utf8_lossy(&result.stderr)
            .lines()
            .last()
            .unwrap_or("本地模型转写失败")
            .to_string());
    }
    fs::read_to_string(output).map_err(|error| error.to_string())
}

fn command_error(stderr: &[u8], fallback: &str) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .last()
        .unwrap_or(fallback)
        .to_string()
}

fn begin_transcription(
    store: &RecordingStore,
    request_path: &str,
) -> Result<(String, PathBuf, PathBuf, String), String> {
    let _metadata = store.metadata.lock().map_err(|_| "录音索引不可用")?;
    let mut recordings = load_recordings(&store.metadata_path)?;
    let index = recordings
        .iter()
        .position(|item| item.path == request_path)
        .ok_or("未找到录音")?;
    recordings[index].transcription_state = TranscriptionState::Processing;
    recordings[index].transcription_error = None;
    let id = recordings[index].id.clone();
    let input = PathBuf::from(&recordings[index].path);
    let output = input.with_extension("txt");
    let start_time = recordings[index].created_at.clone();
    persist_recordings(&store.metadata_path, &recordings)?;
    Ok((id, input, output, start_time))
}

fn finish_transcription(
    store: &RecordingStore,
    id: &str,
    output: PathBuf,
    result: Result<String, String>,
) -> Result<DesktopRecording, String> {
    let _metadata = store.metadata.lock().map_err(|_| "录音索引不可用")?;
    let mut recordings = load_recordings(&store.metadata_path)?;
    let Some(recording) = recordings.iter_mut().find(|item| item.id == id) else {
        let _ = fs::remove_file(output);
        return Err("录音已被删除".into());
    };
    apply_transcription_result(recording, output, result);
    let response = recording.clone();
    persist_recordings(&store.metadata_path, &recordings)?;
    match response.transcription_state {
        TranscriptionState::Completed => Ok(response),
        _ => Err(response
            .transcription_error
            .unwrap_or_else(|| "转写失败".into())),
    }
}

fn apply_transcription_result(
    recording: &mut DesktopRecording,
    output: PathBuf,
    result: Result<String, String>,
) {
    match result {
        Ok(text) => {
            recording.transcription_state = TranscriptionState::Completed;
            recording.transcription_progress = Some(100.0);
            let transcript_name = Path::new(&recording.file_name)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(&recording.file_name);
            recording.transcript_file_name = Some(format!("{transcript_name}.txt"));
            recording.transcript_path = Some(output.to_string_lossy().into_owned());
            recording.transcript_text = Some(text);
        }
        Err(error) => {
            recording.transcription_state = TranscriptionState::Failed;
            recording.transcription_error = Some(error);
            recording.transcription_progress = None;
        }
    }
}

fn recording_from_path(
    path: PathBuf,
    created_at: DateTime<Utc>,
    duration: Duration,
) -> Result<DesktopRecording, String> {
    let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
    let id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("recording")
        .to_string();
    let local: DateTime<Local> = created_at.into();
    Ok(DesktopRecording {
        created_at: created_at.to_rfc3339(),
        duration_ms: duration.as_millis() as u64,
        file_name: format!(
            "会议录音-{}{}",
            local.format("%Y%m%d-%H%M%S"),
            path.extension()
                .and_then(|value| value.to_str())
                .map(|value| format!(".{value}"))
                .unwrap_or_default()
        ),
        file_size_bytes: metadata.len(),
        id,
        path: path.to_string_lossy().into_owned(),
        transcript_file_name: None,
        transcript_path: None,
        transcript_text: None,
        transcription_error: None,
        transcription_progress: None,
        transcription_state: TranscriptionState::Pending,
    })
}

fn load_recordings(path: &Path) -> Result<Vec<DesktopRecording>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn persist_recordings(path: &Path, recordings: &[DesktopRecording]) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(recordings).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn recording_output_directory(store: &RecordingStore) -> Result<PathBuf, String> {
    let preferences = store
        .preferences
        .lock()
        .map_err(|_| "录音配置不可用")?
        .clone();
    let mut directory = if preferences.output_directory.trim().is_empty() {
        store.directory.clone()
    } else {
        PathBuf::from(preferences.output_directory)
    };
    if preferences.organize_by_date {
        directory = directory.join(Local::now().format("%Y-%m-%d").to_string());
    }
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory)
}

fn recording_file_path(recording: &DesktopRecording, kind: &str) -> Result<PathBuf, String> {
    let path = match kind {
        "audio" => Some(recording.path.as_str()),
        "transcript" => recording.transcript_path.as_deref(),
        _ => return Err("不支持的文件类型".into()),
    }
    .ok_or("文件尚未生成")?;
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err("本地文件不存在".into());
    }
    Ok(path)
}

fn remove_recording_files(recording: &DesktopRecording) {
    let _ = fs::remove_file(&recording.path);
    if let Some(path) = recording.transcript_path.as_deref() {
        let _ = fs::remove_file(path);
    }
}

fn show_recording_window(app: &AppHandle, source: &WebviewWindow) {
    if let Some(window) = app.get_webview_window("snack-recording") {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let Ok(mut url) = source.url() else {
        return;
    };
    url.set_path("/apps/snack-record");
    url.set_query(Some("floating=1"));
    let _ = WebviewWindowBuilder::new(app, "snack-recording", WebviewUrl::External(url))
        .title("Snack Record")
        .inner_size(380.0, 176.0)
        .min_inner_size(340.0, 176.0)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .build();
}

fn show_audio_player_window(
    app: &AppHandle,
    source: &WebviewWindow,
    recording: &DesktopRecording,
) -> Result<(), String> {
    let mut url = source.url().map_err(|error| error.to_string())?;
    url.set_path("/apps/snack-record");
    url.set_query(None);
    url.query_pairs_mut().append_pair("player", &recording.id);
    let title = format!("{} - 本地录音", recording.file_name);

    if let Some(window) = app.get_webview_window("snack-recording-player") {
        window.navigate(url).map_err(|error| error.to_string())?;
        window
            .set_title(&title)
            .map_err(|error| error.to_string())?;
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }

    let window =
        WebviewWindowBuilder::new(app, "snack-recording-player", WebviewUrl::External(url))
            .title(&title)
            .inner_size(760.0, 400.0)
            .min_inner_size(640.0, 360.0)
            .resizable(true)
            .decorations(false)
            .build()
            .map_err(|error| error.to_string())?;
    let _ = window.center();
    Ok(())
}

fn hide_recording_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("snack-recording") {
        let _ = window.hide();
    }
}

#[cfg(any(target_os = "macos", windows))]
fn set_recording_tray_status(app: &AppHandle, active: bool) {
    use crate::constants::{TRAY_ATTENTION_ICON, TRAY_DEFAULT_ICON, TRAY_ID};
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if active {
        let _ = tray.set_icon(Some(TRAY_ATTENTION_ICON));
        let _ = tray.set_tooltip(Some("Snack Record 正在录音"));
    } else {
        let _ = tray.set_icon(Some(TRAY_DEFAULT_ICON));
        let _ = tray.set_tooltip(Some("Snack"));
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn set_recording_tray_status(_app: &AppHandle, _active: bool) {}

#[cfg(target_os = "macos")]
fn detect_meeting_process() -> Option<String> {
    let output = Command::new("pgrep")
        .args(["-ifl", "WXWork|Lark|Feishu|wemeet|TencentMeeting"])
        .output()
        .ok()?;
    meeting_app_from_processes(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(target_os = "windows")]
fn detect_meeting_process() -> Option<String> {
    let output = Command::new("tasklist").output().ok()?;
    meeting_app_from_processes(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn detect_meeting_process() -> Option<String> {
    None
}

fn meeting_app_from_processes(processes: &str) -> Option<String> {
    let processes = processes.to_lowercase();
    if processes.contains("wxwork") || processes.contains("wework") {
        return Some("企业微信会议".into());
    }
    if processes.contains("lark") || processes.contains("feishu") {
        return Some("飞书会议".into());
    }
    if processes.contains("wemeet") || processes.contains("tencentmeeting") {
        return Some("腾讯会议".into());
    }
    None
}

fn require_allowed_origin(window: &WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|error| error.to_string())?;
    if is_allowed_web_origin(&url) {
        Ok(())
    } else {
        Err("当前页面无权访问本地录音".into())
    }
}

fn file_path_to_path(value: FilePath) -> Option<PathBuf> {
    match value {
        FilePath::Path(path) => Some(path),
        FilePath::Url(url) => url.to_file_path().ok(),
    }
}
fn cleanup_recording_parts(base: &Path) {
    for suffix in ["-microphone.wav", "-system.m4a", ".m4a", ".wav"] {
        let _ = fs::remove_file(format!("{}{}", base.display(), suffix));
    }
}

#[cfg(target_os = "macos")]
async fn start_native_recording(base: PathBuf) -> Result<(), String> {
    tokio::task::spawn_blocking(move || native_start(&base))
        .await
        .map_err(|error| error.to_string())?
}

#[cfg(not(target_os = "macos"))]
async fn start_native_recording(_: PathBuf) -> Result<(), String> {
    Err("会议录音目前仅支持 macOS 桌面端".into())
}

#[cfg(target_os = "macos")]
async fn stop_native_recording() -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(native_stop)
        .await
        .map_err(|error| error.to_string())?
}

#[cfg(not(target_os = "macos"))]
async fn stop_native_recording() -> Result<PathBuf, String> {
    Err("会议录音目前仅支持 macOS 桌面端".into())
}

#[cfg(target_os = "macos")]
fn native_start(base: &Path) -> Result<(), String> {
    let path = std::ffi::CString::new(base.to_string_lossy().as_bytes())
        .map_err(|error| error.to_string())?;
    if unsafe { snack_recording_start(path.as_ptr()) } {
        Ok(())
    } else {
        Err(native_error())
    }
}

#[cfg(target_os = "macos")]
fn native_stop() -> Result<PathBuf, String> {
    let value = unsafe { snack_recording_stop() };
    if value.is_null() {
        return Err(native_error());
    }
    let path = unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned();
    unsafe { snack_recording_free(value) };
    Ok(PathBuf::from(path))
}

#[cfg(target_os = "macos")]
fn native_error() -> String {
    let value = unsafe { snack_recording_last_error() };
    if value.is_null() {
        return "会议录音失败".into();
    }
    let error = unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned();
    unsafe { snack_recording_free(value) };
    error
}

#[cfg(target_os = "macos")]
extern "C" {
    fn snack_recording_start(base_path: *const c_char) -> bool;
    fn snack_recording_stop() -> *mut c_char;
    fn snack_recording_last_error() -> *mut c_char;
    fn snack_recording_free(value: *mut c_char);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_platform_command_or_control_as_the_default_shortcut() {
        assert_eq!(DEFAULT_RECORDING_SHORTCUT, "CommandOrControl+R");
    }

    #[test]
    fn persists_recording_metadata_atomically() {
        let directory =
            std::env::temp_dir().join(format!("snack-recording-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let audio = directory.join("meeting.wav");
        fs::write(&audio, b"audio").unwrap();
        let recording = recording_from_path(audio, Utc::now(), Duration::from_secs(65)).unwrap();
        let metadata = directory.join("recordings.json");
        persist_recordings(&metadata, std::slice::from_ref(&recording)).unwrap();
        let loaded = load_recordings(&metadata).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].duration_ms, 65_000);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn names_transcript_with_a_single_extension_separator() {
        let directory =
            std::env::temp_dir().join(format!("snack-recording-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let audio = directory.join("meeting.wav");
        fs::write(&audio, b"audio").unwrap();
        let mut recording = recording_from_path(audio, Utc::now(), Duration::ZERO).unwrap();
        apply_transcription_result(
            &mut recording,
            directory.join("meeting.txt"),
            Ok("transcript".into()),
        );
        let transcript_name = recording.transcript_file_name.unwrap();
        assert!(transcript_name.ends_with(".txt"));
        assert!(!transcript_name.ends_with("..txt"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn identifies_supported_meeting_processes() {
        assert_eq!(
            meeting_app_from_processes("/Applications/WXWork.app"),
            Some("企业微信会议".into())
        );
        assert_eq!(
            meeting_app_from_processes("Lark Helper"),
            Some("飞书会议".into())
        );
        assert_eq!(
            meeting_app_from_processes("wemeetapp"),
            Some("腾讯会议".into())
        );
        assert_eq!(meeting_app_from_processes("Finder"), None);
    }
}
