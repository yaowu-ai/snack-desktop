//! Explicit user cleanup of a recording whose source was removed outside Snack.
use std::{path::Path, sync::Mutex};

use super::super::{
    capture::Recorder,
    state::{now_rfc3339, MeetingStore, MeetingTask, TaskState},
};

/// Unknown/unreadable paths and a not-yet-created recording never count as deletion.
pub(super) fn missing_recording_audio(task: &MeetingTask) -> bool {
    task.state == TaskState::Recording
        && task
            .audio_path
            .as_deref()
            .is_some_and(|path| matches!(Path::new(path).try_exists(), Ok(false)))
}

/// The caller serializes recording start/stop/metadata updates and validates account ownership.
pub(super) fn retire_missing_recording(
    store: &MeetingStore,
    task: &MeetingTask,
    recorder: &Mutex<Option<Recorder>>,
) -> Result<(), String> {
    release_missing_capture(task, recorder)?;
    let mut retired = task.clone();
    retired.state = TaskState::Idle;
    retired.audio_bytes = None;
    retired.audio_capture_issue = None;
    retired.error = None;
    retired.updated_at = now_rfc3339();
    retired
        .ended_at
        .get_or_insert_with(|| retired.updated_at.clone());
    store.save_task_progress(&retired)?;
    store.delete_current_task_if(&task.recording_id)
}

/// A different recorder is left running; stop joins the matching capture's existing thread.
fn release_missing_capture(
    task: &MeetingTask,
    recorder: &Mutex<Option<Recorder>>,
) -> Result<(), String> {
    let mut current = recorder.lock().map_err(|_| "无法读取录音状态，请重试")?;
    if !missing_recording_audio(task) {
        return Err("录音文件状态已变化，请刷新后重试".into());
    }
    let matches = current.as_ref().is_some_and(|value| {
        Some(value.audio_path.as_path()) == task.audio_path.as_deref().map(Path::new)
    });
    if matches {
        // stop() returns the expected missing-file error after joining capture.
        if let Some(value) = current.take() {
            let _ = value.stop();
        }
    }
    if !missing_recording_audio(task) {
        return Err("录音文件已恢复，请刷新后重试".into());
    }
    Ok(())
}
