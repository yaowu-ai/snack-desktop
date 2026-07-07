use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager};

const LOG_FILE_NAME: &str = "desktop.log";

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

pub(crate) fn write_app_log(
    app: &AppHandle,
    level: &str,
    target: &str,
    message: &str,
    details: Option<&serde_json::Value>,
) {
    let dir = log_dir(app);
    let _ = create_dir_all(&dir);

    let path = dir.join(LOG_FILE_NAME);
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };

    let details_text = details
        .map(|value| format!(" details={}", compact_json(value)))
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "{} level={} target={} message={}{}",
        timestamp_millis(),
        sanitize_field(level),
        sanitize_field(target),
        sanitize_field(message),
        details_text
    );
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<unserializable>\"".to_string())
}

fn sanitize_field(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' => ' ',
            other => other,
        })
        .collect()
}
