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

use std::collections::{HashMap, HashSet};
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
    pub(crate) recording_reminder_enabled: bool,
    #[serde(default)]
    pub(crate) auto_generate_notes_enabled: bool,
    #[serde(default)]
    pub(crate) organize_transcripts_by_date: bool,
    #[serde(default = "default_meeting_notes_prompt")]
    pub(crate) notes_prompt: String,
}

impl Default for MeetingSettings {
    fn default() -> Self {
        Self {
            storage_directory: None,
            shortcut: "CommandOrControl+R".to_string(),
            retain_audio: true,
            recording_reminder_enabled: false,
            auto_generate_notes_enabled: false,
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
    #[serde(default)]
    pub(crate) stage: Option<String>,
    pub(crate) downloaded_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) speed_bytes_per_sec: u64,
    pub(crate) percent: u8,
    #[serde(default)]
    pub(crate) remaining_seconds: Option<u64>,
}

impl Default for DownloadProgress {
    fn default() -> Self {
        Self {
            stage: None,
            downloaded_bytes: 0,
            total_bytes: 0,
            speed_bytes_per_sec: 0,
            percent: 0,
            remaining_seconds: None,
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
    TranscriptionPaused,
    NoAudioDetected,
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

    pub(crate) fn is_active(self) -> bool {
        matches!(
            self,
            TaskState::Checking
                | TaskState::Recording
                | TaskState::Finalizing
                | TaskState::TranscribingLocal
                | TaskState::TranscriptionPaused
                | TaskState::GeneratingNotes
        )
    }

    pub(crate) fn blocks_recording(self) -> bool {
        matches!(
            self,
            TaskState::Checking | TaskState::Recording | TaskState::Finalizing
        )
    }

    pub(crate) fn blocks_site_switch(self) -> bool {
        matches!(
            self,
            TaskState::Checking
                | TaskState::Recording
                | TaskState::Finalizing
                | TaskState::GeneratingNotes
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
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) auto_generate_notes_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) notes_project_id: Option<String>,
    #[serde(default)]
    pub(crate) notes_project_name: Option<String>,
    pub(crate) state: TaskState,
    pub(crate) started_at: Option<String>,
    pub(crate) ended_at: Option<String>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) language: String,
    pub(crate) audio_path: Option<String>,
    pub(crate) audio_bytes: Option<u64>,
    #[serde(default = "default_audio_file_owned")]
    pub(crate) audio_file_owned: bool,
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
            display_name: None,
            auto_generate_notes_enabled: None,
            notes_project_id: None,
            notes_project_name: None,
            state: TaskState::Idle,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            language,
            audio_path: None,
            audio_bytes: None,
            audio_file_owned: true,
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

fn default_audio_file_owned() -> bool {
    true
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
            .map_err(|error| error.to_string())?;
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
        let file_name = task
            .display_name
            .as_deref()
            .and_then(|name| normalize_transcript_file_name(name).ok())
            .unwrap_or_else(|| {
                let created_at = chrono::DateTime::parse_from_rfc3339(&task.created_at)
                    .map(|time| {
                        time.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d-%H%M%S")
                            .to_string()
                    })
                    .unwrap_or_else(|_| task.recording_id.clone());
                format!("Snack会议-{created_at}.txt")
            });
        self.transcript_text_root(task).join(file_name)
    }

    pub(crate) fn transcript_display_stem(&self, task: &MeetingTask) -> String {
        self.transcript_text_path(task)
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Snack会议".to_string())
    }

    /// Resolve a non-destructive transcript path. User-owned files in the
    /// selected directory are never overwritten by a new recording.
    pub(crate) fn available_transcript_text_path(&self, task: &MeetingTask) -> PathBuf {
        let requested = self.transcript_text_path(task);
        if !requested.exists() {
            return requested;
        }
        let parent = requested.parent().map(PathBuf::from).unwrap_or_default();
        let stem = requested
            .file_stem()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Snack会议".to_string());
        for index in 2..=10_000 {
            let candidate = parent.join(format!("{stem} ({index}).txt"));
            if !candidate.exists() {
                return candidate;
            }
        }
        parent.join(format!("{stem}-{}.txt", unix_millis()))
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
            root
        }
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

    /// Clear completed task records only. Active recording/transcription work
    /// must remain indexed so its background worker can keep updating it.
    /// User-owned WAV/TXT/transcript files are deliberately left untouched.
    pub(crate) fn clear_task_records(&self) -> Result<usize, String> {
        let mut cleared = 0usize;
        let entries =
            fs::read_dir(self.root.join(TASKS_DIR_NAME)).map_err(|error| error.to_string())?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<MeetingTask>(&bytes).ok())
                .is_some_and(|task| task.state.is_active())
            {
                continue;
            }
            fs::remove_file(&path).map_err(|error| error.to_string())?;
            cleared += 1;
        }
        if !self.load_task().is_some_and(|task| task.state.is_active()) {
            match fs::remove_file(self.root.join(TASK_FILE)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(cleared)
    }

    /// Keep only the newest local audio copies while preserving transcripts and
    /// record metadata. Cleared task lists are covered by scanning the current
    /// audio directory directly.
    pub(crate) fn prune_recording_audio(&self, keep: usize) -> Result<usize, String> {
        let tasks = self.load_task_records();
        let candidates = self.recording_audio_candidates(&tasks);
        let protected = protected_audio_paths(&tasks);
        let removed = remove_old_recording_audio(candidates, &protected, keep)?;
        self.clear_removed_audio_references(&tasks, &removed)?;
        Ok(removed.len())
    }

    fn recording_audio_candidates(&self, tasks: &[MeetingTask]) -> HashMap<PathBuf, u64> {
        let mut candidates = HashMap::new();
        let mut directories = HashSet::from([self.recordings_root().join(AUDIO_DIR_NAME)]);
        for task in tasks.iter().filter(|task| task.audio_file_owned) {
            let Some(path) = task.audio_path.as_deref().map(PathBuf::from) else {
                continue;
            };
            if let Some(parent) = path.parent() {
                directories.insert(parent.to_path_buf());
            }
            insert_newest_timestamp(&mut candidates, path, &task.created_at);
        }
        scan_audio_directories(&mut candidates, directories);
        candidates
    }

    fn clear_removed_audio_references(
        &self,
        tasks: &[MeetingTask],
        removed: &HashSet<PathBuf>,
    ) -> Result<(), String> {
        for task in tasks {
            if !task
                .audio_path
                .as_deref()
                .map(PathBuf::from)
                .is_some_and(|path| removed.contains(&path))
            {
                continue;
            }
            let mut next = task.clone();
            next.audio_path = None;
            next.audio_bytes = None;
            self.save_task_record(&next)?;
        }
        if let Some(mut current) = self.load_task() {
            if current
                .audio_path
                .as_deref()
                .map(PathBuf::from)
                .is_some_and(|path| removed.contains(&path))
            {
                current.audio_path = None;
                current.audio_bytes = None;
                self.save_task(&current)?;
            }
        }
        Ok(())
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
        let path = self.available_transcript_text_path(task);
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
        let legacy_default = self.default_recordings_root.join("Snack会议");
        if settings
            .storage_directory
            .as_deref()
            .is_some_and(|path| PathBuf::from(path) == legacy_default)
        {
            settings.storage_directory =
                Some(self.default_recordings_root.to_string_lossy().into());
        }
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
        if settings.shortcut == "CommandOrControl+Shift+R" {
            settings.shortcut = "CommandOrControl+R".to_string();
        }
        settings
    }

    pub(crate) fn prepare_settings(&self, settings: &MeetingSettings) -> Result<(), String> {
        validate_meeting_settings(settings)?;
        let recordings_root = self.recordings_root_for_settings(settings);
        if !recordings_root.is_absolute() {
            return Err("录音保存位置必须是完整的文件夹路径".to_string());
        }
        if !recordings_root.is_dir() {
            return Err("录音保存位置不存在或不是文件夹".to_string());
        }
        Ok(())
    }

    pub(crate) fn save_settings(&self, settings: &MeetingSettings) -> Result<(), String> {
        persist_json_atomic(&self.root.join(SETTINGS_FILE), settings)
    }

    pub(crate) fn delete_task_file(&self) {
        let _ = fs::remove_file(self.root.join(TASK_FILE));
    }

    pub(crate) fn delete_task_record(&self, recording_id: &str) -> Result<(), String> {
        match fs::remove_file(self.task_record_path(recording_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        if self
            .load_task()
            .is_some_and(|task| task.recording_id == recording_id)
        {
            self.delete_task_file();
        }
        Ok(())
    }

    fn task_record_path(&self, recording_id: &str) -> PathBuf {
        self.root
            .join(TASKS_DIR_NAME)
            .join(format!("{recording_id}.json"))
    }
}

fn insert_newest_timestamp(
    candidates: &mut HashMap<PathBuf, u64>,
    path: PathBuf,
    created_at: &str,
) {
    let timestamp = parse_rfc3339_millis(created_at).unwrap_or(0);
    candidates
        .entry(path)
        .and_modify(|current| *current = (*current).max(timestamp))
        .or_insert(timestamp);
}

fn scan_audio_directories(candidates: &mut HashMap<PathBuf, u64>, directories: HashSet<PathBuf>) {
    for directory in directories {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            insert_scanned_audio(candidates, entry);
        }
    }
}

fn insert_scanned_audio(candidates: &mut HashMap<PathBuf, u64>, entry: fs::DirEntry) {
    let path = entry.path();
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(is_supported_audio_extension)
    {
        return;
    }
    candidates.entry(path).or_insert_with(|| {
        entry
            .metadata()
            .and_then(|value| value.modified())
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0)
    });
}

fn protected_audio_paths(tasks: &[MeetingTask]) -> HashSet<PathBuf> {
    tasks
        .iter()
        .filter(|task| task.state.is_active() || !task.audio_file_owned)
        .filter_map(|task| task.audio_path.as_deref().map(PathBuf::from))
        .collect()
}

fn remove_old_recording_audio(
    candidates: HashMap<PathBuf, u64>,
    protected: &HashSet<PathBuf>,
    keep: usize,
) -> Result<HashSet<PathBuf>, String> {
    let mut ordered = candidates
        .into_iter()
        .filter(|(path, _)| !protected.contains(path))
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(&left.1));
    let mut removed = HashSet::new();
    for (path, _) in ordered.into_iter().skip(keep) {
        remove_recording_audio(path, &mut removed)?;
    }
    Ok(removed)
}

fn remove_recording_audio(path: PathBuf, removed: &mut HashSet<PathBuf>) -> Result<(), String> {
    match fs::remove_file(&path) {
        Ok(()) => {
            removed.insert(path);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            removed.insert(path);
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn is_supported_audio_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "wav" | "mp3" | "m4a" | "aac" | "flac" | "ogg" | "opus" | "wma"
    )
}

/// Normalize a user-facing record name to the transcript file name written to
/// the chosen local directory. Names may not escape that directory.
pub(crate) fn normalize_transcript_file_name(value: &str) -> Result<String, String> {
    let name = value.trim();
    if name.is_empty() {
        return Err("文件名不能为空".to_string());
    }
    if name.chars().any(is_invalid_file_name_character) || matches!(name, "." | "..") {
        return Err("文件名包含系统不支持的字符".to_string());
    }
    let without_txt = name
        .strip_suffix(".txt")
        .or_else(|| name.strip_suffix(".TXT"))
        .unwrap_or(name)
        .trim();
    let stem = without_txt
        .rsplit_once('.')
        .filter(|(_, extension)| is_supported_audio_extension(extension))
        .map(|(base, _)| base)
        .unwrap_or(without_txt)
        .trim();
    if stem.is_empty() || matches!(stem, "." | "..") {
        return Err("文件名不能为空".to_string());
    }
    let stem = stem.trim_end_matches([' ', '.']);
    if stem.is_empty() || is_windows_reserved_file_name(stem) {
        return Err("文件名不可用，请换一个名称".to_string());
    }
    if stem.len() > 200 {
        return Err("文件名过长，请缩短后重试".to_string());
    }
    Ok(format!("{stem}.txt"))
}

fn is_invalid_file_name_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
        )
}

fn is_windows_reserved_file_name(stem: &str) -> bool {
    let upper = stem.split('.').next().unwrap_or(stem).to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
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
        now_rfc3339, parse_rfc3339_millis, validate_meeting_settings, DownloadProgress,
        MeetingSettings, MeetingStore, MeetingTask, ResourceState, ResourceStatus,
        ServerSubmission, TaskState, DEFAULT_MEETING_NOTES_PROMPT,
    };
    use std::fs;

    #[test]
    fn resource_defaults_to_not_installed() {
        let status = ResourceStatus::default();
        assert_eq!(status.state, ResourceState::NotInstalled);
        assert!(status.error.is_none());
    }

    #[test]
    fn legacy_download_progress_defaults_new_feedback_fields() {
        let progress: DownloadProgress = serde_json::from_str(
            r#"{"downloadedBytes":10,"totalBytes":100,"speedBytesPerSec":5,"percent":10}"#,
        )
        .unwrap();
        assert_eq!(progress.stage, None);
        assert_eq!(progress.remaining_seconds, None);
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
        assert!(task.audio_file_owned);
        assert!(TaskState::TranscriptionFailed.is_terminal_error());
        assert!(!TaskState::TranscribingLocal.is_terminal_error());
        assert!(TaskState::TranscriptionPaused.is_active());
        assert!(!TaskState::NoAudioDetected.is_terminal_error());
        assert!(!TaskState::NoAudioDetected.is_active());
        assert!(!TaskState::Idle.blocks_recording());
        assert!(!TaskState::NoAudioDetected.blocks_recording());
        assert!(!TaskState::TranscriptReady.blocks_recording());
        assert!(!TaskState::Ready.blocks_recording());
        assert!(TaskState::Recording.blocks_recording());
        assert!(!TaskState::TranscribingLocal.blocks_recording());
        assert_eq!(
            serde_json::to_value(TaskState::NoAudioDetected).unwrap(),
            "no_audio_detected"
        );
    }

    #[test]
    fn legacy_tasks_default_to_owned_audio() {
        let task = MeetingTask::new("rec-legacy".to_string(), "zh".to_string());
        let mut serialized = serde_json::to_value(task).unwrap();
        serialized.as_object_mut().unwrap().remove("audioFileOwned");

        let decoded: MeetingTask = serde_json::from_value(serialized).unwrap();

        assert!(decoded.audio_file_owned);
    }

    #[test]
    fn settings_retain_audio_by_default() {
        let settings = MeetingSettings::default();
        assert!(settings.retain_audio);
        assert!(!settings.recording_reminder_enabled);
        assert!(!settings.auto_generate_notes_enabled);
        assert!(!settings.organize_transcripts_by_date);
        assert_eq!(settings.shortcut, "CommandOrControl+R");
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
        assert!(!settings.recording_reminder_enabled);
        assert!(!settings.auto_generate_notes_enabled);
        assert!(!settings.organize_transcripts_by_date);
    }

    #[test]
    fn settings_preserve_an_explicitly_enabled_recording_reminder() {
        let settings: MeetingSettings = serde_json::from_value(serde_json::json!({
            "storageDirectory": "/tmp/meeting",
            "shortcut": "CommandOrControl+R",
            "retainAudio": true,
            "recordingReminderEnabled": true
        }))
        .unwrap();

        assert!(settings.recording_reminder_enabled);
    }

    #[test]
    fn settings_preserve_an_explicitly_enabled_automatic_notes_preference() {
        let settings: MeetingSettings = serde_json::from_value(serde_json::json!({
            "storageDirectory": "/tmp/meeting",
            "shortcut": "CommandOrControl+R",
            "retainAudio": true,
            "autoGenerateNotesEnabled": true
        }))
        .unwrap();

        assert!(settings.auto_generate_notes_enabled);
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
    fn transcript_json_directory_is_created_on_first_persistence() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-transcript-json-{}",
            super::unix_millis()
        ));
        let transcript_path = root.join(super::TRANSCRIPTS_DIR_NAME).join("rec-1.json");

        assert!(!transcript_path.parent().unwrap().exists());
        super::persist_json_atomic(&transcript_path, &serde_json::json!({ "text": "ok" })).unwrap();
        assert!(transcript_path.is_file());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transcript_without_date_organization_is_written_to_the_selected_directory() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-transcript-root-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let recordings_root = root.join("selected");
        fs::create_dir_all(&state_root).unwrap();
        fs::create_dir_all(&recordings_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: root.join("Desktop"),
        };
        let mut settings = MeetingSettings::default();
        settings.storage_directory = Some(recordings_root.to_string_lossy().into_owned());
        store.save_settings(&settings).unwrap();
        let task = MeetingTask::new("rec-1".to_string(), "zh".to_string());

        store.ensure_transcript_output_directory(&task).unwrap();
        assert_eq!(
            store.transcript_text_path(&task).parent(),
            Some(recordings_root.as_path())
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transcript_name_uses_the_renamed_text_file_name() {
        assert_eq!(
            super::normalize_transcript_file_name("产品周会.txt").unwrap(),
            "产品周会.txt"
        );
        assert_eq!(
            super::normalize_transcript_file_name("产品周会.wav").unwrap(),
            "产品周会.txt"
        );
        assert!(super::normalize_transcript_file_name("../产品周会").is_err());
        assert!(super::normalize_transcript_file_name("CON").is_err());
        assert!(super::normalize_transcript_file_name("周会:复盘").is_err());
        assert!(super::normalize_transcript_file_name(&"会".repeat(100)).is_err());
    }

    #[test]
    fn transcript_path_never_overwrites_an_existing_user_file() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-unique-transcript-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let recordings_root = root.join("selected");
        fs::create_dir_all(&state_root).unwrap();
        fs::create_dir_all(&recordings_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: recordings_root.clone(),
        };
        let mut settings = MeetingSettings::default();
        settings.storage_directory = Some(recordings_root.to_string_lossy().into_owned());
        store.save_settings(&settings).unwrap();
        let mut task = MeetingTask::new("rec-unique".to_string(), "zh".to_string());
        task.display_name = Some("产品周会.txt".to_string());
        fs::write(recordings_root.join("产品周会.txt"), "已有内容").unwrap();

        assert_eq!(
            store.available_transcript_text_path(&task),
            recordings_root.join("产品周会 (2).txt")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validating_desktop_storage_does_not_create_recording_folders() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-lazy-storage-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let desktop_root = root.join("Desktop");
        fs::create_dir_all(&state_root).unwrap();
        fs::create_dir_all(&desktop_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: desktop_root.clone(),
        };
        let mut settings = MeetingSettings::default();
        settings.storage_directory = Some(desktop_root.to_string_lossy().into_owned());

        store.prepare_settings(&settings).unwrap();
        assert!(!desktop_root.join(super::AUDIO_DIR_NAME).exists());
        assert!(!desktop_root.join(super::TRANSCRIPTS_DIR_NAME).exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clearing_task_records_preserves_local_audio_and_transcripts() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-clear-records-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let recordings_root = root.join("recordings");
        fs::create_dir_all(state_root.join(super::TASKS_DIR_NAME)).unwrap();
        fs::create_dir_all(&recordings_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: recordings_root.clone(),
        };
        let audio_path = recordings_root.join("rec-1.wav");
        let transcript_path = recordings_root.join("rec-1.txt");
        fs::write(&audio_path, b"audio").unwrap();
        fs::write(&transcript_path, b"transcript").unwrap();
        let mut task = MeetingTask::new("rec-1".to_string(), "zh".to_string());
        task.audio_path = Some(audio_path.to_string_lossy().into_owned());
        task.transcript_path = Some(transcript_path.to_string_lossy().into_owned());
        store.save_task(&task).unwrap();

        assert_eq!(store.clear_task_records().unwrap(), 1);
        assert!(store.load_task().is_none());
        assert!(store.load_task_records().is_empty());
        assert!(audio_path.exists());
        assert!(transcript_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clearing_task_records_keeps_active_transcriptions() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-clear-active-records-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let recordings_root = root.join("recordings");
        fs::create_dir_all(state_root.join(super::TASKS_DIR_NAME)).unwrap();
        fs::create_dir_all(&recordings_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: recordings_root,
        };
        let completed = MeetingTask::new("completed".to_string(), "zh".to_string());
        store.save_task(&completed).unwrap();
        let mut transcribing = MeetingTask::new("transcribing".to_string(), "zh".to_string());
        transcribing.state = TaskState::TranscribingLocal;
        store.save_task_record(&transcribing).unwrap();

        assert_eq!(store.clear_task_records().unwrap(), 1);
        assert!(store.load_task().is_none());
        assert!(store.load_task_record("completed").is_none());
        assert_eq!(
            store.load_task_record("transcribing").unwrap().state,
            TaskState::TranscribingLocal
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleting_a_paused_task_record_preserves_its_audio_file() {
        let root =
            std::env::temp_dir().join(format!("snack-delete-paused-task-{}", super::unix_millis()));
        let state_root = root.join("state");
        fs::create_dir_all(state_root.join(super::TASKS_DIR_NAME)).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: root.clone(),
        };
        let audio_path = root.join("recording.wav");
        fs::write(&audio_path, b"audio").unwrap();
        let mut task = MeetingTask::new("paused".to_string(), "zh".to_string());
        task.state = TaskState::TranscriptionPaused;
        task.audio_path = Some(audio_path.to_string_lossy().into_owned());
        store.save_task_record(&task).unwrap();

        store.delete_task_record("paused").unwrap();

        assert!(store.load_task_record("paused").is_none());
        assert!(audio_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audio_retention_keeps_the_latest_ten_without_deleting_transcripts() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-audio-retention-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let recordings_root = root.join("recordings");
        let audio_root = recordings_root.join(super::AUDIO_DIR_NAME);
        let transcript_root = recordings_root.join(super::TRANSCRIPTS_DIR_NAME);
        fs::create_dir_all(state_root.join(super::TASKS_DIR_NAME)).unwrap();
        fs::create_dir_all(&audio_root).unwrap();
        fs::create_dir_all(&transcript_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: recordings_root,
        };

        let mut audio_paths = Vec::new();
        let mut transcript_paths = Vec::new();
        for index in 0..12 {
            let audio_path = audio_root.join(format!("rec-{index}.wav"));
            let transcript_path = transcript_root.join(format!("rec-{index}.txt"));
            fs::write(&audio_path, format!("audio-{index}")).unwrap();
            fs::write(&transcript_path, format!("transcript-{index}")).unwrap();
            let mut task = MeetingTask::new(format!("rec-{index}"), "zh".to_string());
            task.created_at = format!("2026-08-06T10:00:{index:02}Z");
            task.updated_at = task.created_at.clone();
            task.audio_path = Some(audio_path.to_string_lossy().into_owned());
            task.audio_bytes = Some(7);
            task.transcript_path = Some(transcript_path.to_string_lossy().into_owned());
            store.save_task_record(&task).unwrap();
            audio_paths.push(audio_path);
            transcript_paths.push(transcript_path);
        }

        assert_eq!(store.prune_recording_audio(10).unwrap(), 2);
        assert!(!audio_paths[0].exists());
        assert!(!audio_paths[1].exists());
        assert!(audio_paths[2..].iter().all(|path| path.exists()));
        assert!(transcript_paths.iter().all(|path| path.exists()));
        assert!(store
            .load_task_record("rec-0")
            .unwrap()
            .audio_path
            .is_none());
        assert!(store
            .load_task_record("rec-1")
            .unwrap()
            .audio_path
            .is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audio_retention_never_deletes_referenced_imports() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-import-retention-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let recordings_root = root.join("recordings");
        let audio_root = recordings_root.join(super::AUDIO_DIR_NAME);
        let external_root = root.join("user-audio");
        fs::create_dir_all(state_root.join(super::TASKS_DIR_NAME)).unwrap();
        fs::create_dir_all(&audio_root).unwrap();
        fs::create_dir_all(&external_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: recordings_root,
        };
        let managed_reference = audio_root.join("selected.wav");
        let external_reference = external_root.join("selected.mp3");
        let external_sibling = external_root.join("unrelated.mp3");
        fs::write(&managed_reference, b"managed").unwrap();
        fs::write(&external_reference, b"external").unwrap();
        fs::write(&external_sibling, b"unrelated").unwrap();

        for (recording_id, path) in [
            ("managed-reference", &managed_reference),
            ("external-reference", &external_reference),
        ] {
            let mut task = MeetingTask::new(recording_id.to_string(), "zh".to_string());
            task.state = TaskState::TranscriptReady;
            task.audio_path = Some(path.to_string_lossy().into_owned());
            task.audio_file_owned = false;
            store.save_task_record(&task).unwrap();
        }

        assert_eq!(store.prune_recording_audio(0).unwrap(), 0);
        assert!(managed_reference.exists());
        assert!(external_reference.exists());
        assert!(external_sibling.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audio_retention_protects_a_recording_in_progress() {
        let root = std::env::temp_dir().join(format!(
            "snack-meeting-active-audio-{}",
            super::unix_millis()
        ));
        let state_root = root.join("state");
        let recordings_root = root.join("recordings");
        let audio_root = recordings_root.join(super::AUDIO_DIR_NAME);
        fs::create_dir_all(state_root.join(super::TASKS_DIR_NAME)).unwrap();
        fs::create_dir_all(&audio_root).unwrap();
        let store = MeetingStore {
            root: state_root,
            default_recordings_root: recordings_root,
        };
        let audio_path = audio_root.join("active.wav");
        fs::write(&audio_path, b"active").unwrap();
        let mut task = MeetingTask::new("active".to_string(), "zh".to_string());
        task.state = TaskState::Recording;
        task.audio_path = Some(audio_path.to_string_lossy().into_owned());
        store.save_task_record(&task).unwrap();

        assert_eq!(store.prune_recording_audio(0).unwrap(), 0);
        assert!(audio_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imported_audio_formats_are_supported() {
        assert!(super::is_supported_audio_extension("MP3"));
        assert!(super::is_supported_audio_extension("m4a"));
        assert!(super::is_supported_audio_extension("wav"));
        assert!(!super::is_supported_audio_extension("txt"));
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
