//! Server-side meeting notes submission.
//!
//! Only the transcript and minimal metadata are sent — never audio, local
//! paths, model or environment details. `recordingId` is the idempotency key:
//! the server upserts on it, so retries can never create duplicate records.
//!
//! Flow: the user explicitly submits a local transcript → the server starts
//! generating notes → desktop polls until notes are `ready`. Network failures
//! remain in the local backlog until the user retries.

use std::fs;
use std::time::Duration;

use reqwest::header::COOKIE;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::meeting::emit_state;
use crate::meeting::state::{now_rfc3339, MeetingStore, MeetingTask, TaskState, Transcript};
use crate::web::{desktop_user_agent, is_allowed_web_origin};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_POLL_ATTEMPTS: u32 = 240; // ~20 minutes

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitTranscriptRequest<'a> {
    recording_id: &'a str,
    started_at: &'a str,
    ended_at: &'a str,
    duration_ms: u64,
    language: &'a str,
    transcript: &'a Transcript,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiEnvelope<T> {
    success: bool,
    data: Option<T>,
    desc: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MeetingRecordResponse {
    /// Snowflake id serialized as a string by the server.
    id: String,
    notes_status: Option<String>,
}

/// Resolve the meeting API base from the main window's URL (works for qa and
/// prod hosts) and read the session cookie for authentication.
fn resolve_api_base(window: &WebviewWindow) -> Result<(String, String), String> {
    let url = window
        .url()
        .map_err(|_| "无法读取当前窗口地址".to_string())?;
    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to submit meeting transcripts".to_string());
    }
    // Preserve an explicit development port (for example localhost:3000).
    // Rebuilding the origin from only scheme + host silently redirected local
    // submissions to port 80.
    let origin = url.origin().ascii_serialization();
    let cookies = window
        .cookies_for_url(url)
        .map_err(|_| "无法读取登录状态".to_string())?;
    let token = cookies
        .into_iter()
        .find(|cookie| cookie.name() == "auth_token")
        .map(|cookie| cookie.value().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "桌面端未登录".to_string())?;
    Ok((origin, token))
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| "无法创建网络客户端".to_string())
}

/// Submit the transcript to the server. Returns the server record id.
pub(crate) async fn submit_transcript(
    window: &WebviewWindow,
    _store: &MeetingStore,
    task: &MeetingTask,
) -> Result<String, String> {
    let (base, token) = resolve_api_base(window)?;
    let transcript = task
        .transcript
        .as_ref()
        .ok_or_else(|| "没有可提交的转写结果".to_string())?;
    let started_at = task.started_at.as_deref().unwrap_or("");
    let ended_at = task.ended_at.as_deref().unwrap_or("");
    let request = SubmitTranscriptRequest {
        recording_id: &task.recording_id,
        started_at,
        ended_at,
        duration_ms: task.duration_ms.unwrap_or(0),
        language: &task.language,
        transcript,
    };

    let response = client()?
        .post(format!("{base}/api/snack/meetings"))
        .header(COOKIE, format!("auth_token={token}"))
        .header("User-Agent", desktop_user_agent())
        .json(&request)
        .send()
        .await
        .map_err(|error| format!("无法连接服务端: {error}"))?;

    let status = response.status();
    let body: ApiEnvelope<MeetingRecordResponse> = response
        .json()
        .await
        .map_err(|_| format!("服务端响应异常 (HTTP {status})"))?;
    if !body.success {
        return Err(body.desc.unwrap_or_else(|| "服务端拒绝了提交".to_string()));
    }
    let data = body.data.ok_or_else(|| "服务端响应缺少数据".to_string())?;
    if data.id.chars().all(|character| character.is_ascii_digit()) {
        Ok(data.id)
    } else {
        Err("服务端返回了无效的记录 id".to_string())
    }
}

/// Fetch the notes status of a server record.
pub(crate) async fn fetch_notes_status(
    window: &WebviewWindow,
    record_id: &str,
) -> Result<String, String> {
    let (base, token) = resolve_api_base(window)?;
    let response = client()?
        .get(format!("{base}/api/snack/meetings/{record_id}"))
        .header(COOKIE, format!("auth_token={token}"))
        .header("User-Agent", desktop_user_agent())
        .send()
        .await
        .map_err(|error| format!("无法连接服务端: {error}"))?;
    let status = response.status();
    let body: ApiEnvelope<MeetingRecordResponse> = response
        .json()
        .await
        .map_err(|_| format!("服务端响应异常 (HTTP {status})"))?;
    if !body.success {
        return Err(body.desc.unwrap_or_else(|| "服务端响应失败".to_string()));
    }
    let data = body.data.ok_or_else(|| "服务端响应缺少数据".to_string())?;
    Ok(data.notes_status.unwrap_or_else(|| "pending".to_string()))
}

/// Run the full submission pipeline (submit + poll). Mutates and persists the
/// task state at every step. Returns Ok(record_id) when notes are ready.
pub(crate) async fn run_submission_pipeline(
    app: &AppHandle,
    store: &MeetingStore,
    recording_id: &str,
) -> Result<String, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "主窗口不可用".to_string())?;

    let record_id = {
        let mut task = store
            .load_task_record(recording_id)
            .ok_or_else(|| "没有待提交的会议任务".to_string())?;
        task.submission.attempts += 1;
        task.state = TaskState::GeneratingNotes;
        task.error = None;
        task.next_retry_at = None;
        store.save_task_progress(&task)?;
        emit_state(app, store);

        match submit_transcript(&window, store, &task).await {
            Ok(record_id) => {
                let mut task = store.load_task_record(recording_id).ok_or("任务丢失")?;
                task.submission.server_record_id = Some(record_id.clone());
                task.submission.last_error = None;
                store.save_task_progress(&task)?;
                record_id
            }
            Err(message) => {
                let mut task = store.load_task_record(recording_id).ok_or("任务丢失")?;
                task.submission.last_error = Some(message.clone());
                store.save_task_progress(&task)?;
                return Err(message);
            }
        }
    };

    // Poll until notes are ready.
    let mut poll_attempts = 0u32;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        poll_attempts += 1;
        match fetch_notes_status(&window, &record_id).await {
            Ok(status) if status == "ready" => {
                let mut task = store.load_task_record(recording_id).ok_or("任务丢失")?;
                task.state = TaskState::Ready;
                task.ended_at = Some(now_rfc3339());
                store.save_task_progress(&task)?;
                emit_state(app, store);
                super::notifications::notify_notes_ready(app);
                return Ok(record_id);
            }
            Ok(status) if status == "failed" => {
                let mut task = store.load_task_record(recording_id).ok_or("任务丢失")?;
                task.state = TaskState::NotesFailed;
                task.error = Some("服务端生成会议纪要失败".to_string());
                store.save_task_progress(&task)?;
                emit_state(app, store);
                return Err("服务端生成会议纪要失败".to_string());
            }
            Ok(_) => {
                if poll_attempts >= MAX_POLL_ATTEMPTS {
                    let mut task = store.load_task_record(recording_id).ok_or("任务丢失")?;
                    task.state = TaskState::NotesFailed;
                    task.error = Some("服务端生成会议纪要超时".to_string());
                    store.save_task_progress(&task)?;
                    emit_state(app, store);
                    return Err("服务端生成会议纪要超时".to_string());
                }
            }
            Err(message) => {
                // Network hiccup while polling: keep the task in generating
                // state and retry the poll (bounded).
                let mut task = store.load_task_record(recording_id).ok_or("任务丢失")?;
                task.submission.last_error = Some(message.clone());
                store.save_task_progress(&task)?;
                if poll_attempts >= MAX_POLL_ATTEMPTS {
                    task.state = TaskState::NotesFailed;
                    task.error = Some("无法获取纪要生成状态".to_string());
                    store.save_task_progress(&task)?;
                    emit_state(app, store);
                    return Err("无法获取纪要生成状态".to_string());
                }
            }
        }
    }
}

/// Persist a transcript atomically while retaining the user-owned local audio.
pub(crate) fn persist_transcript(
    store: &MeetingStore,
    task: &mut MeetingTask,
    transcript: Transcript,
) -> Result<(), String> {
    crate::meeting::state::persist_json_atomic(
        &store.transcript_path(&task.recording_id),
        &transcript,
    )?;
    let text_path = store.available_transcript_text_path(task);
    if let Some(parent) = text_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(
        &text_path,
        crate::meeting::state::transcript_text(&transcript),
    )
    .map_err(|error| error.to_string())?;
    task.transcript = Some(transcript);
    task.transcript_path = Some(text_path.to_string_lossy().into_owned());
    Ok(())
}
