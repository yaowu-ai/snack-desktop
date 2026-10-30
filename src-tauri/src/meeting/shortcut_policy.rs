//! Native-owned shortcut decisions and irreversible recording facts.
//! The sidecar owns the committed key, so a crash between settings/metadata writes
//! cannot restore an obsolete key. Account bindings are deliberately memory-only.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use super::state::{now_rfc3339, persist_json_atomic, MeetingSettings, MeetingStore, MeetingTask};

const POLICY_FILE: &str = "shortcut-policy.json";
pub(crate) const LEGACY_SHORTCUT: &str = "CommandOrControl+R";
static POLICY_LOCK: Mutex<()> = Mutex::new(());
static PENDING_STARTS: Mutex<BTreeMap<PathBuf, PendingStarts>> = Mutex::new(BTreeMap::new());

#[derive(Default, Clone)]
struct PendingStarts {
    device: Option<String>,
    accounts: BTreeMap<String, String>,
}
static SESSION: Mutex<Session> = Mutex::new(Session {
    account_key: None,
    usage: Usage::Unknown,
    first_started_at: None,
    registration_error: None,
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Usage {
    Used,
    Never,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Default,
    User,
    LegacyUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Policy {
    schema_version: u32,
    migration_version: u32,
    effective_shortcut: Option<String>,
    source: Source,
    eligible_for_default: bool,
    first_device_started_at: Option<String>,
    account_starts: BTreeMap<String, String>,
}

#[derive(Clone)]
struct Session {
    account_key: Option<String>,
    usage: Usage,
    first_started_at: Option<String>,
    registration_error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReconcileRequest {
    account_key: Option<String>,
    usage: Usage,
    first_recording_started_at: Option<String>,
    #[serde(default)]
    bind_only: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PolicySnapshot {
    account_key: Option<String>,
    usage: Usage,
    first_recording_started_at: Option<String>,
    shortcut: String,
    registration_error: Option<String>,
}

pub(crate) fn new_default_shortcut() -> &'static str {
    if cfg!(target_os = "macos") {
        "Control+R"
    } else {
        LEGACY_SHORTCUT
    }
}

/// Bootstrap without treating absent tasks, absent settings or read failures as never-used.
pub(crate) fn initialize(store: &MeetingStore) -> Result<(), String> {
    let _guard = POLICY_LOCK.lock().map_err(|e| e.to_string())?;
    let existing = read_policy(store.root())?;
    let starts = successful_recording_starts(store.root())?;
    if let Some(mut policy) = existing {
        if !starts.is_empty() {
            backfill_recording_starts(&mut policy, starts);
            save_policy(store.root(), &policy)?;
        }
        return Ok(());
    }
    let settings = read_optional::<MeetingSettings>(&store.root().join("settings.json"))?;
    let legacy_data = has_legacy_data(store.root())?;
    let first_started = starts.first().map(|start| start.started_at.clone());
    let mut policy = bootstrap(settings.as_ref(), legacy_data, first_started);
    backfill_recording_starts(&mut policy, starts);
    save_policy(store.root(), &policy)
}

fn bootstrap(
    settings: Option<&MeetingSettings>,
    legacy_data: bool,
    first_started: Option<String>,
) -> Policy {
    let effective = settings
        .map(|value| legacy_effective(&value.shortcut))
        .or_else(|| legacy_data.then(|| LEGACY_SHORTCUT.to_string()));
    Policy {
        schema_version: 1,
        migration_version: u32::from(effective.is_some()),
        effective_shortcut: effective,
        source: if settings.is_some() {
            Source::LegacyUnknown
        } else {
            Source::Default
        },
        eligible_for_default: settings.is_none() && !legacy_data && first_started.is_none(),
        first_device_started_at: first_started,
        account_starts: BTreeMap::new(),
    }
}

fn legacy_effective(shortcut: &str) -> String {
    if shortcut == "CommandOrControl+Shift+R" {
        LEGACY_SHORTCUT
    } else {
        shortcut
    }
    .to_string()
}

pub(crate) fn committed_shortcut(store: &MeetingStore) -> Option<String> {
    read_policy(store.root()).ok().flatten()?.effective_shortcut
}

pub(crate) fn should_register(store: &MeetingStore) -> bool {
    // On a pristine installation, wait for identity rather than briefly occupying Cmd+R.
    matches!(
        read_policy(store.root()),
        Ok(Some(Policy {
            effective_shortcut: Some(_),
            ..
        }))
    )
}

pub(crate) fn account_key() -> Option<String> {
    SESSION.lock().ok()?.account_key.clone()
}

/// Full-page navigation may destroy Web cleanup callbacks; never retain the old login binding.
pub(crate) fn clear_account_binding() {
    if let Ok(mut session) = SESSION.lock() {
        session.account_key = None;
        session.usage = Usage::Unknown;
        session.first_started_at = None;
    }
}

pub(crate) fn snapshot(store: &MeetingStore) -> Option<PolicySnapshot> {
    let session = SESSION.lock().ok()?.clone();
    let policy = require_policy(store.root()).ok()?;
    Some(snapshot_for(&policy, &session))
}

fn snapshot_for(policy: &Policy, session: &Session) -> PolicySnapshot {
    let local = session
        .account_key
        .as_ref()
        .and_then(|key| policy.account_starts.get(key));
    PolicySnapshot {
        account_key: session.account_key.clone(),
        usage: if local.is_some() {
            Usage::Used
        } else {
            session.usage
        },
        first_recording_started_at: local.cloned().or_else(|| session.first_started_at.clone()),
        shortcut: policy
            .effective_shortcut
            .clone()
            .unwrap_or_else(|| LEGACY_SHORTCUT.into()),
        registration_error: session.registration_error.clone(),
    }
}

/// Bind the current authenticated page, then register and commit a decision exactly once.
pub(crate) fn reconcile(
    app: &AppHandle,
    request: ReconcileRequest,
) -> Result<PolicySnapshot, String> {
    validate_request(&request)?;
    let _guard = POLICY_LOCK.lock().map_err(|e| e.to_string())?;
    let state = app.state::<super::MeetingManagerState>();
    let mut session = SESSION.lock().map_err(|e| e.to_string())?;
    session.account_key = request.account_key.clone();
    session.usage = if request.account_key.is_some() {
        request.usage
    } else {
        Usage::Unknown
    };
    session.first_started_at = if request.account_key.is_some() {
        request.first_recording_started_at.clone()
    } else {
        None
    };
    let mut policy = require_policy(state.store.root())?;
    if request.account_key.is_none() || request.bind_only {
        return Ok(snapshot_for(&policy, &session));
    }
    if request.usage == Usage::Used {
        policy
            .account_starts
            .entry(request.account_key.clone().unwrap())
            .or_insert_with(|| {
                request
                    .first_recording_started_at
                    .clone()
                    .expect("validated recording time")
            });
    }
    let previous = policy.effective_shortcut.clone();
    let account_used = policy
        .account_starts
        .contains_key(request.account_key.as_ref().unwrap());
    apply_usage(
        &mut policy,
        if account_used {
            Usage::Used
        } else {
            request.usage
        },
    );
    let shortcut = policy
        .effective_shortcut
        .as_deref()
        .ok_or("快捷键状态暂不可用")?;
    if let Err(error) = super::quick_access::activate_shortcut(app, previous.as_deref(), shortcut) {
        session.registration_error = Some(error);
        return Ok(snapshot_for(&require_policy(state.store.root())?, &session));
    }
    commit_or_rollback(app, &state.store, &policy, previous.as_deref())?;
    session.registration_error = None;
    let result = snapshot_for(&policy, &session);
    drop(session);
    drop(_guard);
    super::emit_state(app, &state.store);
    Ok(result)
}

fn validate_request(request: &ReconcileRequest) -> Result<(), String> {
    if request.account_key.is_some()
        && request.usage == Usage::Used
        && request.first_recording_started_at.is_none()
    {
        return Err("录音使用状态缺少开始时间".into());
    }
    if request
        .account_key
        .as_ref()
        .is_some_and(|key| key.trim().is_empty() || key.len() > 2048)
    {
        return Err("账号标识无效".into());
    }
    if request
        .first_recording_started_at
        .as_ref()
        .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_err())
    {
        return Err("录音使用状态无效".into());
    }
    Ok(())
}

fn apply_usage(policy: &mut Policy, usage: Usage) {
    if policy.eligible_for_default
        && policy.source == Source::Default
        && policy.first_device_started_at.is_none()
    {
        if usage == Usage::Never {
            policy.effective_shortcut = Some(new_default_shortcut().into());
            policy.migration_version = 1;
        } else if usage == Usage::Used {
            policy
                .effective_shortcut
                .get_or_insert_with(|| LEGACY_SHORTCUT.into());
            policy.eligible_for_default = false;
            policy.migration_version = 1;
        }
    }
    policy
        .effective_shortcut
        .get_or_insert_with(|| LEGACY_SHORTCUT.into());
}

fn commit_or_rollback(
    app: &AppHandle,
    store: &MeetingStore,
    policy: &Policy,
    previous: Option<&str>,
) -> Result<(), String> {
    if let Err(error) = save_policy(store.root(), policy) {
        let next = policy
            .effective_shortcut
            .as_deref()
            .ok_or("快捷键状态暂不可用")?;
        super::quick_access::restore_shortcut(app, next, previous)?;
        return Err(error);
    }
    Ok(())
}

/// Called only after capture succeeded, with the identity captured before starting capture.
pub(crate) fn mark_started(store: &MeetingStore, account: Option<&str>) -> Result<(), String> {
    let _guard = POLICY_LOCK.lock().map_err(|e| e.to_string())?;
    let started_at = now_rfc3339();
    let mut pending = PENDING_STARTS.lock().map_err(|e| e.to_string())?;
    let facts = pending.entry(store.root().clone()).or_default();
    facts.device.get_or_insert_with(|| started_at.clone());
    if let Some(account) = account {
        facts
            .accounts
            .entry(account.into())
            .or_insert(started_at.clone());
    }
    drop(pending);
    let mut policy = require_policy(store.root())?;
    record_start(&mut policy, account, started_at);
    save_policy(store.root(), &policy)
}

fn record_start(policy: &mut Policy, account: Option<&str>, started_at: String) {
    policy
        .first_device_started_at
        .get_or_insert_with(|| started_at.clone());
    policy.eligible_for_default = false;
    if let Some(account) = account {
        policy
            .account_starts
            .entry(account.into())
            .or_insert(started_at);
    }
}

/// Web cannot overwrite policy metadata by posting an older MeetingSettings object.
pub(crate) fn save_user_settings(
    app: &AppHandle,
    store: &MeetingStore,
    settings: &MeetingSettings,
    explicit: Option<bool>,
) -> Result<(), String> {
    let _guard = POLICY_LOCK.lock().map_err(|e| e.to_string())?;
    let mut policy = require_policy(store.root())?;
    let previous = store.load_settings();
    let settings = merge_user_settings(&mut policy, &previous, settings, explicit);
    super::quick_access::activate_shortcut(app, Some(&previous.shortcut), &settings.shortcut)?;
    if let Err(error) = store.save_settings(&settings) {
        super::quick_access::restore_shortcut(app, &settings.shortcut, Some(&previous.shortcut))?;
        return Err(error);
    }
    if let Err(error) = save_policy(store.root(), &policy) {
        store.save_settings(&previous)?;
        super::quick_access::restore_shortcut(app, &settings.shortcut, Some(&previous.shortcut))?;
        return Err(error);
    }
    clear_registration_error();
    Ok(())
}

/// A successful explicit save resolves any earlier registration failure shown in Web.
fn clear_registration_error() {
    if let Ok(mut session) = SESSION.lock() {
        session.registration_error = None;
    }
}

/// An explicit non-shortcut save cannot replay a stale Web shortcut into native settings.
fn merge_user_settings(
    policy: &mut Policy,
    previous: &MeetingSettings,
    incoming: &MeetingSettings,
    explicit: Option<bool>,
) -> MeetingSettings {
    let mut settings = incoming.clone();
    if explicit == Some(false) {
        settings.shortcut = previous.shortcut.clone();
    }
    if explicit == Some(true) || previous.shortcut != settings.shortcut {
        policy.source = Source::User;
        policy.eligible_for_default = false;
    }
    policy.effective_shortcut = Some(settings.shortcut.clone());
    policy.migration_version = 1;
    settings
}

fn require_policy(root: &Path) -> Result<Policy, String> {
    let mut policy = read_policy(root)?.ok_or("快捷键迁移状态暂不可用，请重启后重试")?;
    let pending = PENDING_STARTS.lock().map_err(|e| e.to_string())?;
    if let Some(facts) = pending.get(root) {
        if let Some(started) = &facts.device {
            record_start(&mut policy, None, started.clone());
        }
        for (account, started) in &facts.accounts {
            record_start(&mut policy, Some(account), started.clone());
        }
    }
    Ok(policy)
}

fn read_policy(root: &Path) -> Result<Option<Policy>, String> {
    let policy = read_optional::<Policy>(&root.join(POLICY_FILE))?;
    if policy.as_ref().is_some_and(|value| {
        value.schema_version != 1
            || value
                .effective_shortcut
                .as_ref()
                .is_some_and(|key| key.trim().is_empty())
    }) {
        return Err("快捷键迁移记录无效，已保留原配置".into());
    }
    Ok(policy)
}

fn save_policy(root: &Path, policy: &Policy) -> Result<(), String> {
    persist_json_atomic(&root.join(POLICY_FILE), policy)
}

fn read_optional<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn has_legacy_data(root: &Path) -> Result<bool, String> {
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_name() == POLICY_FILE {
            continue;
        }
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            return Ok(true);
        }
        if fs::read_dir(entry.path())
            .map_err(|error| error.to_string())?
            .next()
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

struct RecordingStartFact {
    account_key: Option<String>,
    started_at: String,
}

fn successful_recording_starts(root: &Path) -> Result<Vec<RecordingStartFact>, String> {
    let mut paths = vec![root.join("task.json")];
    if let Ok(entries) = fs::read_dir(root.join("tasks")) {
        paths.extend(entries.filter_map(Result::ok).map(|entry| entry.path()));
    }
    let mut starts = paths
        .into_iter()
        .filter_map(|path| read_optional::<MeetingTask>(&path).ok().flatten())
        .filter(|task| task.audio_file_owned && task.audio_path.is_some())
        .filter_map(|task| {
            task.started_at.map(|started_at| RecordingStartFact {
                account_key: task.recording_account_key,
                started_at,
            })
        })
        .collect::<Vec<_>>();
    starts.sort_by(|left, right| left.started_at.cmp(&right.started_at));
    Ok(starts)
}

fn backfill_recording_starts(policy: &mut Policy, starts: Vec<RecordingStartFact>) {
    for start in starts {
        record_start(policy, start.account_key.as_deref(), start.started_at);
    }
}

#[cfg(test)]
fn legacy_recording_start(root: &Path) -> Result<Option<String>, String> {
    Ok(successful_recording_starts(root)?
        .into_iter()
        .next()
        .map(|start| start.started_at))
}

#[cfg(test)]
mod tests;
