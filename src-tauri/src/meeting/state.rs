//! Persistent state for the local meeting recording feature.
//!
//! Meeting state is persisted under `{app_data_dir}/meeting`:
//! - `resource.json` — the local model resource state machine
//! - `task.json` — the current meeting task state machine
//! - `tasks/*.json` — every retained local recording and transcript
//! - `settings.json` — user-owned storage and quick-recording settings
//!
//! Every transition is validated by a state machine so illegal jumps fail fast
//! and crash recovery can reconcile persisted state on startup.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

pub(crate) const MEETING_DIR_NAME: &str = "meeting";
pub(crate) const MODELS_DIR_NAME: &str = "models";
pub(crate) const AUDIO_DIR_NAME: &str = "audio";
pub(crate) const TRANSCRIPTS_DIR_NAME: &str = "transcripts";
pub(crate) const DOWNLOADS_DIR_NAME: &str = "downloads";
pub(crate) const TASKS_DIR_NAME: &str = "tasks";
pub(crate) const DEFAULT_MEETING_NOTES_PROMPT: &str =
    "调用会议纪要 skill 帮我结构化总结下面这段会议转写，不超过 600 字。";
pub(crate) const MAX_MEETING_NOTES_PROMPT_CHARS: usize = 600;

const RESOURCE_FILE: &str = "resource.json";
const TASK_FILE: &str = "task.json";
const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeetingSettings {
    pub(crate) storage_directory: Option<String>,
    pub(crate) shortcut: String,
    pub(crate) retain_audio: bool,
    #[serde(default)]
    pub(crate) organize_transcripts_by_date: bool,
    #[serde(default = "default_meeting_notes_prompt")]
    pub(crate) notes_prompt: String,
}

impl Default for MeetingSettings {
    fn default() -> Self {
        Self {
            storage_directory: None,
            shortcut: "CommandOrControl+Shift+R".to_string(),
            retain_audio: true,
            organize_transcripts_by_date: false,
            notes_prompt: default_meeting_notes_prompt(),
        }
    }
}

fn default_meeting_notes_prompt() -> String {
    DEFAULT_MEETING_NOTES_PROMPT.to_string()
}

// ---------------------------------------------------------------------------
// Resource state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResourceState {
    NotInstalled,
    Checking,
    Downloading,
    Paused,
    Verifying,
    Installing,
    Validating,
    Ready,
    InsufficientDisk,
    Corrupted,
    Failed,
    UpdateRequired,
}

impl ResourceState {
    /// Whether the state represents an idle, actionable state (no background work running).
    pub(crate) fn is_idle(self) -> bool {
        matches!(
            self,
            ResourceState::NotInstalled
                | ResourceState::Ready
                | ResourceState::InsufficientDisk
                | ResourceState::Corrupted
                | ResourceState::Failed
                | ResourceState::UpdateRequired
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DownloadProgress {
    pub(crate) downloaded_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) speed_bytes_per_sec: u64,
    pub(crate) percent: u8,
}

impl Default for DownloadProgress {
    fn default() -> Self {
        Self {
            downloaded_bytes: 0,
            total_bytes: 0,
            speed_bytes_per_sec: 0,
            percent: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResourceStatus {
    pub(crate) state: ResourceState,
    pub(crate) model_key: Option<String>,
    pub(crate) model_size_bytes: Option<u64>,
    pub(crate) installed_size_bytes: Option<u64>,
    pub(crate) download: Option<DownloadProgress>,
    pub(crate) error: Option<String>,
    pub(crate) updated_at: String,
}

impl Default for ResourceStatus {
    fn default() -> Self {
        Self {
            state: ResourceState::NotInstalled,
            model_key: None,
            model_size_bytes: None,
            installed_size_bytes: None,
            download: None,
            error: None,
            updated_at: now_rfc3339(),
        }
    }
}

impl ResourceStatus {
    pub(crate) fn with_state(mut self, state: ResourceState) -> Self {
        self.state = state;
        self.updated_at = now_rfc3339();
        self
    }
}

// ---------------------------------------------------------------------------
// Meeting task state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskState {
    Idle,
    Checking,
    Recording,
    Finalizing,
    TranscribingLocal,
    TranscriptReady,
    WaitingForNetwork,
    GeneratingNotes,
    Ready,
    PermissionDenied,
    CaptureFailed,
    FinalizeFailed,
    TranscriptionFailed,
    NotesFailed,
}

impl TaskState {
    pub(crate) fn is_terminal_error(self) -> bool {
        matches!(
            self,
            TaskState::PermissionDenied
                | TaskState::CaptureFailed
                | TaskState::FinalizeFailed
                | TaskState::TranscriptionFailed
                | TaskState::NotesFailed
        )
    }

    #[allow(dead_code)]
    pub(crate) fn is_active(self) -> bool {
        matches!(
            self,
            TaskState::Checking
                | TaskState::Recording
                | TaskState::Finalizing
                | TaskState::TranscribingLocal
                | TaskState::GeneratingNotes
        )
    }

    pub(crate) fn blocks_recording(self) -> bool {
        matches!(
            self,
            TaskState::Checking
                | TaskState::Recording
                | TaskState::Finalizing
                | TaskState::TranscribingLocal
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TranscriptSegment {
    pub(crate) start_ms: u64,
    pub(crate) end_ms: u64,
    pub(crate) text: String,
    /// Relative speaker label, e.g. "说话人 1". Never a real identity.
    pub(crate) speaker: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Transcript {
    pub(crate) text: String,
    pub(crate) language: String,
    pub(crate) segments: Vec<TranscriptSegment>,
    pub(crate) model_key: String,
    pub(crate) engine: String,
    pub(crate) generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerSubmission {
    pub(crate) attempts: u32,
    #[serde(default, deserialize_with = "deserialize_optional_record_id")]
    pub(crate) server_record_id: Option<String>,
    pub(crate) last_error: Option<String>,
}

fn deserialize_optional_record_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(serde_json::Value::Number(value)) => Ok(Some(value.to_string())),
        Some(_) => Err(serde::de::Error::custom("invalid server record id")),
    }
}

impl Default for ServerSubmission {
    fn default() -> Self {
        Self {
            attempts: 0,
            server_record_id: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeetingTask {
    pub(crate) recording_id: String,
    pub(crate) state: TaskState,
    pub(crate) started_at: Option<String>,
    pub(crate) ended_at: Option<String>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) language: String,
    pub(crate) audio_path: Option<String>,
    pub(crate) audio_bytes: Option<u64>,
    pub(crate) transcript: Option<Transcript>,
    #[serde(default)]
    pub(crate) transcript_path: Option<String>,
    pub(crate) submission: ServerSubmission,
    /// RFC3339 time after which an automatic retry is allowed (waiting_for_network).
    pub(crate) next_retry_at: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

impl MeetingTask {
    pub(crate) fn new(recording_id: String, language: String) -> Self {
        let now = now_rfc3339();
        Self {
            recording_id,
            state: TaskState::Idle,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            language,
            audio_path: None,
            audio_bytes: None,
            transcript: None,
            transcript_path: None,
            submission: ServerSubmission::default(),
            next_retry_at: None,
            error: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_state(mut self, state: TaskState) -> Self {
        self.state = state;
        self.updated_at = now_rfc3339();
        self
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct MeetingStore {
    root: PathBuf,
    default_recordings_root: PathBuf,
}

impl MeetingStore {
    pub(crate) fn open(app: &AppHandle) -> Result<Self, String> {
        let root = app
            .path()
            .app_data_dir()
            .map_err(|error| error.to_string())?
            .join(MEETING_DIR_NAME);
        let default_recordings_root = app
            .path()
            .desktop_dir()
            .map_err(|error| error.to_string())?
            .join("Snack会议");
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        for sub in [
            MODELS_DIR_NAME,
            AUDIO_DIR_NAME,
            TRANSCRIPTS_DIR_NAME,
            DOWNLOADS_DIR_NAME,
            TASKS_DIR_NAME,
        ] {
            fs::create_dir_all(root.join(sub)).map_err(|error| error.to_string())?;
        }
        fs::create_dir_all(&default_recordings_root).map_err(|error| error.to_string())?;
        Ok(Self {
            root,
            default_recordings_root,
        })
    }

    pub(crate) fn root(&self) -> &PathBuf {
        &self.root
    }

    pub(crate) fn clone_for_task(&self) -> MeetingStore {
        self.clone()
    }

    pub(crate) fn models_dir(&self) -> PathBuf {
        self.root.join(MODELS_DIR_NAME)
    }

    pub(crate) fn downloads_dir(&self) -> PathBuf {
        self.root.join(DOWNLOADS_DIR_NAME)
    }

    pub(crate) fn audio_path(&self, recording_id: &str) -> PathBuf {
        self.recordings_root()
            .join(AUDIO_DIR_NAME)
            .join(format!("{recording_id}.wav"))
    }

    pub(crate) fn transcript_path(&self, recording_id: &str) -> PathBuf {
        self.recordings_root()
            .join(TRANSCRIPTS_DIR_NAME)
            .join(format!("{recording_id}.json"))
    }

    pub(crate) fn transcript_text_path(&self, task: &MeetingTask) -> PathBuf {
        let created_at = chrono::DateTime::parse_from_rfc3339(&task.created_at)
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d-%H%M%S")
                    .to_string()
            })
            .unwrap_or_else(|_| task.recording_id.clone());
        self.transcript_text_root(task)
            .join(format!("Snack会议-{created_at}.txt"))
    }

    pub(crate) fn ensure_transcript_output_directory(
        &self,
        task: &MeetingTask,
    ) -> Result<(), String> {
        fs::create_dir_all(self.transcript_text_root(task)).map_err(|error| error.to_string())
    }

    pub(crate) fn recordings_root(&self) -> PathBuf {
        self.recordings_root_for_settings(&self.load_settings())
    }

    fn recordings_root_for_settings(&self, settings: &MeetingSettings) -> PathBuf {
        settings
            .storage_directory
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_recordings_root.clone())
    }

    fn transcript_text_root(&self, task: &MeetingTask) -> PathBuf {
        let settings = self.load_settings();
        let root = self.recordings_root_for_settings(&settings);
        if settings.organize_transcripts_by_date {
            root.join(meeting_task_local_date(task))
        } else {
            root.join(TRANSCRIPTS_DIR_NAME)
        }
    }

    pub(crate) fn ensure_recording_directories(&self) -> Result<(), String> {
        self.ensure_recording_directories_for(&self.load_settings())
    }

    fn ensure_recording_directories_for(&self, settings: &MeetingSettings) -> Result<(), String> {
        let root = self.recordings_root_for_settings(settings);
        for sub in [AUDIO_DIR_NAME, TRANSCRIPTS_DIR_NAME] {
            fs::create_dir_all(root.join(sub)).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn load_resource(&self) -> ResourceStatus {
        match fs::read(self.root.join(RESOURCE_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => ResourceStatus::default(),
        }
    }

    pub(crate) fn save_resource(&self, status: &ResourceStatus) -> Result<(), String> {
        persist_json_atomic(&self.root.join(RESOURCE_FILE), status)
    }

    pub(crate) fn load_task(&self) -> Option<MeetingTask> {
        let bytes = fs::read(self.root.join(TASK_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub(crate) fn save_task(&self, task: &MeetingTask) -> Result<(), String> {
        persist_json_atomic(&self.root.join(TASK_FILE), task)?;
        self.save_task_record(task)
    }

    pub(crate) fn save_task_record(&self, task: &MeetingTask) -> Result<(), String> {
        persist_json_atomic(&self.task_record_path(&task.recording_id), task)
    }

    pub(crate) fn save_task_progress(&self, task: &MeetingTask) -> Result<(), String> {
        self.save_task_record(task)?;
        if self
            .load_task()
            .is_some_and(|current| current.recording_id == task.recording_id)
        {
            persist_json_atomic(&self.root.join(TASK_FILE), task)?;
        }
        Ok(())
    }

    pub(crate) fn load_task_record(&self, recording_id: &str) -> Option<MeetingTask> {
        let bytes = fs::read(self.task_record_path(recording_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub(crate) fn load_task_records(&self) -> Vec<MeetingTask> {
        let Ok(entries) = fs::read_dir(self.root.join(TASKS_DIR_NAME)) else {
            return Vec::new();
        };
        let mut tasks = entries
            .flatten()
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<MeetingTask>(&bytes).ok())
            .collect::<Vec<_>>();
        for task in &mut tasks {
            self.ensure_transcript_text_file(task);
        }
        tasks.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        tasks
    }

    fn ensure_transcript_text_file(&self, task: &mut MeetingTask) {
        if task
            .transcript_path
            .as_deref()
            .is_some_and(|path| PathBuf::from(path).exists())
        {
            return;
        }
        let Some(transcript) = task.transcript.as_ref() else {
            return;
        };
        let path = self.transcript_text_path(task);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::write(&path, transcript_text(transcript)).is_ok() {
            task.transcript_path = Some(path.to_string_lossy().into_owned());
            let _ = self.save_task_record(task);
        }
    }

    pub(crate) fn load_settings(&self) -> MeetingSettings {
        let mut settings: MeetingSettings = fs::read(self.root.join(SETTINGS_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        if settings
            .storage_directory
            .as_deref()
            .is_none_or(|path| path.trim().is_empty())
        {
            settings.storage_directory =
                Some(self.default_recordings_root.to_string_lossy().into());
        }
        if settings.notes_prompt.trim().is_empty() {
            settings.notes_prompt = default_meeting_notes_prompt();
        }
        settings
    }

    pub(crate) fn prepare_settings(&self, settings: &MeetingSettings) -> Result<(), String> {
        validate_meeting_settings(settings)?;
        let recordings_root = self.recordings_root_for_settings(settings);
        if !recordings_root.is_absolute() {
            return Err("录音保存位置必须是完整的文件夹路径".to_string());
        }
        self.ensure_recording_directories_for(settings)
            .map_err(|error| format!("无法使用所选录音保存位置: {error}"))
    }

    pub(crate) fn save_settings(&self, settings: &MeetingSettings) -> Result<(), String> {
        persist_json_atomic(&self.root.join(SETTINGS_FILE), settings)
    }

    pub(crate) fn delete_task_file(&self) {
        let _ = fs::remove_file(self.root.join(TASK_FILE));
    }

    fn task_record_path(&self, recording_id: &str) -> PathBuf {
        self.root
            .join(TASKS_DIR_NAME)
            .join(format!("{recording_id}.json"))
    }
}

pub(crate) fn validate_meeting_settings(settings: &MeetingSettings) -> Result<(), String> {
    if !settings.retain_audio {
        return Err("Snack 会议必须保留本地录音文件".to_string());
    }
    if settings.shortcut.trim().is_empty() {
        return Err("请设置录音快捷键".to_string());
    }
    let prompt_chars = settings.notes_prompt.chars().count();
    if prompt_chars == 0 || prompt_chars > MAX_MEETING_NOTES_PROMPT_CHARS {
        return Err("会议纪要 Prompt 需为 1 到 600 个字".to_string());
    }
    Ok(())
}

pub(crate) fn persist_json_atomic<T: Serialize>(path: &PathBuf, value: &T) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub(crate) fn parse_rfc3339_millis(value: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp_millis() as u64)
}

pub(crate) fn transcript_text(transcript: &Transcript) -> String {
    if transcript.segments.is_empty() {
        return transcript.text.trim().to_string();
    }
    transcript
        .segments
        .iter()
        .map(|segment| {
            let seconds = segment.start_ms / 1_000;
            format!(
                "[{:02}:{:02}] {}：{}",
                seconds / 60,
                seconds % 60,
                segment.speaker,
                segment.text.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn meeting_task_local_date(task: &MeetingTask) -> String {
    task.ended_at
        .as_deref()
        .or(task.started_at.as_deref())
        .unwrap_or(&task.created_at)
        .parse::<chrono::DateTime<chrono::FixedOffset>>()
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|_| chrono::Local::now().format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        now_rfc3339, parse_rfc3339_millis, validate_meeting_settings, MeetingSettings,
        MeetingStore, MeetingTask, ResourceState, ResourceStatus, ServerSubmission, TaskState,
        DEFAULT_MEETING_NOTES_PROMPT,
    };
    use std::fs;

    #[test]
    fn resource_defaults_to_not_installed() {
        let status = ResourceStatus::default();
        assert_eq!(status.state, ResourceState::NotInstalled);
        assert!(status.error.is_none());
    }

    #[test]
    fn resource_idle_states_exclude_busy_ones() {
        assert!(ResourceState::NotInstalled.is_idle());
        assert!(ResourceState::Ready.is_idle());
        assert!(!ResourceState::Downloading.is_idle());
        assert!(!ResourceState::Validating.is_idle());
        assert!(ResourceState::Corrupted.is_idle());
    }

    #[test]
    fn task_created_idle_and_terminals_are_errors() {
        let task = MeetingTask::new("rec-1".to_string(), "zh".to_string());
        assert_eq!(task.state, TaskState::Idle);
        assert!(TaskState::TranscriptionFailed.is_terminal_error());
        assert!(!TaskState::TranscribingLocal.is_terminal_error());
        assert!(!TaskState::Idle.blocks_recording());
        assert!(!TaskState::TranscriptReady.blocks_recording());
        assert!(!TaskState::Ready.blocks_recording());
        assert!(TaskState::Recording.blocks_recording());
        assert!(TaskState::TranscribingLocal.blocks_recording());
    }

    #[test]
    fn settings_retain_audio_by_default() {
        let settings = MeetingSettings::default();
        assert!(settings.retain_audio);
        assert!(!settings.organize_transcripts_by_date);
        assert_eq!(settings.shortcut, "CommandOrControl+Shift+R");
        assert_eq!(settings.notes_prompt, DEFAULT_MEETING_NOTES_PROMPT);
        assert!(validate_meeting_settings(&settings).is_ok());
    }

    #[test]
    fn legacy_settings_receive_the_default_notes_prompt() {
        let settings: MeetingSettings = serde_json::from_value(serde_json::json!({
            "storageDirectory": "/tmp/meeting",
            "shortcut": "CommandOrControl+Shift+R",
            "retainAudio": true
        }))
        .unwrap();

        assert_eq!(settings.notes_prompt, DEFAULT_MEETING_NOTES_PROMPT);
        assert!(!settings.organize_transcripts_by_date);
    }

    #[test]
    fn dated_transcript_directory_is_created_on_first_transcription() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-date-folder-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let recordings_root = root.join("recordings");
        fs::create_dir_all(&state_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: recordings_root.clone(),
        };
        let mut settings = MeetingSettings::default();
        settings.storage_directory = Some(recordings_root.to_string_lossy().into_owned());
        settings.organize_transcripts_by_date = true;
        store.save_settings(&settings).unwrap();
        let mut task = MeetingTask::new("rec-1".to_string(), "zh".to_string());
        task.started_at = Some("2026-08-05T02:43:42Z".to_string());
        task.ended_at = Some("2026-08-05T02:44:32Z".to_string());

        let expected = recordings_root.join("2026-08-05");
        assert!(!expected.exists());
        store.ensure_transcript_output_directory(&task).unwrap();
        assert!(expected.is_dir());
        assert_eq!(
            store.transcript_text_path(&task).parent(),
            Some(expected.as_path())
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn notes_prompt_must_fit_the_editable_limit() {
        let mut settings = MeetingSettings::default();
        settings.notes_prompt = "会".repeat(601);
        assert_eq!(
            validate_meeting_settings(&settings),
            Err("会议纪要 Prompt 需为 1 到 600 个字".to_string())
        );
    }

    #[test]
    fn rfc3339_roundtrip() {
        let now = now_rfc3339();
        assert!(parse_rfc3339_millis(&now).is_some());
        assert!(parse_rfc3339_millis("not-a-time").is_none());
    }

    #[test]
    fn legacy_numeric_server_record_id_migrates_without_precision_loss() {
        let submission: ServerSubmission = serde_json::from_value(serde_json::json!({
            "attempts": 2,
            "serverRecordId": 342862274794627072_i64,
            "lastError": null
        }))
        .unwrap();
        assert_eq!(
            submission.server_record_id.as_deref(),
            Some("342862274794627072")
        );
    }
}
