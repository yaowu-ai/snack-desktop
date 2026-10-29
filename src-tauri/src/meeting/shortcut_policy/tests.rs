use super::super::state::TaskState;
use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
const FIRST: &str = "2026-10-27T01:00:00.000Z";
const SECOND: &str = "2026-10-27T02:00:00.000Z";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let id = FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "snack-shortcut-policy-{}-{}-{id}",
            std::process::id(),
            super::super::state::unix_millis()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        PENDING_STARTS.lock().unwrap().remove(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn settings(shortcut: &str) -> MeetingSettings {
    MeetingSettings {
        shortcut: shortcut.into(),
        ..MeetingSettings::default()
    }
}

fn session(account: Option<&str>, usage: Usage) -> Session {
    Session {
        account_key: account.map(str::to_owned),
        usage,
        first_started_at: None,
        registration_error: None,
    }
}

#[test]
fn legacy_saved_shift_key_freezes_the_key_that_was_actually_active() {
    let policy = bootstrap(Some(&settings("CommandOrControl+Shift+R")), true, None);
    assert_eq!(policy.effective_shortcut.as_deref(), Some(LEGACY_SHORTCUT));
    assert_eq!(policy.source, Source::LegacyUnknown);
    assert!(!policy.eligible_for_default);
    assert_eq!(policy.migration_version, 1);
}

#[test]
fn legacy_conversion_only_matches_the_exact_previous_compatibility_rule() {
    for value in ["Command+Shift+R", "Control+Shift+R", "Alt+R", "Command+K"] {
        let policy = bootstrap(Some(&settings(value)), true, None);
        assert_eq!(policy.effective_shortcut.as_deref(), Some(value));
    }
}

#[test]
fn recorded_legacy_installation_without_settings_keeps_old_default() {
    let mut policy = bootstrap(None, true, Some(FIRST.into()));
    apply_usage(&mut policy, Usage::Never);
    assert_eq!(policy.effective_shortcut.as_deref(), Some(LEGACY_SHORTCUT));
    assert_eq!(policy.first_device_started_at.as_deref(), Some(FIRST));
    assert!(!policy.eligible_for_default);
}

#[test]
fn existing_installation_with_unknown_history_keeps_old_default() {
    let mut policy = bootstrap(None, true, None);
    apply_usage(&mut policy, Usage::Unknown);
    apply_usage(&mut policy, Usage::Never);
    assert_eq!(policy.effective_shortcut.as_deref(), Some(LEGACY_SHORTCUT));
    assert!(!policy.eligible_for_default);
}

#[test]
fn pristine_installation_waits_for_an_identity_decision() {
    let policy = bootstrap(None, false, None);
    assert!(policy.effective_shortcut.is_none());
    assert!(policy.eligible_for_default);
    assert_eq!(policy.migration_version, 0);
}

#[test]
fn confirmed_new_user_receives_the_platform_default() {
    let mut policy = bootstrap(None, false, None);
    apply_usage(&mut policy, Usage::Never);
    assert_eq!(
        policy.effective_shortcut.as_deref(),
        Some(new_default_shortcut())
    );
    assert_eq!(policy.migration_version, 1);
    if cfg!(target_os = "macos") {
        assert_eq!(policy.effective_shortcut.as_deref(), Some("Control+R"));
    } else {
        assert_eq!(policy.effective_shortcut.as_deref(), Some(LEGACY_SHORTCUT));
    }
}

#[test]
fn returning_account_on_pristine_device_keeps_legacy_default() {
    let mut policy = bootstrap(None, false, None);
    apply_usage(&mut policy, Usage::Used);
    apply_usage(&mut policy, Usage::Never);
    assert_eq!(policy.effective_shortcut.as_deref(), Some(LEGACY_SHORTCUT));
    assert!(!policy.eligible_for_default);
}

#[test]
fn user_owned_key_has_priority_over_a_never_used_account() {
    let mut policy = bootstrap(None, false, None);
    policy.source = Source::User;
    policy.effective_shortcut = Some("Command+Shift+K".into());
    apply_usage(&mut policy, Usage::Never);
    assert_eq!(
        policy.effective_shortcut.as_deref(),
        Some("Command+Shift+K")
    );
}

#[test]
fn repeated_resolution_and_later_usage_do_not_change_the_committed_new_key() {
    let mut policy = bootstrap(None, false, None);
    apply_usage(&mut policy, Usage::Never);
    let committed = policy.effective_shortcut.clone();
    apply_usage(&mut policy, Usage::Never);
    record_start(&mut policy, Some("tenant-a:user-a"), FIRST.into());
    apply_usage(&mut policy, Usage::Used);
    apply_usage(&mut policy, Usage::Never);
    assert_eq!(policy.effective_shortcut, committed);
    assert!(!policy.eligible_for_default);
}

#[test]
fn successful_recording_is_irreversible_and_preserves_the_first_start() {
    let mut policy = bootstrap(None, false, None);
    record_start(&mut policy, Some("tenant-a:user-a"), FIRST.into());
    record_start(&mut policy, Some("tenant-a:user-a"), SECOND.into());
    assert_eq!(policy.first_device_started_at.as_deref(), Some(FIRST));
    assert_eq!(
        policy
            .account_starts
            .get("tenant-a:user-a")
            .map(String::as_str),
        Some(FIRST)
    );
    assert!(!policy.eligible_for_default);
}

#[test]
fn account_recordings_do_not_leak_to_another_account_or_tenant() {
    let mut policy = bootstrap(None, false, None);
    record_start(&mut policy, Some("tenant-a:user-a"), FIRST.into());
    let owner = snapshot_for(&policy, &session(Some("tenant-a:user-a"), Usage::Never));
    assert_eq!(owner.usage, Usage::Used);
    assert_eq!(owner.first_recording_started_at.as_deref(), Some(FIRST));
    for key in ["tenant-a:user-b", "tenant-b:user-a"] {
        let other = snapshot_for(&policy, &session(Some(key), Usage::Unknown));
        assert_eq!(other.usage, Usage::Unknown);
        assert!(other.first_recording_started_at.is_none());
    }
}

#[test]
fn unattributed_recording_cannot_be_claimed_by_the_next_login() {
    let mut policy = bootstrap(None, false, None);
    record_start(&mut policy, None, FIRST.into());
    let next = snapshot_for(
        &policy,
        &session(Some("tenant-a:next-user"), Usage::Unknown),
    );
    assert_eq!(next.usage, Usage::Unknown);
    assert!(next.first_recording_started_at.is_none());
    assert!(policy.account_starts.is_empty());
    assert!(!policy.eligible_for_default);
}

#[test]
fn durable_facts_and_user_key_round_trip_independently_of_recording_history() {
    let fixture = Fixture::new();
    let mut policy = bootstrap(None, false, None);
    policy.source = Source::User;
    policy.effective_shortcut = Some("CommandOrControl+Shift+R".into());
    record_start(&mut policy, Some("tenant-a:user-a"), FIRST.into());
    save_policy(fixture.path(), &policy).unwrap();
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    fs::write(fixture.path().join("tasks/removed.json"), b"{}").unwrap();
    fs::remove_dir_all(fixture.path().join("tasks")).unwrap();
    let loaded = require_policy(fixture.path()).unwrap();
    assert_eq!(loaded.effective_shortcut, policy.effective_shortcut);
    assert_eq!(loaded.source, Source::User);
    assert_eq!(loaded.account_starts, policy.account_starts);
    assert_eq!(
        loaded.first_device_started_at,
        policy.first_device_started_at
    );
}

#[test]
fn corrupt_or_future_policy_is_unavailable_and_never_silently_overwritten() {
    let fixture = Fixture::new();
    for bytes in [b"{broken".as_slice(), b"{\"schemaVersion\":999}".as_slice()] {
        let path = fixture.path().join(POLICY_FILE);
        fs::write(&path, bytes).unwrap();
        assert!(read_policy(fixture.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn missing_file_and_malformed_settings_are_distinct() {
    let fixture = Fixture::new();
    let path = fixture.path().join("settings.json");
    assert!(read_optional::<MeetingSettings>(&path).unwrap().is_none());
    fs::write(&path, b"{}").unwrap();
    assert!(read_optional::<MeetingSettings>(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"{}");
}

#[test]
fn an_empty_task_directory_is_not_positive_legacy_data() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    assert!(!has_legacy_data(fixture.path()).unwrap());
    fs::write(fixture.path().join("tasks/old.json"), b"{}").unwrap();
    assert!(has_legacy_data(fixture.path()).unwrap());
}

#[test]
fn importing_audio_or_failing_to_start_does_not_count_as_a_recording_start() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    let mut imported = MeetingTask::new("import".into(), "zh".into());
    imported.audio_file_owned = false;
    imported.started_at = Some(FIRST.into());
    imported.audio_path = Some("/external/import.wav".into());
    persist_json_atomic(&fixture.path().join("tasks/import.json"), &imported).unwrap();
    let mut failed = MeetingTask::new("failed".into(), "zh".into());
    failed.state = TaskState::CaptureFailed;
    failed.started_at = Some(FIRST.into());
    persist_json_atomic(&fixture.path().join("tasks/failed.json"), &failed).unwrap();
    assert!(legacy_recording_start(fixture.path()).unwrap().is_none());
    let mut recorded = MeetingTask::new("recorded".into(), "zh".into());
    recorded.state = TaskState::Recording;
    recorded.started_at = Some(SECOND.into());
    recorded.audio_path = Some("/local/recorded.wav".into());
    persist_json_atomic(&fixture.path().join("tasks/recorded.json"), &recorded).unwrap();
    assert_eq!(
        legacy_recording_start(fixture.path()).unwrap().as_deref(),
        Some(SECOND)
    );
}

#[test]
fn bridge_payload_rejects_anonymous_identifiers_and_invalid_timestamps() {
    for value in [
        serde_json::json!({ "accountKey": " ", "usage": "never", "firstRecordingStartedAt": null }),
        serde_json::json!({ "accountKey": "tenant:user", "usage": "used", "firstRecordingStartedAt": "bad" }),
        serde_json::json!({ "accountKey": "tenant:user", "usage": "used", "firstRecordingStartedAt": null }),
    ] {
        let request = serde_json::from_value::<ReconcileRequest>(value).unwrap();
        assert!(validate_request(&request).is_err());
    }
    assert!(
        serde_json::from_value::<ReconcileRequest>(serde_json::json!({
            "accountKey": "tenant:user", "usage": "never", "firstRecordingStartedAt": null,
            "unexpected": true
        }))
        .is_err()
    );
}

#[test]
fn initialization_is_idempotent_and_freezes_the_legacy_active_key() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    store
        .save_settings(&settings("CommandOrControl+Shift+R"))
        .unwrap();
    initialize(&store).unwrap();
    let path = fixture.path().join(POLICY_FILE);
    let before = fs::read(&path).unwrap();
    initialize(&store).unwrap();
    assert_eq!(fs::read(path).unwrap(), before);
    assert_eq!(store.load_settings().shortcut, LEGACY_SHORTCUT);
    assert!(should_register(&store));
}

#[test]
fn committed_sidecar_recovers_a_key_when_settings_still_contain_an_old_value() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    store.save_settings(&settings(LEGACY_SHORTCUT)).unwrap();
    let mut policy = bootstrap(None, false, None);
    policy.source = Source::User;
    policy.effective_shortcut = Some("CommandOrControl+Shift+R".into());
    save_policy(fixture.path(), &policy).unwrap();
    assert_eq!(store.load_settings().shortcut, "CommandOrControl+Shift+R");
    assert_eq!(require_policy(fixture.path()).unwrap().source, Source::User);
}

#[test]
fn clearing_retained_recordings_does_not_erase_the_permanent_usage_fact() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    let mut task = MeetingTask::new("legacy-recording".into(), "zh".into());
    task.state = TaskState::Ready;
    task.started_at = Some(FIRST.into());
    task.audio_path = Some("/local/recording.wav".into());
    store.save_task(&task).unwrap();
    initialize(&store).unwrap();
    assert_eq!(store.clear_task_records().unwrap(), 1);
    assert!(store.load_task_records().is_empty());
    let policy = require_policy(fixture.path()).unwrap();
    assert_eq!(policy.first_device_started_at.as_deref(), Some(FIRST));
    assert_eq!(policy.effective_shortcut.as_deref(), Some(LEGACY_SHORTCUT));
    assert!(!policy.eligible_for_default);
}

#[test]
fn corrupted_policy_prevents_registration_and_initialization_preserves_the_files() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    store
        .save_settings(&settings("CommandOrControl+Shift+R"))
        .unwrap();
    let settings_path = fixture.path().join("settings.json");
    let saved_settings = fs::read(&settings_path).unwrap();
    let policy_path = fixture.path().join(POLICY_FILE);
    fs::write(&policy_path, b"{bad-policy").unwrap();
    assert!(initialize(&store).is_err());
    assert!(!should_register(&store));
    assert_eq!(fs::read(policy_path).unwrap(), b"{bad-policy");
    assert_eq!(fs::read(settings_path).unwrap(), saved_settings);
}

#[test]
fn recording_write_failure_keeps_in_memory_facts_and_prevents_migration() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    let policy = bootstrap(None, false, None);
    save_policy(fixture.path(), &policy).unwrap();
    let temporary = fixture.path().join(POLICY_FILE).with_extension("json.tmp");
    fs::create_dir(&temporary).unwrap();
    assert!(mark_started(&store, Some("tenant-a:user-a")).is_err());
    let on_disk = read_policy(fixture.path()).unwrap().unwrap();
    assert!(on_disk.first_device_started_at.is_none());
    let mut protected = require_policy(fixture.path()).unwrap();
    let view = snapshot_for(&protected, &session(Some("tenant-a:user-a"), Usage::Never));
    assert_eq!(view.usage, Usage::Used);
    assert!(view.first_recording_started_at.is_some());
    assert!(!protected.eligible_for_default);
    apply_usage(&mut protected, Usage::Never);
    assert_eq!(
        protected.effective_shortcut.as_deref(),
        Some(LEGACY_SHORTCUT)
    );
    fs::remove_dir(temporary).unwrap();
    save_policy(fixture.path(), &protected).unwrap();
    let persisted = read_policy(fixture.path()).unwrap().unwrap();
    assert!(persisted.account_starts.contains_key("tenant-a:user-a"));
    assert!(!persisted.eligible_for_default);
}

#[test]
fn restart_backfills_a_successful_task_even_when_a_policy_already_exists() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    save_policy(fixture.path(), &bootstrap(None, false, None)).unwrap();
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    let mut task = MeetingTask::new("after-sidecar-write-failure".into(), "zh".into());
    task.state = TaskState::Ready;
    task.started_at = Some(FIRST.into());
    task.audio_path = Some("/local/recording.wav".into());
    store.save_task(&task).unwrap();
    initialize(&store).unwrap();
    let mut recovered = read_policy(fixture.path()).unwrap().unwrap();
    assert_eq!(recovered.first_device_started_at.as_deref(), Some(FIRST));
    assert!(recovered.account_starts.is_empty());
    assert!(!recovered.eligible_for_default);
    apply_usage(&mut recovered, Usage::Never);
    assert_eq!(
        recovered.effective_shortcut.as_deref(),
        Some(LEGACY_SHORTCUT)
    );
}

#[test]
fn explicit_non_shortcut_save_keeps_the_native_key_and_other_new_preferences() {
    let mut policy = bootstrap(None, false, None);
    let previous = settings("Control+K");
    let mut incoming = settings(LEGACY_SHORTCUT);
    incoming.recording_reminder_enabled = true;
    let merged = merge_user_settings(&mut policy, &previous, &incoming, Some(false));
    assert_eq!(merged.shortcut, "Control+K");
    assert!(merged.recording_reminder_enabled);
    assert_eq!(policy.source, Source::Default);
    assert!(policy.eligible_for_default);
}

#[test]
fn explicit_shortcut_selection_claims_user_ownership_even_if_the_key_is_unchanged() {
    let mut policy = bootstrap(None, false, None);
    let previous = settings("Control+K");
    let merged = merge_user_settings(&mut policy, &previous, &previous, Some(true));
    assert_eq!(merged.shortcut, previous.shortcut);
    assert_eq!(policy.source, Source::User);
    assert!(!policy.eligible_for_default);
}

#[test]
fn legacy_clients_can_still_set_a_key_without_the_new_intent_flag() {
    let mut policy = bootstrap(None, false, None);
    let merged = merge_user_settings(
        &mut policy,
        &settings(LEGACY_SHORTCUT),
        &settings("Command+Shift+K"),
        None,
    );
    assert_eq!(merged.shortcut, "Command+Shift+K");
    assert_eq!(policy.source, Source::User);
    assert!(!policy.eligible_for_default);
}

#[test]
fn legacy_unchanged_preference_save_does_not_claim_shortcut_ownership() {
    let mut policy = bootstrap(None, false, None);
    let previous = settings(LEGACY_SHORTCUT);
    merge_user_settings(&mut policy, &previous, &previous, None);
    assert_eq!(policy.source, Source::Default);
    assert!(policy.eligible_for_default);
}

#[test]
fn bind_only_payload_is_supported_without_changing_legacy_payload_defaults() {
    let legacy: ReconcileRequest = serde_json::from_value(serde_json::json!({
        "accountKey": "tenant-a:user-a", "usage": "unknown", "firstRecordingStartedAt": null
    }))
    .unwrap();
    assert!(!legacy.bind_only);
    let binding: ReconcileRequest = serde_json::from_value(serde_json::json!({
        "accountKey": "tenant-a:user-a", "usage": "unknown", "firstRecordingStartedAt": null,
        "bindOnly": true
    }))
    .unwrap();
    assert!(binding.bind_only);
    assert_eq!(binding.account_key.as_deref(), Some("tenant-a:user-a"));
    assert!(validate_request(&binding).is_ok());
}

#[test]
fn retained_task_restores_account_usage_after_a_sidecar_write_failure_and_restart() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    save_policy(fixture.path(), &bootstrap(None, false, None)).unwrap();
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    let mut task = MeetingTask::new("recovered-account".into(), "zh".into());
    task.state = TaskState::Ready;
    task.recording_account_key = Some("tenant-a:user-a".into());
    task.started_at = Some(FIRST.into());
    task.audio_path = Some("/local/recovered.wav".into());
    store.save_task(&task).unwrap();
    initialize(&store).unwrap();
    let recovered = require_policy(fixture.path()).unwrap();
    assert_eq!(
        recovered
            .account_starts
            .get("tenant-a:user-a")
            .map(String::as_str),
        Some(FIRST)
    );
    let owner = snapshot_for(&recovered, &session(Some("tenant-a:user-a"), Usage::Never));
    assert_eq!(owner.usage, Usage::Used);
    let next = snapshot_for(
        &recovered,
        &session(Some("tenant-a:user-b"), Usage::Unknown),
    );
    assert_eq!(next.usage, Usage::Unknown);
    assert!(next.first_recording_started_at.is_none());
}

#[test]
fn first_policy_bootstrap_recovers_each_known_recording_account() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    for (id, account, started) in [
        ("first", "tenant-a:user-a", FIRST),
        ("second", "tenant-a:user-b", SECOND),
    ] {
        let mut task = MeetingTask::new(id.into(), "zh".into());
        task.state = TaskState::Ready;
        task.recording_account_key = Some(account.into());
        task.started_at = Some(started.into());
        task.audio_path = Some(format!("/local/{id}.wav"));
        store.save_task(&task).unwrap();
    }
    initialize(&store).unwrap();
    let recovered = require_policy(fixture.path()).unwrap();
    assert_eq!(recovered.first_device_started_at.as_deref(), Some(FIRST));
    assert_eq!(
        recovered
            .account_starts
            .get("tenant-a:user-a")
            .map(String::as_str),
        Some(FIRST)
    );
    assert_eq!(
        recovered
            .account_starts
            .get("tenant-a:user-b")
            .map(String::as_str),
        Some(SECOND)
    );
    assert!(!recovered.eligible_for_default);
}

#[test]
fn old_tasks_without_native_identity_only_restore_device_usage() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    let mut task = MeetingTask::new("legacy-unattributed".into(), "zh".into());
    task.state = TaskState::Ready;
    task.started_at = Some(FIRST.into());
    task.audio_path = Some("/local/legacy.wav".into());
    let mut value = serde_json::to_value(&task).unwrap();
    value.as_object_mut().unwrap().remove("recordingAccountKey");
    let legacy: MeetingTask = serde_json::from_value(value.clone()).unwrap();
    assert!(legacy.recording_account_key.is_none());
    persist_json_atomic(&fixture.path().join("tasks/legacy.json"), &value).unwrap();
    initialize(&store).unwrap();
    let recovered = require_policy(fixture.path()).unwrap();
    assert_eq!(recovered.first_device_started_at.as_deref(), Some(FIRST));
    assert!(recovered.account_starts.is_empty());
}

#[test]
fn imported_and_failed_tasks_cannot_restore_an_account_recording_fact() {
    let fixture = Fixture::new();
    let store = MeetingStore::for_test(fixture.0.clone(), fixture.0.clone());
    fs::create_dir_all(fixture.path().join("tasks")).unwrap();
    let mut imported = MeetingTask::new("imported".into(), "zh".into());
    imported.audio_file_owned = false;
    imported.recording_account_key = Some("tenant-a:user-a".into());
    imported.started_at = Some(FIRST.into());
    imported.audio_path = Some("/external/imported.wav".into());
    store.save_task_record(&imported).unwrap();
    let mut failed = MeetingTask::new("failed".into(), "zh".into());
    failed.state = TaskState::CaptureFailed;
    failed.recording_account_key = Some("tenant-a:user-a".into());
    failed.started_at = Some(FIRST.into());
    store.save_task_record(&failed).unwrap();
    initialize(&store).unwrap();
    let recovered = require_policy(fixture.path()).unwrap();
    assert!(recovered.first_device_started_at.is_none());
    assert!(recovered.account_starts.is_empty());
}

#[test]
fn a_successful_settings_save_clears_only_the_old_registration_error() {
    let _guard = POLICY_LOCK.lock().unwrap();
    let before = SESSION.lock().unwrap().clone();
    {
        let mut current = SESSION.lock().unwrap();
        current.account_key = Some("tenant-a:user-a".into());
        current.usage = Usage::Used;
        current.first_started_at = Some(FIRST.into());
        current.registration_error = Some("already occupied".into());
    }
    clear_registration_error();
    let mut current = SESSION.lock().unwrap();
    assert!(current.registration_error.is_none());
    assert_eq!(current.account_key.as_deref(), Some("tenant-a:user-a"));
    assert_eq!(current.usage, Usage::Used);
    assert_eq!(current.first_started_at.as_deref(), Some(FIRST));
    *current = before;
}
