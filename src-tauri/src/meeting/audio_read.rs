//! Bounded audio reads for cloud synchronization, addressed only by native recording ID.

use std::fs::{self, File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use super::state::{MeetingTask, TaskState};
use super::MeetingManagerState;

const MAX_CHUNK_BYTES: usize = 512 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AudioReadRequest {
    recording_id: String,
    offset: u64,
    length: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AudioReadResponse {
    base64: String,
    offset: u64,
    total_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct AudioReadError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl AudioReadError {
    fn new(code: &'static str, message: &str) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

pub(crate) async fn read(
    app: &AppHandle,
    request: AudioReadRequest,
) -> Result<AudioReadResponse, AudioReadError> {
    let store = app.state::<MeetingManagerState>().store.clone();
    tauri::async_runtime::spawn_blocking(move || {
        read_chunk(
            &request,
            |id| store.load_task_record(id),
            || store.load_task(),
        )
    })
    .await
    .map_err(|_| AudioReadError::new("RECORDING_AUDIO_READ_FAILED", "读取本机录音失败，请重试"))?
}

fn read_chunk(
    request: &AudioReadRequest,
    load_record: impl Fn(&str) -> Option<MeetingTask>,
    load_current: impl Fn() -> Option<MeetingTask>,
) -> Result<AudioReadResponse, AudioReadError> {
    validate_request(request)?;
    let task = load_record(&request.recording_id).ok_or_else(recording_not_found)?;
    if task.recording_id != request.recording_id {
        return Err(recording_not_found());
    }
    ensure_finalized(&task, load_current().as_ref())?;
    let path = task.audio_path.as_deref().ok_or_else(audio_not_found)?;
    let expected_bytes = task.audio_bytes.filter(|bytes| *bytes > 0).ok_or_else(|| {
        AudioReadError::new("RECORDING_AUDIO_NOT_READY", "录音尚未完成保存，请稍后重试")
    })?;
    let path = Path::new(path);
    let before_path = fs::metadata(path).map_err(read_error)?;
    if !before_path.is_file() {
        return Err(audio_not_found());
    }
    let mut file = File::open(path).map_err(read_error)?;
    let before = file.metadata().map_err(read_error)?;
    if before.len() != expected_bytes || !same_file_snapshot(&before_path, &before) {
        return Err(audio_changed());
    }
    if request.offset > before.len() {
        return Err(invalid_range());
    }
    let size = (before.len() - request.offset).min(request.length as u64) as usize;
    let mut bytes = vec![0; size];
    file.seek(SeekFrom::Start(request.offset))
        .map_err(read_error)?;
    file.read_exact(&mut bytes).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            audio_changed()
        } else {
            read_error(error)
        }
    })?;

    // Do not return bytes from a task being replaced, removed, or reopened for capture.
    let current = load_record(&request.recording_id).ok_or_else(audio_changed)?;
    ensure_finalized(&current, load_current().as_ref())?;
    if current.recording_id != task.recording_id
        || current.audio_path != task.audio_path
        || current.audio_bytes != task.audio_bytes
        || !same_file_snapshot(&before, &file.metadata().map_err(read_error)?)
        || !same_file_snapshot(&before, &fs::metadata(path).map_err(read_error)?)
    {
        return Err(audio_changed());
    }
    Ok(AudioReadResponse {
        base64: STANDARD.encode(bytes),
        offset: request.offset,
        total_bytes: before.len(),
    })
}

fn validate_request(request: &AudioReadRequest) -> Result<(), AudioReadError> {
    // Native IDs contain only these characters; never join a caller-supplied path to the store.
    if request.recording_id.is_empty()
        || request.recording_id.len() > 128
        || !request
            .recording_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(AudioReadError::new("INVALID_RECORDING_ID", "录音标识无效"));
    }
    if request.length == 0 || request.length > MAX_CHUNK_BYTES {
        return Err(invalid_range());
    }
    Ok(())
}

fn ensure_finalized(
    task: &MeetingTask,
    current: Option<&MeetingTask>,
) -> Result<(), AudioReadError> {
    let writing = |task: &MeetingTask| {
        matches!(
            task.state,
            TaskState::Checking
                | TaskState::Recording
                | TaskState::Finalizing
                | TaskState::FinalizeFailed
        )
    };
    let same_active_file = current.is_some_and(|current| {
        writing(current)
            && (current.recording_id == task.recording_id
                || (task.audio_path.is_some() && current.audio_path == task.audio_path))
    });
    if writing(task) || same_active_file {
        return Err(AudioReadError::new(
            "RECORDING_AUDIO_NOT_READY",
            "录音仍在保存中，完成后可重试同步",
        ));
    }
    Ok(())
}

fn same_file_snapshot(before: &Metadata, after: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return false;
        }
    }
    before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && before.created().ok() == after.created().ok()
}

fn recording_not_found() -> AudioReadError {
    AudioReadError::new("RECORDING_NOT_FOUND", "没有找到这条本机录音")
}

fn audio_not_found() -> AudioReadError {
    AudioReadError::new(
        "RECORDING_AUDIO_NOT_FOUND",
        "本机录音文件不存在，可能已被移动或清理",
    )
}

fn audio_changed() -> AudioReadError {
    AudioReadError::new("RECORDING_AUDIO_CHANGED", "录音文件已变更，请重新同步")
}

fn invalid_range() -> AudioReadError {
    AudioReadError::new("INVALID_AUDIO_RANGE", "录音读取范围无效")
}

fn read_error(error: std::io::Error) -> AudioReadError {
    if error.kind() == std::io::ErrorKind::NotFound {
        audio_not_found()
    } else {
        AudioReadError::new(
            "RECORDING_AUDIO_READ_FAILED",
            "无法读取本机录音文件，请检查文件访问权限后重试",
        )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        task: MeetingTask,
    }

    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "snack-audio-read-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let path = root.join("recording.wav");
            fs::write(&path, bytes).unwrap();
            let mut task = MeetingTask::new("rec-123-456-0".to_string(), "zh".to_string());
            task.state = TaskState::TranscriptReady;
            task.audio_path = Some(path.to_string_lossy().into_owned());
            task.audio_bytes = Some(bytes.len() as u64);
            Self { root, task }
        }

        fn request(&self, offset: u64, length: usize) -> AudioReadRequest {
            AudioReadRequest {
                recording_id: self.task.recording_id.clone(),
                offset,
                length,
            }
        }

        fn read(&self, request: &AudioReadRequest) -> Result<AudioReadResponse, AudioReadError> {
            read_chunk(
                request,
                |id| (id == self.task.recording_id).then(|| self.task.clone()),
                || None,
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn reads_bounded_chunks_and_reassembles_binary_bytes() {
        let bytes: Vec<u8> = (0..MAX_CHUNK_BYTES + 17).map(|index| index as u8).collect();
        let fixture = Fixture::new(&bytes);
        let first = fixture.read(&fixture.request(0, MAX_CHUNK_BYTES)).unwrap();
        let last = fixture
            .read(&fixture.request(MAX_CHUNK_BYTES as u64, MAX_CHUNK_BYTES))
            .unwrap();
        assert_eq!(first.offset, 0);
        assert_eq!(last.offset, MAX_CHUNK_BYTES as u64);
        assert_eq!(first.total_bytes, bytes.len() as u64);
        assert_eq!(last.total_bytes, bytes.len() as u64);
        let mut rebuilt = STANDARD.decode(first.base64).unwrap();
        assert_eq!(rebuilt.len(), MAX_CHUNK_BYTES);
        rebuilt.extend(STANDARD.decode(last.base64).unwrap());
        assert_eq!(rebuilt, bytes);
    }

    #[test]
    fn eof_is_empty_but_offsets_beyond_eof_are_invalid() {
        let fixture = Fixture::new(b"abcd");
        let eof = fixture.read(&fixture.request(4, 10)).unwrap();
        assert_eq!(eof.base64, "");
        assert_eq!(eof.total_bytes, 4);
        for offset in [5, u64::MAX] {
            assert_eq!(
                fixture.read(&fixture.request(offset, 1)).unwrap_err().code,
                "INVALID_AUDIO_RANGE"
            );
        }
        for length in [0, MAX_CHUNK_BYTES + 1] {
            assert_eq!(
                fixture.read(&fixture.request(0, length)).unwrap_err().code,
                "INVALID_AUDIO_RANGE"
            );
        }
    }

    #[test]
    fn rejects_unknown_and_traversal_ids_before_accessing_the_store() {
        let fixture = Fixture::new(b"audio");
        let mut request = fixture.request(0, 2);
        request.recording_id = "unknown".to_string();
        assert_eq!(
            fixture.read(&request).unwrap_err().code,
            "RECORDING_NOT_FOUND"
        );
        for id in [
            "",
            "../settings",
            "rec/other",
            "rec\\other",
            "%2e%2e",
            ".",
            "/tmp/audio",
        ] {
            request.recording_id = id.to_string();
            let error = read_chunk(
                &request,
                |_| panic!("invalid ID reached the store"),
                || None,
            )
            .unwrap_err();
            assert_eq!(error.code, "INVALID_RECORDING_ID");
        }
    }

    #[test]
    fn rejects_arbitrary_paths_and_non_integer_ranges_in_payload() {
        for payload in [
            serde_json::json!({"recordingId":"rec-1","offset":0,"length":1,"path":"/tmp/private"}),
            serde_json::json!({"recordingId":"rec-1","offset":-1,"length":1}),
            serde_json::json!({"recordingId":"rec-1","offset":0.5,"length":1}),
        ] {
            assert!(serde_json::from_value::<AudioReadRequest>(payload).is_err());
        }
    }

    #[test]
    fn rejects_missing_files_and_metadata_size_mismatches() {
        let mut fixture = Fixture::new(b"audio");
        fixture.task.audio_bytes = Some(4);
        assert_eq!(
            fixture.read(&fixture.request(0, 2)).unwrap_err().code,
            "RECORDING_AUDIO_CHANGED"
        );
        fs::remove_file(fixture.task.audio_path.as_ref().unwrap()).unwrap();
        assert_eq!(
            fixture.read(&fixture.request(0, 2)).unwrap_err().code,
            "RECORDING_AUDIO_NOT_FOUND"
        );
        fixture.task.audio_path = None;
        assert_eq!(
            fixture.read(&fixture.request(0, 2)).unwrap_err().code,
            "RECORDING_AUDIO_NOT_FOUND"
        );
    }

    #[test]
    fn refuses_recording_and_unfinalized_files_even_with_stale_saved_metadata() {
        let mut fixture = Fixture::new(b"audio");
        for state in [
            TaskState::Checking,
            TaskState::Recording,
            TaskState::Finalizing,
            TaskState::FinalizeFailed,
        ] {
            fixture.task.state = state;
            assert_eq!(
                fixture.read(&fixture.request(0, 2)).unwrap_err().code,
                "RECORDING_AUDIO_NOT_READY"
            );
        }
        fixture.task.state = TaskState::TranscriptReady;
        let mut active = fixture.task.clone();
        active.state = TaskState::Recording;
        active.recording_id = "rec-another".to_string();
        let error = read_chunk(
            &fixture.request(0, 2),
            |_| Some(fixture.task.clone()),
            || Some(active.clone()),
        )
        .unwrap_err();
        assert_eq!(error.code, "RECORDING_AUDIO_NOT_READY");
    }

    #[test]
    fn rejects_file_changes_during_a_chunk_even_when_length_is_unchanged() {
        let fixture = Fixture::new(b"audio");
        let reads = Cell::new(0);
        let error = read_chunk(
            &fixture.request(0, 2),
            |_| {
                reads.set(reads.get() + 1);
                if reads.get() == 2 {
                    let path = fixture.task.audio_path.as_ref().unwrap();
                    fs::write(path, b"other").unwrap();
                    File::options()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
                        .unwrap();
                }
                Some(fixture.task.clone())
            },
            || None,
        )
        .unwrap_err();
        assert_eq!(error.code, "RECORDING_AUDIO_CHANGED");
    }

    #[test]
    fn rejects_native_task_changes_during_a_chunk() {
        let fixture = Fixture::new(b"audio");
        let reads = Cell::new(0);
        let error = read_chunk(
            &fixture.request(0, 2),
            |_| {
                reads.set(reads.get() + 1);
                let mut task = fixture.task.clone();
                if reads.get() == 2 {
                    task.audio_path = None;
                }
                Some(task)
            },
            || None,
        )
        .unwrap_err();
        assert_eq!(error.code, "RECORDING_AUDIO_CHANGED");
    }
}
