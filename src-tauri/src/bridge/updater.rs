use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use super::protocol::{
    BridgeUpdateProgress, BridgeUpdateSnapshot, BridgeUpdateStage, BRIDGE_UPDATE_PROGRESS_EVENT,
};

pub(crate) struct BridgeUpdateCoordinator {
    state: Mutex<BridgeUpdateSnapshot>,
}

pub(crate) enum StartUpdateResult {
    Updating(Option<String>),
    Unsupported,
    Busy(String),
    Failed(String),
}

impl BridgeUpdateCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(idle_snapshot()),
        }
    }

    pub(crate) fn snapshot(&self) -> BridgeUpdateSnapshot {
        self.state.lock().expect("bridge updater poisoned").clone()
    }

    fn begin_check(&self) -> Result<(), BridgeUpdateSnapshot> {
        let mut state = self.state.lock().expect("bridge updater poisoned");
        if is_update_active(state.stage) {
            return Err(state.clone());
        }
        *state = stage_snapshot(BridgeUpdateStage::Checking, None);
        Ok(())
    }

    fn set_stage(&self, stage: BridgeUpdateStage, version: Option<String>) {
        *self.state.lock().expect("bridge updater poisoned") = stage_snapshot(stage, version);
    }

    fn set_error(&self, version: Option<String>, error: String) {
        *self.state.lock().expect("bridge updater poisoned") = BridgeUpdateSnapshot {
            stage: BridgeUpdateStage::Error,
            version,
            error: Some(error),
        };
    }
}

pub(crate) async fn start_update(app: &AppHandle) -> StartUpdateResult {
    if let Some(reason) = crate::meeting::desktop_update_block_reason(app) {
        return StartUpdateResult::Busy(reason);
    }

    let coordinator = app.state::<BridgeUpdateCoordinator>();
    if let Err(active) = coordinator.begin_check() {
        return StartUpdateResult::Updating(active.version);
    }

    match check_for_update(app).await {
        Ok(Some(update)) => start_install(app, update),
        Ok(None) => {
            coordinator.set_stage(BridgeUpdateStage::Idle, None);
            StartUpdateResult::Unsupported
        }
        Err(error) => {
            coordinator.set_error(None, error.clone());
            StartUpdateResult::Failed(error)
        }
    }
}

async fn check_for_update(app: &AppHandle) -> Result<Option<Update>, String> {
    app.updater()
        .map_err(|error| error.to_string())?
        .check()
        .await
        .map_err(|error| error.to_string())
}

fn start_install(app: &AppHandle, update: Update) -> StartUpdateResult {
    let version = update.version.clone();
    set_stage_and_emit(app, BridgeUpdateStage::Downloading, &version, 0, None);
    let task_app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = install_update(&task_app, update).await {
            fail_update(&task_app, error);
        }
    });
    StartUpdateResult::Updating(Some(version))
}

async fn install_update(app: &AppHandle, update: Update) -> Result<(), String> {
    let version = update.version.clone();
    let download_version = version.clone();
    let verify_version = version.clone();
    let download_app = app.clone();
    let verify_app = app.clone();
    let mut downloaded_bytes = 0_u64;
    let bytes = update
        .download(
            move |chunk_length, total_bytes| {
                downloaded_bytes = downloaded_bytes.saturating_add(chunk_length as u64);
                emit_download(
                    &download_app,
                    &download_version,
                    downloaded_bytes,
                    total_bytes,
                );
            },
            move || {
                set_stage_and_emit(
                    &verify_app,
                    BridgeUpdateStage::Verifying,
                    &verify_version,
                    0,
                    None,
                )
            },
        )
        .await
        .map_err(|error| error.to_string())?;

    if let Some(reason) = crate::meeting::desktop_update_block_reason(app) {
        return Err(reason);
    }
    set_stage_and_emit(app, BridgeUpdateStage::Installing, &version, 0, None);
    update.install(&bytes).map_err(|error| error.to_string())?;
    set_stage_and_emit(app, BridgeUpdateStage::Relaunching, &version, 0, None);
    app.restart();
}

fn emit_download(app: &AppHandle, version: &str, downloaded: u64, total: Option<u64>) {
    let percent = total.and_then(|size| calculate_percent(downloaded, size));
    emit_progress(
        app,
        BridgeUpdateProgress {
            stage: BridgeUpdateStage::Downloading,
            version: version.to_string(),
            downloaded_bytes: Some(downloaded),
            total_bytes: total,
            percent,
            error: None,
        },
    );
}

fn set_stage_and_emit(
    app: &AppHandle,
    stage: BridgeUpdateStage,
    version: &str,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
) {
    app.state::<BridgeUpdateCoordinator>()
        .set_stage(stage, Some(version.to_string()));
    emit_progress(app, progress(stage, version, downloaded_bytes, total_bytes));
}

fn fail_update(app: &AppHandle, error: String) {
    let coordinator = app.state::<BridgeUpdateCoordinator>();
    let version = coordinator.snapshot().version;
    coordinator.set_error(version.clone(), error.clone());
    crate::logging::write_app_log(
        app,
        "error",
        "bridge.updater",
        "Bridge-triggered desktop update failed",
        Some(&serde_json::json!({ "error": error, "version": version.clone() })),
    );
    if let Some(version) = version {
        let mut payload = progress(BridgeUpdateStage::Error, &version, 0, None);
        payload.error = Some(error);
        emit_progress(app, payload);
    }
}

fn emit_progress(app: &AppHandle, payload: BridgeUpdateProgress) {
    let _ = app.emit(BRIDGE_UPDATE_PROGRESS_EVENT, payload);
}

fn progress(
    stage: BridgeUpdateStage,
    version: &str,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
) -> BridgeUpdateProgress {
    BridgeUpdateProgress {
        stage,
        version: version.to_string(),
        downloaded_bytes: (downloaded_bytes > 0).then_some(downloaded_bytes),
        total_bytes,
        percent: total_bytes.and_then(|total| calculate_percent(downloaded_bytes, total)),
        error: None,
    }
}

fn calculate_percent(downloaded: u64, total: u64) -> Option<u8> {
    if total == 0 {
        return None;
    }
    Some(((downloaded.saturating_mul(100) / total).min(100)) as u8)
}

fn idle_snapshot() -> BridgeUpdateSnapshot {
    stage_snapshot(BridgeUpdateStage::Idle, None)
}

fn stage_snapshot(stage: BridgeUpdateStage, version: Option<String>) -> BridgeUpdateSnapshot {
    BridgeUpdateSnapshot {
        stage,
        version,
        error: None,
    }
}

fn is_update_active(stage: BridgeUpdateStage) -> bool {
    !matches!(stage, BridgeUpdateStage::Idle | BridgeUpdateStage::Error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_is_single_flight_and_retryable_after_error() {
        let coordinator = BridgeUpdateCoordinator::new();
        assert!(coordinator.begin_check().is_ok());
        assert_eq!(
            coordinator.begin_check().unwrap_err().stage,
            BridgeUpdateStage::Checking
        );
        coordinator.set_error(None, "network".to_string());
        assert!(coordinator.begin_check().is_ok());
    }

    #[test]
    fn percent_is_bounded_and_handles_unknown_sizes() {
        assert_eq!(calculate_percent(20, 100), Some(20));
        assert_eq!(calculate_percent(200, 100), Some(100));
        assert_eq!(calculate_percent(1, 0), None);
    }
}
