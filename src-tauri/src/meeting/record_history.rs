//! List visibility is independent of the task/file index used by notes and workers.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::capture::Recorder;
use super::state::{
    persist_json_atomic, MeetingStore, MeetingTask, TaskState, TranscriptAssetState,
};

static RECORDS_LOCK: Mutex<()> = Mutex::new(());
const FILE: &str = "record-history.json";

mod missing_audio;
use missing_audio::{missing_recording_audio, retire_missing_recording};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Records {
    revision: u64,
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    hidden: bool,
    binding: Option<NoteBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NoteBinding {
    pub(crate) note_key: String,
    pub(crate) account_key: String,
    pub(crate) site_origin: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordHistoryStatus {
    pub(crate) visible: bool,
    pub(crate) clearable: bool,
    pub(crate) retained_reason: Option<&'static str>,
    pub(crate) missing_recording_audio: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClearHistoryResult {
    pub(crate) cleared_ids: Vec<String>,
    pub(crate) retained_ids: Vec<String>,
    pub(crate) failed_ids: Vec<String>,
}

/// The encoded origin may differ between localhost and the same account's website.
pub(crate) fn same_account(left: &str, right: &str) -> bool {
    let identity = |value: &str| {
        value
            .rsplit(':')
            .take(2)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let left = identity(left);
    left.len() == 2 && left == identity(right)
}

fn owns(task: &MeetingTask, account: Option<&str>) -> bool {
    task.recording_account_key
        .as_deref()
        .is_none_or(|owner| account.is_some_and(|current| same_account(owner, current)))
}

fn retained_reason(task: &MeetingTask) -> Option<&'static str> {
    if matches!(
        task.transcript_asset_state,
        TranscriptAssetState::Pending | TranscriptAssetState::Uploading
    ) {
        return Some("正在或排队保存到项目");
    }
    if missing_recording_audio(task) {
        return None;
    }
    match task.state {
        TaskState::Checking | TaskState::Recording | TaskState::Finalizing => Some("录音尚未结束"),
        TaskState::AwaitingTranscription => Some("等待转写或模型下载"),
        TaskState::TranscribingLocal => Some("正在转写"),
        TaskState::TranscriptionPaused => Some("转写已暂停，尚未结束"),
        TaskState::GeneratingNotes | TaskState::WaitingForNetwork => Some("纪要仍在处理中"),
        _ => None,
    }
}

impl MeetingStore {
    fn read_history(&self) -> Result<Records, String> {
        match fs::read(self.root().join(FILE)) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| "无法读取录音记录状态，请重试".into())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Records::default()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn save_history(&self, records: &mut Records) -> Result<(), String> {
        records.revision += 1;
        persist_json_atomic(&self.root().join(FILE), records)
    }

    pub(crate) fn decorate_record_history(
        &self,
        tasks: &mut [MeetingTask],
        account: Option<&str>,
    ) -> u64 {
        let _guard = RECORDS_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let records = self.read_history();
        for task in tasks {
            let visible = owns(task, account)
                && records
                    .as_ref()
                    .ok()
                    .and_then(|value| value.entries.get(&task.recording_id))
                    .is_none_or(|entry| !entry.hidden);
            let reason = if records.is_err() {
                Some("无法读取记录状态")
            } else {
                retained_reason(task)
            };
            task.history = Some(RecordHistoryStatus {
                visible,
                clearable: visible && reason.is_none(),
                retained_reason: reason,
                missing_recording_audio: missing_recording_audio(task),
            });
        }
        records.map(|value| value.revision).unwrap_or_default()
    }

    /// Revalidate IDs and release only confirmed missing-audio recordings before hiding them.
    pub(crate) fn clear_record_history(
        &self,
        ids: &[String],
        account: Option<&str>,
        recorder: &Mutex<Option<Recorder>>,
    ) -> Result<ClearHistoryResult, String> {
        let tasks = self.load_task_records();
        let tasks: BTreeMap<_, _> = tasks
            .iter()
            .map(|task| (task.recording_id.as_str(), task))
            .collect();
        let _guard = RECORDS_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut records = self.read_history()?;
        let mut result = ClearHistoryResult::default();
        for id in ids.iter().collect::<BTreeSet<_>>() {
            let Some(task) = tasks.get(id.as_str()) else {
                result.failed_ids.push(id.clone());
                continue;
            };
            if !owns(task, account) {
                return Err("账号已切换，请重新打开录音记录".into());
            }
            if retained_reason(task).is_some() {
                result.retained_ids.push(id.clone());
                continue;
            }
            if missing_recording_audio(task)
                && retire_missing_recording(self, task, recorder).is_err()
            {
                result.failed_ids.push(id.clone());
                continue;
            }
            let entry = records.entries.entry(id.clone()).or_default();
            if !entry.hidden {
                entry.hidden = true;
                result.cleared_ids.push(id.clone());
            }
        }
        if !result.cleared_ids.is_empty() {
            self.save_history(&mut records)?;
        }
        Ok(result)
    }

    /// Only an explicit new transcription attempt makes an old list entry visible again.
    pub(crate) fn show_record_history(&self, recording_id: &str) -> Result<(), String> {
        let _guard = RECORDS_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut records = self.read_history()?;
        if let Some(entry) = records
            .entries
            .get_mut(recording_id)
            .filter(|entry| entry.hidden)
        {
            entry.hidden = false;
            self.save_history(&mut records)?;
        }
        Ok(())
    }

    pub(crate) fn bind_record_note(
        &self,
        recording_id: &str,
        binding: NoteBinding,
    ) -> Result<(), String> {
        let task = self
            .load_task_record(recording_id)
            .ok_or("没有找到本地录音")?;
        if !owns(&task, Some(&binding.account_key)) {
            return Err("这条录音属于其他账号".into());
        }
        let _guard = RECORDS_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut records = self.read_history()?;
        let entry = records.entries.entry(recording_id.into()).or_default();
        if let Some(existing) = &entry.binding {
            if existing.note_key != binding.note_key
                || existing.site_origin != binding.site_origin
                || !same_account(&existing.account_key, &binding.account_key)
            {
                return Err("录音已关联其他笔记或站点".into());
            }
            return Ok(());
        }
        entry.binding = Some(binding);
        self.save_history(&mut records)
    }

    pub(crate) fn read_record_note(
        &self,
        recording_id: &str,
        account: &str,
        site: &str,
    ) -> Result<Option<String>, String> {
        let _guard = RECORDS_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let records = self.read_history()?;
        let Some(binding) = records
            .entries
            .get(recording_id)
            .and_then(|entry| entry.binding.as_ref())
        else {
            return Ok(None);
        };
        if !same_account(account, &binding.account_key) {
            return Err("这条录音属于其他账号".into());
        }
        if binding.site_origin != site {
            return Err("请切换到录制时的站点查看笔记".into());
        }
        Ok(Some(binding.note_key.clone()))
    }
}

#[cfg(test)]
mod tests;
