use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewWindow};
use url::Url;

use crate::web::is_allowed_web_origin;

const MAX_OFFICIAL_JOBS: usize = 7;
const SNACK_RECORD_BUNDLE_ID: &str = "app.snackrecord.local";
const EMBEDDED_RECORDING_BUNDLE_ID: &str = "cn.yaowutech.snack.recording-service";
const EMBEDDED_RECORDING_APP_NAME: &str = "Snack Recording Service.app";
const EMBEDDED_RECORDING_SCHEME: &str = "snack-record-runtime";
const LEGACY_RECORDING_SCHEME: &str = "snack-record";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SnackRecordRuntime {
    pub(crate) path: PathBuf,
    pub(crate) scheme: &'static str,
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
    transcription_progress: Option<f64>,
    transcription_state: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopRecordingStatus {
    active: bool,
    elapsed_ms: u64,
    started_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingPreferences {
    auto_meeting_reminder: bool,
    language: String,
    mode: String,
    organize_by_date: bool,
    output_directory: String,
    shortcut: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingPreferencesRequest {
    auto_meeting_reminder: bool,
    language: String,
    mode: String,
    organize_by_date: bool,
    output_directory: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct TranscriptionRequest {
    language: String,
    mode: String,
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntegrationState {
    active: bool,
    pid: Option<u32>,
    started_at: Option<String>,
}

#[tauri::command]
pub(crate) async fn start_system_audio_recording(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<DesktopRecordingStatus, String> {
    ensure_allowed_origin(&window)?;
    start_recording(&app)?;
    let root = snack_record_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        for _ in 0..50 {
            let status = read_recording_status(&root);
            if status.active {
                return Ok(status);
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err("Snack Record 未能开始录音，请在 Snack Record 中检查录音权限和运行状态".to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(crate) fn start_recording(app: &AppHandle) -> Result<(), String> {
    send_action(app, "start", &[])
}

#[tauri::command]
pub(crate) async fn stop_system_audio_recording(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<DesktopRecording, String> {
    ensure_allowed_origin(&window)?;
    let root = snack_record_root(&app)?;
    let previous_ids = load_official_jobs(&root)
        .into_iter()
        .map(|recording| recording.id)
        .collect::<Vec<_>>();
    send_action(&app, "stop", &[])?;
    tauri::async_runtime::spawn_blocking(move || {
        for _ in 0..50 {
            let jobs = load_official_jobs(&root);
            if let Some(recording) = jobs
                .iter()
                .find(|recording| !previous_ids.contains(&recording.id))
                .cloned()
            {
                return Ok(recording);
            }
            if !read_recording_status(&root).active {
                if let Some(recording) = jobs.into_iter().next() {
                    return Ok(recording);
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err("Snack Record 已停止，但尚未生成可读取的录音记录".to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(crate) fn get_system_audio_recording_status(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<DesktopRecordingStatus, String> {
    ensure_allowed_origin(&window)?;
    Ok(read_recording_status(&snack_record_root(&app)?))
}

#[tauri::command]
pub(crate) fn list_system_audio_recordings(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<Vec<DesktopRecording>, String> {
    ensure_allowed_origin(&window)?;
    Ok(load_official_jobs(&snack_record_root(&app)?))
}

#[tauri::command]
pub(crate) fn choose_and_import_recording(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<Option<DesktopRecording>, String> {
    ensure_allowed_origin(&window)?;
    send_action(&app, "import", &[])?;
    Ok(None)
}

#[tauri::command]
pub(crate) fn transcribe_local_audio(
    app: AppHandle,
    window: WebviewWindow,
    request: TranscriptionRequest,
) -> Result<DesktopRecording, String> {
    ensure_allowed_origin(&window)?;
    let root = snack_record_root(&app)?;
    let recording = load_official_jobs(&root)
        .into_iter()
        .find(|recording| recording.path == request.path)
        .ok_or_else(|| "未在 Snack Record 历史记录中找到该音频".to_string())?;
    send_action(
        &app,
        "retry",
        &[
            ("id", recording.id.as_str()),
            ("mode", normalized_mode(&request.mode)),
            ("language", normalized_language(&request.language)),
        ],
    )?;
    Ok(DesktopRecording {
        transcription_progress: Some(0.0),
        transcription_state: "pending".to_string(),
        ..recording
    })
}

#[tauri::command]
pub(crate) fn delete_system_audio_recording(
    app: AppHandle,
    window: WebviewWindow,
    id: String,
) -> Result<bool, String> {
    ensure_allowed_origin(&window)?;
    send_action(&app, "delete", &[("id", id.as_str())])?;
    Ok(true)
}

#[tauri::command]
pub(crate) fn delete_system_audio_recordings(
    app: AppHandle,
    window: WebviewWindow,
    ids: Vec<String>,
) -> Result<usize, String> {
    ensure_allowed_origin(&window)?;
    if ids.is_empty() {
        return Ok(0);
    }
    let joined = ids.join(",");
    send_action(&app, "delete-many", &[("ids", joined.as_str())])?;
    Ok(ids.len())
}

#[tauri::command]
pub(crate) fn open_local_recording_file(
    app: AppHandle,
    window: WebviewWindow,
    id: String,
    kind: String,
) -> Result<(), String> {
    ensure_allowed_origin(&window)?;
    let recording = load_official_jobs(&snack_record_root(&app)?)
        .into_iter()
        .find(|recording| recording.id == id)
        .ok_or_else(|| "未找到这条 Snack Record 记录".to_string())?;
    let path = match kind.as_str() {
        "audio" => recording.path,
        "transcript" => recording
            .transcript_path
            .ok_or_else(|| "这条录音还没有可打开的转写文件".to_string())?,
        _ => return Err("不支持的录音文件类型".to_string()),
    };
    open_path(Path::new(&path))
}

#[tauri::command]
pub(crate) fn open_recording_window(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    ensure_allowed_origin(&window)?;
    show_recording_window(&app)
}

pub(crate) fn show_recording_window(app: &AppHandle) -> Result<(), String> {
    send_action(app, "show", &[])
}

#[tauri::command]
pub(crate) fn choose_recording_output_directory(
    _app: AppHandle,
    window: WebviewWindow,
) -> Result<Option<String>, String> {
    ensure_allowed_origin(&window)?;
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/osascript")
            .args([
                "-e",
                "POSIX path of (choose folder with prompt \"选择 Snack Record 转写文件保存位置\")",
            ])
            .output()
            .map_err(|error| format!("无法打开文件夹选择器：{error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_end_matches('/')
            .to_string();
        Ok((!path.is_empty()).then_some(path))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err("当前桌面系统暂不支持选择输出目录".to_string())
    }
}

#[tauri::command]
pub(crate) fn get_recording_preferences(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<RecordingPreferences, String> {
    ensure_allowed_origin(&window)?;
    Ok(read_preferences(&app))
}

#[tauri::command]
pub(crate) fn configure_recording_preferences(
    app: AppHandle,
    window: WebviewWindow,
    request: RecordingPreferencesRequest,
) -> Result<(), String> {
    ensure_allowed_origin(&window)?;
    send_action(
        &app,
        "configure",
        &[
            ("language", normalized_language(&request.language)),
            ("mode", normalized_mode(&request.mode)),
            (
                "reminder",
                if request.auto_meeting_reminder {
                    "automatic"
                } else {
                    "off"
                },
            ),
            ("daily", if request.organize_by_date { "1" } else { "0" }),
            ("output", request.output_directory.as_str()),
        ],
    )
}

#[tauri::command]
pub(crate) fn configure_recording_shortcut(
    window: WebviewWindow,
    shortcut: String,
) -> Result<(), String> {
    ensure_allowed_origin(&window)?;
    if shortcut == "Control+R" {
        return Ok(());
    }
    Err("Snack Record 当前固定使用 Control+R，暂不支持自定义快捷键".to_string())
}

#[tauri::command]
pub(crate) fn detect_active_meeting_app(window: WebviewWindow) -> Result<Option<String>, String> {
    ensure_allowed_origin(&window)?;
    Ok(None)
}

fn send_action(app: &AppHandle, action: &str, params: &[(&str, &str)]) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let runtime = snack_record_runtime(app)?;
        let mut url = Url::parse(&format!("{}://control/{action}", runtime.scheme))
            .map_err(|e| e.to_string())?;
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        let status = Command::new("/usr/bin/open")
            .arg("-a")
            .arg(runtime.path)
            .arg(url.as_str())
            .status()
            .map_err(|error| format!("无法打开 Snack Record：{error}"))?;
        if status.success() {
            return Ok(());
        }
        Err("无法把操作交给 Snack Record".to_string())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, action, params);
        Err("当前桌面系统暂不支持 Snack Record".to_string())
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn snack_record_runtime(app: &AppHandle) -> Result<SnackRecordRuntime, String> {
    let home = app.path().home_dir().map_err(|error| error.to_string())?;
    let resources = app
        .path()
        .resource_dir()
        .map_err(|error| error.to_string())?;
    snack_record_runtime_candidates(&resources, &home)
        .into_iter()
        .find(|candidate| candidate.path.is_dir())
        .ok_or_else(|| "Snack 内置录音组件缺失，请重新安装或升级 Snack".to_string())
}

#[cfg(target_os = "macos")]
fn snack_record_runtime_candidates(resources: &Path, home: &Path) -> [SnackRecordRuntime; 3] {
    [
        SnackRecordRuntime {
            path: resources.join(EMBEDDED_RECORDING_APP_NAME),
            scheme: EMBEDDED_RECORDING_SCHEME,
        },
        SnackRecordRuntime {
            path: home.join("Applications/Snack Record.app"),
            scheme: LEGACY_RECORDING_SCHEME,
        },
        SnackRecordRuntime {
            path: PathBuf::from("/Applications/Snack Record.app"),
            scheme: LEGACY_RECORDING_SCHEME,
        },
    ]
}

fn snack_record_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .home_dir()
        .map(|home| home.join("Library/Application Support/Snack Record"))
        .map_err(|error| error.to_string())
}

fn read_recording_status(root: &Path) -> DesktopRecordingStatus {
    let state = fs::read(root.join("integration-state.json"))
        .ok()
        .and_then(|contents| serde_json::from_slice::<IntegrationState>(&contents).ok());
    let Some(state) = state else {
        return inactive_status();
    };
    if !state.active || !state.pid.is_some_and(process_is_running) {
        return inactive_status();
    }
    let elapsed_ms = state
        .started_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|started| {
            Utc::now()
                .signed_duration_since(started.with_timezone(&Utc))
                .num_milliseconds()
                .max(0) as u64
        })
        .unwrap_or(0);
    DesktopRecordingStatus {
        active: true,
        elapsed_ms,
        started_at: state.started_at,
    }
}

fn inactive_status() -> DesktopRecordingStatus {
    DesktopRecordingStatus {
        active: false,
        elapsed_ms: 0,
        started_at: None,
    }
}

fn process_is_running(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

fn read_preferences(_app: &AppHandle) -> RecordingPreferences {
    RecordingPreferences {
        auto_meeting_reminder: read_default("SnackRecordReminderMode")
            .is_some_and(|value| value == "automatic"),
        language: read_default("SnackRecordInterfaceLanguage")
            .filter(|value| value == "en")
            .unwrap_or_else(|| "zh".to_string()),
        mode: read_default("SnackRecordTranscriptionMode")
            .filter(|value| value == "standard")
            .unwrap_or_else(|| "fast".to_string()),
        organize_by_date: read_default("SnackRecordDailyFolder")
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "YES")),
        output_directory: read_default("SnackRecordOutputDirectory").unwrap_or_default(),
        shortcut: "Control+R".to_string(),
    }
}

fn read_default(key: &str) -> Option<String> {
    [EMBEDDED_RECORDING_BUNDLE_ID, SNACK_RECORD_BUNDLE_ID]
        .into_iter()
        .find_map(|bundle_id| {
            let output = Command::new("/usr/bin/defaults")
                .args(["read", bundle_id, key])
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
}

fn load_official_jobs(root: &Path) -> Vec<DesktopRecording> {
    let plist = root.join("recent-jobs.plist");
    if !plist.is_file() {
        return Vec::new();
    }
    (0..MAX_OFFICIAL_JOBS)
        .filter_map(|index| load_official_job(&plist, index))
        .collect()
}

fn load_official_job(plist: &Path, index: usize) -> Option<DesktopRecording> {
    let id = plist_value(plist, index, "identifier")?;
    let path = plist_value(plist, index, "audioPath")?;
    if !Path::new(&path).is_file() {
        return None;
    }
    let transcript_path =
        plist_value(plist, index, "outputPath").filter(|value| Path::new(value).is_file());
    let transcript_text = transcript_path
        .as_ref()
        .and_then(|value| fs::read_to_string(value).ok());
    let state = plist_value(plist, index, "state").unwrap_or_else(|| "0".to_string());
    let transcription_state = match state.as_str() {
        "1" => "processing",
        "2" => "completed",
        "3" => "failed",
        _ => "pending",
    };
    let metadata = fs::metadata(&path).ok()?;
    Some(DesktopRecording {
        created_at: plist_value(plist, index, "startDate")
            .unwrap_or_else(|| Utc::now().to_rfc3339()),
        duration_ms: media_duration_ms(Path::new(&path)),
        file_name: Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("recording.wav")
            .to_string(),
        file_size_bytes: metadata.len(),
        id,
        path,
        transcript_file_name: plist_value(plist, index, "filename"),
        transcript_path,
        transcript_text,
        transcription_error: (transcription_state == "failed").then(|| "转写失败".to_string()),
        transcription_progress: Some(if transcription_state == "completed" {
            100.0
        } else {
            0.0
        }),
        transcription_state: transcription_state.to_string(),
    })
}

fn plist_value(plist: &Path, index: usize, key: &str) -> Option<String> {
    let key_path = format!("{index}.{key}");
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", &key_path, "raw"])
        .arg(plist)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn media_duration_ms(path: &Path) -> u64 {
    let Some(ffprobe) = ffprobe_path() else {
        return 0;
    };
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output();
    output
        .ok()
        .filter(|value| value.status.success())
        .and_then(|value| String::from_utf8(value.stdout).ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .map(|seconds| (seconds * 1000.0) as u64)
        .unwrap_or(0)
}

fn ffprobe_path() -> Option<PathBuf> {
    ["/opt/homebrew/bin/ffprobe", "/usr/local/bin/ffprobe"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn open_path(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err("文件不存在或已经被移动".to_string());
    }
    let status = Command::new("/usr/bin/open")
        .arg(path)
        .status()
        .map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "无法打开本地文件".to_string())
}

fn normalized_mode(value: &str) -> &'static str {
    if value == "standard" {
        "standard"
    } else {
        "fast"
    }
}

fn normalized_language(value: &str) -> &'static str {
    if value == "en" {
        "en"
    } else {
        "zh"
    }
}

fn ensure_allowed_origin(window: &WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|error| error.to_string())?;
    is_allowed_web_origin(&url)
        .then_some(())
        .ok_or_else(|| "origin is not allowed to use Snack Record".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_supported_preferences() {
        assert_eq!(normalized_mode("standard"), "standard");
        assert_eq!(normalized_mode("anything"), "fast");
        assert_eq!(normalized_language("en"), "en");
        assert_eq!(normalized_language("anything"), "zh");
    }

    #[test]
    fn inactive_status_has_no_elapsed_time() {
        let status = inactive_status();
        assert!(!status.active);
        assert_eq!(status.elapsed_ms, 0);
        assert!(status.started_at.is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn embedded_recording_runtime_is_preferred_over_legacy_installations() {
        let resources = Path::new("/Applications/Snack.app/Contents/Resources");
        let home = Path::new("/Users/tester");
        let candidates = snack_record_runtime_candidates(resources, home);

        assert_eq!(
            candidates[0],
            SnackRecordRuntime {
                path: resources.join("Snack Recording Service.app"),
                scheme: "snack-record-runtime",
            }
        );
        assert_eq!(
            candidates[1].path,
            home.join("Applications/Snack Record.app")
        );
        assert_eq!(candidates[1].scheme, "snack-record");
    }
}
