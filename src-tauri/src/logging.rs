use std::fs::{self, create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

const LOG_FILE_NAME: &str = "desktop.log";
const LOG_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const LOG_CAPACITY_BYTES: u64 = 10 * 1024 * 1024;
const LOG_SEGMENT_BYTES: u64 = 256 * 1024;
const LOG_EVENT_BYTES: usize = 16 * 1024;
static UUID_COUNTER: AtomicU64 = AtomicU64::new(1);
static ACTIVE_SCOPE: OnceLock<Mutex<Option<DiagnosticScope>>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct DiagnosticRecord {
    pub(crate) scope: String,
    pub(crate) user_id: String,
    pub(crate) tenant_key: String,
    pub(crate) owner_type: String,
    pub(crate) session_id: String,
    pub(crate) event: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct DiagnosticScopeInput {
    pub(crate) scope: String,
    pub(crate) user_id: String,
    pub(crate) tenant_key: String,
    pub(crate) owner_type: String,
    pub(crate) session_id: String,
}

#[derive(Clone, Debug)]
struct DiagnosticScope {
    scope: String,
    user_id: String,
    tenant_key: String,
    owner_type: String,
    session_id: String,
    install_id: String,
    sequence: u64,
    dropped_event_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StoredDiagnostic {
    pub(crate) manifest: DiagnosticManifest,
    pub(crate) event: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct DiagnosticManifest {
    #[serde(rename = "installId")]
    pub(crate) install_id: String,
    #[serde(rename = "sessionId")]
    pub(crate) session_id: String,
    #[serde(rename = "appVersion")]
    pub(crate) app_version: String,
    #[serde(rename = "buildNumber")]
    pub(crate) build_number: String,
    pub(crate) platform: String,
    #[serde(rename = "osVersion")]
    pub(crate) os_version: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticInfo {
    pub(crate) count: usize,
    pub(crate) bytes: u64,
    pub(crate) earliest_at: Option<String>,
    pub(crate) latest_at: Option<String>,
    pub(crate) dropped_event_count: u64,
}

pub(crate) fn log_path(app: &AppHandle) -> PathBuf {
    log_dir(app).join(LOG_FILE_NAME)
}

pub(crate) fn log_dir(app: &AppHandle) -> PathBuf {
    if let Ok(dir) = app.path().app_log_dir() {
        return dir;
    }

    if let Ok(home) = app.path().home_dir() {
        return home.join(".snack").join("logs");
    }

    std::env::temp_dir().join("snack").join("logs")
}

pub(crate) fn activate_scope(app: &AppHandle, input: &DiagnosticScopeInput) -> Result<(), String> {
    validate_scope(input)?;
    let lock = ACTIVE_SCOPE.get_or_init(|| Mutex::new(None));
    let mut active = lock.lock().map_err(|_| "诊断日志状态不可用".to_string())?;
    if active
        .as_ref()
        .is_some_and(|scope| scope.scope == input.scope)
    {
        return Ok(());
    }
    *active = Some(DiagnosticScope {
        scope: input.scope.clone(),
        user_id: input.user_id.clone(),
        tenant_key: input.tenant_key.clone(),
        owner_type: input.owner_type.clone(),
        session_id: input.session_id.clone(),
        install_id: install_id(app),
        sequence: 0,
        dropped_event_count: 0,
    });
    Ok(())
}

pub(crate) fn record_web_event(app: &AppHandle, input: DiagnosticRecord) -> Result<(), String> {
    let event = normalize_event(input.event, "webview")?;
    activate_scope(
        app,
        &DiagnosticScopeInput {
            scope: input.scope,
            user_id: input.user_id,
            tenant_key: input.tenant_key,
            owner_type: input.owner_type,
            session_id: input.session_id,
        },
    )?;
    append_active_event(app, event);
    Ok(())
}

pub(crate) fn snapshot(
    app: &AppHandle,
    scope: &str,
    since: i64,
    limit: usize,
) -> Result<Vec<StoredDiagnostic>, String> {
    prune(app);
    let _ = active_scope(scope)?;
    let mut rows = Vec::new();
    for path in scope_files(app, scope) {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let Ok(row) = serde_json::from_str::<StoredDiagnostic>(line) else {
                continue;
            };
            let occurred_at = row
                .event
                .get("occurredAt")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp_millis())
                .unwrap_or_default();
            if occurred_at >= since {
                rows.push(row);
                if rows.len() >= limit {
                    return Ok(rows);
                }
            }
        }
    }
    Ok(rows)
}

pub(crate) fn clear_scope(app: &AppHandle, scope: &str) -> Result<(), String> {
    let _ = active_scope(scope)?;
    for path in scope_files(app, scope) {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    if let Ok(mut state) = ACTIVE_SCOPE.get_or_init(|| Mutex::new(None)).lock() {
        if let Some(active) = state.as_mut() {
            active.dropped_event_count = 0;
        }
    }
    Ok(())
}

pub(crate) fn info(app: &AppHandle, scope: &str) -> Result<DiagnosticInfo, String> {
    let rows = snapshot(app, scope, now_millis() - LOG_RETENTION_MS, 10000)?;
    let bytes = rows
        .iter()
        .map(|row| {
            serde_json::to_vec(&row.event)
                .map(|value| value.len() as u64)
                .unwrap_or_default()
        })
        .sum();
    let earliest_at = rows
        .first()
        .and_then(|row| row.event.get("occurredAt"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let latest_at = rows
        .last()
        .and_then(|row| row.event.get("occurredAt"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let dropped_event_count = ACTIVE_SCOPE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|state| state.as_ref().map(|value| value.dropped_event_count))
        .unwrap_or_default();
    Ok(DiagnosticInfo {
        count: rows.len(),
        bytes,
        earliest_at,
        latest_at,
        dropped_event_count,
    })
}

pub(crate) fn write_app_log(
    app: &AppHandle,
    level: &str,
    target: &str,
    message: &str,
    details: Option<&serde_json::Value>,
) {
    let event = serde_json::json!({
        "id": uuid(),
        "occurredAt": timestamp(),
        "level": normalize_level(level),
        "source": "desktop",
        "module": normalize_label(target, "native"),
        "event": normalize_label(&format!("native.{target}"), "native.event"),
        "message": redact_text(&format_message(message, details)),
    });
    if active_scope_from_state().is_some() {
        append_active_event(app, event);
        return;
    }
    write_unscoped_log(app, event);
}

fn append_active_event(app: &AppHandle, mut event: serde_json::Value) {
    let lock = ACTIVE_SCOPE.get_or_init(|| Mutex::new(None));
    let Ok(mut state) = lock.lock() else { return };
    let Some(active) = state.as_mut() else { return };
    if let Some(sequence) = event.get("sequence").and_then(serde_json::Value::as_u64) {
        active.sequence = active.sequence.max(sequence + 1);
    } else {
        event["sequence"] = serde_json::Value::from(active.sequence);
        active.sequence += 1;
    }
    let stored = StoredDiagnostic {
        manifest: manifest(&active),
        event,
    };
    let scope = active.scope.clone();
    drop(state);
    if !append_stored(app, &scope, &stored) {
        increment_dropped();
    }
}

fn append_stored(app: &AppHandle, scope: &str, row: &StoredDiagnostic) -> bool {
    let Ok(text) = serde_json::to_string(row).map(|value| format!("{value}\n")) else {
        return false;
    };
    if text.len() > LOG_EVENT_BYTES {
        return false;
    }
    let directory = log_dir(app);
    if create_dir_all(&directory).is_err() {
        return false;
    }
    prune(app);
    let day = Utc::now().format("%Y-%m-%d").to_string();
    let prefix = scope_prefix(scope);
    let mut candidate = None;
    if let Ok(entries) = fs::read_dir(&directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if !name.starts_with(&format!("{prefix}_{day}_")) || !name.ends_with(".jsonl") {
                continue;
            }
            let size = entry
                .metadata()
                .map(|value| value.len())
                .unwrap_or_default();
            if size + text.len() as u64 <= LOG_SEGMENT_BYTES {
                candidate = Some(path);
            }
        }
    }
    let path = candidate.unwrap_or_else(|| {
        directory.join(format!("{prefix}_{day}_{}_{}.jsonl", now_millis(), uuid()))
    });
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .is_ok()
}

fn write_unscoped_log(app: &AppHandle, event: serde_json::Value) {
    let directory = log_dir(app);
    if create_dir_all(&directory).is_err() {
        return;
    }
    prune(app);
    let path = log_path(app);
    let Ok(text) = serde_json::to_string(&event).map(|value| format!("{value}\n")) else {
        return;
    };
    let should_reset = fs::metadata(&path)
        .map(|metadata| should_reset_unscoped_log(&metadata, text.len()))
        .unwrap_or(false);
    if should_reset {
        let _ = fs::write(&path, []);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(text.as_bytes());
    }
}

fn should_reset_unscoped_log(metadata: &fs::Metadata, incoming_bytes: usize) -> bool {
    let over_capacity = metadata.len().saturating_add(incoming_bytes as u64) > LOG_CAPACITY_BYTES;
    let expired = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| (value.as_millis() as i64) < now_millis() - LOG_RETENTION_MS)
        .unwrap_or(false);
    over_capacity || expired
}

fn active_scope(scope: &str) -> Result<DiagnosticScope, String> {
    let active =
        active_scope_from_state().ok_or_else(|| "当前账号尚未建立诊断日志上下文".to_string())?;
    if active.scope != scope {
        return Err("账号或租户已切换，请重新选择日志".to_string());
    }
    Ok(active)
}

fn active_scope_from_state() -> Option<DiagnosticScope> {
    ACTIVE_SCOPE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|state| state.clone())
}

fn increment_dropped() {
    if let Ok(mut state) = ACTIVE_SCOPE.get_or_init(|| Mutex::new(None)).lock() {
        if let Some(active) = state.as_mut() {
            active.dropped_event_count += 1;
        }
    }
}

fn manifest(scope: &DiagnosticScope) -> DiagnosticManifest {
    let _ = (&scope.user_id, &scope.tenant_key, &scope.owner_type);
    DiagnosticManifest {
        install_id: scope.install_id.clone(),
        session_id: scope.session_id.clone(),
        app_version: env!("SNACK_DESKTOP_VERSION").to_string(),
        build_number: env!("SNACK_DESKTOP_VERSION").to_string(),
        platform: env!("SNACK_DESKTOP_PLATFORM").to_string(),
        os_version: std::env::consts::OS.to_string(),
    }
}

fn validate_scope(input: &DiagnosticScopeInput) -> Result<(), String> {
    if input.scope.trim().is_empty() || input.scope.len() > 256 {
        return Err("诊断日志 scope 无效".to_string());
    }
    if input.user_id.is_empty()
        || input.user_id.len() > 64
        || !input.user_id.chars().all(|ch| ch.is_ascii_digit())
    {
        return Err("诊断日志 userId 无效".to_string());
    }
    if input.tenant_key.len() > 128
        || input.owner_type != "internal" && input.owner_type != "external"
    {
        return Err("诊断日志账号信息无效".to_string());
    }
    if !is_uuid(&input.session_id) {
        return Err("诊断日志 sessionId 无效".to_string());
    }
    Ok(())
}

fn normalize_event(
    mut event: serde_json::Value,
    source: &str,
) -> Result<serde_json::Value, String> {
    {
        let object = event
            .as_object_mut()
            .ok_or_else(|| "诊断日志事件必须是对象".to_string())?;
        if object.get("source").and_then(serde_json::Value::as_str) != Some(source) {
            return Err("诊断日志 source 不被允许".to_string());
        }
        let required = ["id", "occurredAt", "level", "module", "event"];
        if required.iter().any(|key| !object.contains_key(*key)) {
            return Err("诊断日志字段不完整".to_string());
        }
    }
    let serialized = serde_json::to_string(&event).map_err(|error| error.to_string())?;
    if serialized.len() > LOG_EVENT_BYTES {
        return Err("诊断日志事件过大".to_string());
    }
    let object = event
        .as_object()
        .ok_or_else(|| "诊断日志事件必须是对象".to_string())?;
    for key in ["module", "event"] {
        let value = object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if value.is_empty()
            || value.len() > 128
            || !value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "_.:-".contains(ch))
        {
            return Err("诊断日志标签无效".to_string());
        }
    }
    Ok(event)
}

fn scope_files(app: &AppHandle, scope: &str) -> Vec<PathBuf> {
    let prefix = format!("{}_", scope_prefix(scope));
    let mut files = fs::read_dir(log_dir(app))
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".jsonl"))
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn prune(app: &AppHandle) {
    let directory = log_dir(app);
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    let mut files = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.ends_with(".jsonl") && name != LOG_FILE_NAME {
                return None;
            }
            let modified_at = entry
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_millis() as i64;
            Some((path, modified_at))
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|(_, modified_at)| *modified_at);
    let mut bytes = files
        .iter()
        .map(|(path, _)| {
            fs::metadata(path)
                .map(|value| value.len())
                .unwrap_or_default()
        })
        .sum::<u64>();
    let cutoff = now_millis() - LOG_RETENTION_MS;
    for (path, modified_at) in files {
        if modified_at >= cutoff && bytes <= LOG_CAPACITY_BYTES {
            continue;
        }
        let size = fs::metadata(&path)
            .map(|value| value.len())
            .unwrap_or_default();
        if fs::remove_file(path).is_ok() {
            bytes = bytes.saturating_sub(size);
        }
    }
}

fn scope_prefix(scope: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(scope.as_bytes());
    hex_bytes(&hash.finalize())
}

fn install_id(app: &AppHandle) -> String {
    let path = log_dir(app).join("install-id");
    if let Ok(value) = fs::read_to_string(&path) {
        let value = value.trim();
        if is_uuid(value) {
            return value.to_string();
        }
    }
    let value = uuid();
    if let Some(parent) = path.parent() {
        let _ = create_dir_all(parent);
    }
    let _ = fs::write(path, &value);
    value
}

fn normalize_level(level: &str) -> &'static str {
    match level.to_ascii_lowercase().as_str() {
        "warn" | "warning" => "warn",
        "error" => "error",
        "fatal" => "fatal",
        _ => "info",
    }
}

fn normalize_label(value: &str, fallback: &str) -> String {
    let result = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || "_.:-".contains(*ch))
        .take(128)
        .collect::<String>();
    if result.is_empty() {
        fallback.to_string()
    } else {
        result
    }
}

fn format_message(message: &str, details: Option<&serde_json::Value>) -> String {
    let details = details.map(compact_json).unwrap_or_default();
    if details.is_empty() {
        message.to_string()
    } else {
        format!("{message} details={details}")
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_string())
}

fn redact_text(value: &str) -> String {
    let mut result = value
        .replace('\r', " ")
        .replace('\n', " ")
        .replace('\t', " ");
    for key in ["token", "password", "cookie", "authorization", "secret"] {
        let lower = result.to_ascii_lowercase();
        let Some(start) = lower.find(key) else {
            continue;
        };
        let suffix = &result[start + key.len()..];
        let Some(separator) = suffix.find(['=', ':']) else {
            continue;
        };
        let value_start = start + key.len() + separator + 1;
        let value_end = result[value_start..]
            .find(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
            .map(|offset| value_start + offset)
            .unwrap_or(result.len());
        result.replace_range(value_start..value_end, "[REDACTED]");
    }
    result.chars().take(2048).collect()
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or_default()
}

fn uuid() -> String {
    let timestamp = now_millis() as u64;
    let counter = UUID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&timestamp.to_be_bytes());
    bytes[8..].copy_from_slice(&counter.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, ch)| {
            if [8, 13, 18, 23].contains(&index) {
                ch == '-'
            } else {
                ch.is_ascii_hexdigit()
            }
        })
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_native_sensitive_text_and_labels() {
        let event = normalize_event(
            serde_json::json!({
                "id": uuid(),
                "occurredAt": timestamp(),
                "sequence": 1,
                "level": "error",
                "source": "webview",
                "module": "api",
                "event": "webview.failed"
            }),
            "webview",
        )
        .unwrap();
        assert_eq!(event["source"], "webview");
        assert!(normalize_label("bad value", "fallback").contains("badvalue"));
    }

    #[test]
    fn rejects_another_source_and_invalid_uuid() {
        let event = serde_json::json!({
            "id": uuid(),
            "occurredAt": timestamp(),
            "level": "error",
            "source": "rn",
            "module": "api",
            "event": "failed"
        });
        assert!(normalize_event(event, "webview").is_err());
        assert!(!is_uuid("not-a-uuid"));
    }

    #[test]
    fn scopes_are_hashed_before_being_used_in_filenames() {
        assert_ne!(scope_prefix("account-a"), scope_prefix("account-b"));
        assert_eq!(scope_prefix("account-a").len(), 64);
    }

    #[test]
    fn unscoped_logs_reset_when_capacity_would_be_exceeded() {
        let metadata = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        assert!(should_reset_unscoped_log(
            &metadata,
            LOG_CAPACITY_BYTES as usize,
        ));
    }
}
