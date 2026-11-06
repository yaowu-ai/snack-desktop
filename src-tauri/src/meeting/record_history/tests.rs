use super::super::state::now_rfc3339;
use super::*;

const ACCOUNT: &str = "http%3A%2F%2Flocalhost%3A3000:tenant:7";

fn store() -> MeetingStore {
    let root = std::env::temp_dir().join(format!(
        "snack-history-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    MeetingStore::for_test(root.join("state"), root.join("recordings"))
}

fn task(store: &MeetingStore, id: &str, state: TaskState) -> MeetingTask {
    let mut task = MeetingTask::new(id.into(), "zh".into());
    task.state = state;
    task.defer_transcription = true;
    task.recording_account_key = Some(ACCOUNT.into());
    store.save_task_record(&task).unwrap();
    task
}

impl MeetingStore {
    fn clear_history(
        &self,
        ids: &[String],
        account: Option<&str>,
    ) -> Result<ClearHistoryResult, String> {
        self.clear_record_history(ids, account, &Mutex::new(None))
    }
}

fn visible(store: &MeetingStore) -> Vec<String> {
    let mut tasks = store.load_task_records();
    store.decorate_record_history(&mut tasks, Some(ACCOUNT));
    tasks
        .into_iter()
        .filter(|task| task.history.as_ref().unwrap().visible)
        .map(|task| task.recording_id)
        .collect()
}

#[test]
fn clears_completed_note_history_without_deleting_files_or_task_index() {
    let store = store();
    let mut old_task = task(&store, "note", TaskState::TranscriptReady);
    let audio = store.root().join("note.wav");
    let txt = store.root().join("note.txt");
    fs::write(&audio, b"audio").unwrap();
    fs::write(&txt, "原文").unwrap();
    old_task.audio_path = Some(audio.to_string_lossy().into_owned());
    old_task.transcript_path = Some(txt.to_string_lossy().into_owned());
    store.save_task(&old_task).unwrap();
    let result = store
        .clear_history(&["note".into()], Some(ACCOUNT))
        .unwrap();
    assert_eq!(result.cleared_ids, vec!["note"]);
    assert!(visible(&store).is_empty());
    assert!(store.load_task_record("note").is_some());
    assert!(store.load_task().is_some());
    assert!(audio.exists() && txt.exists());
    // A stale worker updates its old copy after the user cleared the list.
    old_task.updated_at = now_rfc3339();
    store.save_task_progress(&old_task).unwrap();
    let reopened = MeetingStore::for_test(store.root().clone(), store.root().join("recordings"));
    assert!(visible(&reopened).is_empty());
    reopened.show_record_history("note").unwrap();
    assert_eq!(visible(&reopened), vec!["note"]);
}

#[test]
fn retains_unfinished_tasks_and_revalidates_confirmed_ids() {
    let store = store();
    for (id, state) in [
        ("done", TaskState::TranscriptReady),
        ("failed", TaskState::TranscriptionFailed),
        ("waiting", TaskState::AwaitingTranscription),
        ("paused", TaskState::TranscriptionPaused),
        ("active", TaskState::TranscribingLocal),
        ("summary", TaskState::GeneratingNotes),
    ] {
        task(&store, id, state);
    }
    let ids = [
        "done", "failed", "waiting", "paused", "active", "summary", "missing",
    ]
    .map(String::from);
    let result = store.clear_history(&ids, Some(ACCOUNT)).unwrap();
    assert_eq!(result.cleared_ids, vec!["done", "failed"]);
    assert_eq!(result.retained_ids.len(), 4);
    assert_eq!(result.failed_ids, vec!["missing"]);
    assert_eq!(visible(&store).len(), 4);
    assert!(store
        .clear_history(&["done".into()], Some(ACCOUNT))
        .unwrap()
        .cleared_ids
        .is_empty());
}

#[test]
fn project_save_failure_is_clearable_but_uploading_and_pending_are_protected() {
    let store = store();
    for (id, state) in [
        ("failed", TranscriptAssetState::Failed),
        ("pending", TranscriptAssetState::Pending),
        ("uploading", TranscriptAssetState::Uploading),
    ] {
        let mut record = task(&store, id, TaskState::TranscriptReady);
        record.transcript_asset_state = state;
        store.save_task_record(&record).unwrap();
    }
    let result = store
        .clear_history(
            &["failed".into(), "pending".into(), "uploading".into()],
            Some(ACCOUNT),
        )
        .unwrap();
    assert_eq!(result.cleared_ids, vec!["failed"]);
    assert_eq!(result.retained_ids.len(), 2);
}

#[test]
fn persists_note_binding_and_checks_account_and_site_after_list_clearing() {
    let store = store();
    task(&store, "note", TaskState::TranscriptReady);
    let binding = NoteBinding {
        note_key: "cloud-note".into(),
        account_key: ACCOUNT.into(),
        site_origin: "https://qasnack.mechlabs.cn".into(),
    };
    store.bind_record_note("note", binding.clone()).unwrap();
    store
        .clear_history(&["note".into()], Some(ACCOUNT))
        .unwrap();
    assert_eq!(
        store
            .read_record_note("note", ACCOUNT, &binding.site_origin)
            .unwrap()
            .as_deref(),
        Some("cloud-note")
    );
    assert!(store
        .read_record_note("note", "origin:tenant:8", &binding.site_origin)
        .is_err());
    assert!(store
        .read_record_note("note", ACCOUNT, "https://snack.mechlabs.cn")
        .is_err());
    assert!(store
        .bind_record_note(
            "note",
            NoteBinding {
                note_key: "different".into(),
                ..binding
            }
        )
        .is_err());
}

#[test]
fn rejects_foreign_account_and_corrupt_history_without_changing_visibility() {
    let store = store();
    task(&store, "note", TaskState::TranscriptReady);
    assert!(store
        .clear_history(&["note".into()], Some("origin:tenant:8"))
        .is_err());
    assert_eq!(visible(&store), vec!["note"]);
    fs::write(store.root().join(FILE), "broken").unwrap();
    assert!(store
        .clear_history(&["note".into()], Some(ACCOUNT))
        .is_err());
    assert_eq!(
        fs::read_to_string(store.root().join(FILE)).unwrap(),
        "broken"
    );
}

#[test]
fn retention_protects_a_reader_then_reclaims_old_note_audio_without_changing_text() {
    let store = store();
    for index in 0..12 {
        let id = format!("note-{index}");
        let mut record = task(&store, &id, TaskState::TranscriptReady);
        let audio = store.root().join(format!("{id}.wav"));
        let txt = store.root().join(format!("{id}.txt"));
        fs::write(&audio, "audio").unwrap();
        fs::write(&txt, "原文").unwrap();
        record.audio_path = Some(audio.to_string_lossy().into_owned());
        record.transcript_path = Some(txt.to_string_lossy().into_owned());
        record.created_at = format!("2026-11-03T01:00:{index:02}Z");
        store.save_task_record(&record).unwrap();
    }
    super::super::audio_usage::update("note-0".into(), "reader-test".into(), true);
    assert_eq!(store.prune_recording_audio(10).unwrap(), 1);
    assert!(store.root().join("note-0.wav").exists());
    assert!(!store.root().join("note-1.wav").exists());
    super::super::audio_usage::update("note-0".into(), "reader-test".into(), false);
    store
        .clear_history(&["note-0".into()], Some(ACCOUNT))
        .unwrap();
    assert_eq!(store.prune_recording_audio(10).unwrap(), 1);
    assert!(!store.root().join("note-0.wav").exists());
    assert_eq!(
        fs::read_to_string(store.root().join("note-0.txt")).unwrap(),
        "原文"
    );
    assert!(store
        .load_task_record("note-0")
        .unwrap()
        .audio_path
        .is_none());
}

#[test]
fn retention_only_reclaims_identifiable_owned_orphans_and_ignores_missing_file_quota() {
    let store = store();
    let audio_dir = store
        .recordings_root()
        .join(super::super::state::AUDIO_DIR_NAME);
    fs::create_dir_all(&audio_dir).unwrap();
    let managed = audio_dir.join("rec-1762100000000-12-0.wav");
    let external = audio_dir.join("my-import.wav");
    fs::write(&managed, "owned").unwrap();
    fs::write(&external, "user").unwrap();
    assert_eq!(store.prune_recording_audio(0).unwrap(), 1);
    assert!(!managed.exists());
    assert!(external.exists());
    for index in 0..12 {
        let mut record = task(
            &store,
            &format!("known-{index}"),
            TaskState::TranscriptReady,
        );
        let path = audio_dir.join(format!("known-{index}.wav"));
        if index != 11 {
            fs::write(&path, "audio").unwrap();
        }
        record.audio_path = Some(path.to_string_lossy().into_owned());
        record.created_at = format!("2026-11-03T01:00:{index:02}Z");
        store.save_task_record(&record).unwrap();
    }
    assert_eq!(store.prune_recording_audio(10).unwrap(), 1);
    assert!(store
        .load_task_record("known-11")
        .unwrap()
        .audio_path
        .is_none());
    assert!(audio_dir.join("known-1.wav").exists());
    assert!(external.exists());
}

fn recorder(path: std::path::PathBuf) -> Recorder {
    Recorder {
        shared: super::super::capture::CaptureShared::new(0),
        session: None,
        audio_path: path,
    }
}

#[test]
fn missing_audio_cleanup_stops_capture_releases_current_and_preserves_note_data() {
    use std::sync::atomic::Ordering;
    let store = store();
    let mut lost = task(&store, "lost", TaskState::Recording);
    let path = store.root().join("lost.wav");
    fs::write(&path, b"old audio").unwrap();
    lost.audio_path = Some(path.to_string_lossy().into_owned());
    let txt = store.root().join("lost.txt");
    fs::write(&txt, "keep original transcript").unwrap();
    lost.transcript_path = Some(txt.to_string_lossy().into_owned());
    store.save_task(&lost).unwrap();
    let binding = NoteBinding {
        note_key: "cloud-note".into(),
        account_key: ACCOUNT.into(),
        site_origin: "https://qasnack.mechlabs.cn".into(),
    };
    store.bind_record_note("lost", binding.clone()).unwrap();
    let mut capture = recorder(path.clone());
    let shared = capture.shared.clone();
    let thread_shared = shared.clone();
    let joined = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_joined = joined.clone();
    capture.session = Some(std::thread::spawn(move || {
        while !thread_shared.stop.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        thread_joined.store(true, Ordering::SeqCst);
    }));
    let current = Mutex::new(Some(capture));
    fs::remove_file(&path).unwrap();
    let mut snapshot = store.load_task_records();
    store.decorate_record_history(&mut snapshot, Some(ACCOUNT));
    assert!(snapshot[0].history.as_ref().unwrap().clearable);
    assert!(
        snapshot[0]
            .history
            .as_ref()
            .unwrap()
            .missing_recording_audio
    );
    let result = store
        .clear_record_history(&["lost".into()], Some(ACCOUNT), &current)
        .unwrap();
    assert_eq!(result.cleared_ids, vec!["lost"]);
    assert!(current.lock().unwrap().is_none());
    assert!(shared.stop.load(Ordering::SeqCst) && joined.load(Ordering::SeqCst));
    assert!(store.load_task().is_none());
    let retained = store.load_task_record("lost").unwrap();
    assert_eq!(retained.state, TaskState::Idle);
    assert!(retained.ended_at.is_some());
    assert!(txt.exists() && !path.exists());
    assert_eq!(
        store
            .read_record_note("lost", ACCOUNT, &binding.site_origin)
            .unwrap()
            .as_deref(),
        Some("cloud-note")
    );
    let reopened = MeetingStore::for_test(store.root().clone(), store.root().join("recordings"));
    assert!(visible(&reopened).is_empty());
    assert_eq!(
        reopened.load_task_record("lost").unwrap().state,
        TaskState::Idle
    );
    assert!(reopened.load_task().is_none());
}

#[test]
fn existing_or_restored_audio_and_unknown_paths_keep_recording_protected() {
    let store = store();
    let mut active = task(&store, "active", TaskState::Recording);
    let path = store.root().join("active.wav");
    active.audio_path = Some(path.to_string_lossy().into_owned());
    store.save_task(&active).unwrap();
    let current = Mutex::new(Some(recorder(path.clone())));
    // The file was missing in a previous snapshot, but exists by confirmation time.
    let mut snapshot = store.load_task_records();
    store.decorate_record_history(&mut snapshot, Some(ACCOUNT));
    assert!(snapshot[0].history.as_ref().unwrap().clearable);
    fs::write(&path, b"").unwrap();
    let result = store
        .clear_record_history(&["active".into()], Some(ACCOUNT), &current)
        .unwrap();
    assert_eq!(result.retained_ids, vec!["active"]);
    assert!(current.lock().unwrap().is_some());
    assert_eq!(store.load_task().unwrap().state, TaskState::Recording);
    assert!(path.exists());
    active.audio_path = None;
    store.save_task(&active).unwrap();
    assert_eq!(
        store
            .clear_history(&["active".into()], Some(ACCOUNT))
            .unwrap()
            .retained_ids,
        vec!["active"]
    );
    active.audio_path = Some(
        path.join("not-a-directory.wav")
            .to_string_lossy()
            .into_owned(),
    );
    store.save_task(&active).unwrap();
    assert_eq!(
        store
            .clear_history(&["active".into()], Some(ACCOUNT))
            .unwrap()
            .retained_ids,
        vec!["active"]
    );
}

#[test]
fn clearing_an_old_missing_recording_never_stops_a_different_current_recorder() {
    use std::sync::atomic::Ordering;
    let store = store();
    let mut old = task(&store, "old", TaskState::Recording);
    old.audio_path = Some(
        store
            .root()
            .join("deleted.wav")
            .to_string_lossy()
            .into_owned(),
    );
    store.save_task_record(&old).unwrap();
    let mut active = task(&store, "new", TaskState::Recording);
    let path = store.root().join("new.wav");
    fs::write(&path, b"active").unwrap();
    active.audio_path = Some(path.to_string_lossy().into_owned());
    store.save_task(&active).unwrap();
    let capture = recorder(path.clone());
    let shared = capture.shared.clone();
    let current = Mutex::new(Some(capture));
    let result = store
        .clear_record_history(&["old".into(), "new".into()], Some(ACCOUNT), &current)
        .unwrap();
    assert_eq!(result.cleared_ids, vec!["old"]);
    assert_eq!(result.retained_ids, vec!["new"]);
    assert!(!shared.stop.load(Ordering::SeqCst));
    assert!(current.lock().unwrap().is_some());
    assert_eq!(store.load_task().unwrap().recording_id, "new");
    assert_eq!(visible(&store), vec!["new"]);
    assert!(path.exists());
}

#[test]
fn foreign_account_cannot_stop_a_missing_audio_recording() {
    use std::sync::atomic::Ordering;
    let store = store();
    let mut lost = task(&store, "lost", TaskState::Recording);
    let path = store.root().join("lost.wav");
    lost.audio_path = Some(path.to_string_lossy().into_owned());
    store.save_task(&lost).unwrap();
    let capture = recorder(path);
    let shared = capture.shared.clone();
    let current = Mutex::new(Some(capture));
    assert!(store
        .clear_record_history(&["lost".into()], Some("origin:tenant:8"), &current)
        .is_err());
    assert!(!shared.stop.load(Ordering::SeqCst));
    assert_eq!(store.load_task().unwrap().state, TaskState::Recording);
    assert!(current.lock().unwrap().is_some());
}
