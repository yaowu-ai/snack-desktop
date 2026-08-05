//! Local model resource manager: install, pause/resume/cancel download,
//! verify, validate, uninstall, update.
//!
//! Installation pipeline (mirrors the PRD):
//!   check disk space → download (resumable, pausable, cancellable)
//!   → sha256 verify → install (move into place + manifest)
//!   → runtime self-check → inference self-check → ready
//!
//! Downloads are user-initiated only; the manager never silently downloads or
//! auto-updates multi-GB models.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::RANGE;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::meeting::audio::{available_bytes, disk_has_room, sha256_file};
use crate::meeting::catalog::{
    find_model, platform_label, CatalogModel, ModelArtifact, ModelKey, CATALOG_VERSION,
};
use crate::meeting::state::{
    now_rfc3339, persist_json_atomic, DownloadProgress, MeetingStore, ResourceState, ResourceStatus,
};

const MANIFEST_FILE: &str = "manifest.json";
const PART_EXTENSION: &str = "part";
const INSTALL_PROGRESS_EVENT: &str = "meeting-install-progress";
const MAX_DOWNLOAD_RETRIES: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelManifest {
    pub(crate) model_key: String,
    pub(crate) catalog_version: u32,
    pub(crate) filename: String,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: String,
    pub(crate) installed_at: String,
    #[serde(default)]
    pub(crate) artifacts: Vec<ModelArtifactManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelArtifactManifest {
    pub(crate) filename: String,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallProgressPayload {
    pub(crate) stage: &'static str,
    pub(crate) percent: u8,
    pub(crate) downloaded_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) speed_bytes_per_sec: u64,
}

/// Control channel for an in-flight download.
struct DownloadControl {
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

pub(crate) struct InstallManager {
    inner: Mutex<Option<InstallInner>>,
}

struct InstallInner {
    control: DownloadControl,
}

impl InstallManager {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    fn control(&self) -> Option<DownloadControl> {
        self.inner
            .lock()
            .expect("install manager poisoned")
            .as_ref()
            .map(|inner| DownloadControl {
                pause: Arc::clone(&inner.control.pause),
                cancel: Arc::clone(&inner.control.cancel),
            })
    }

    fn register(&self, control: DownloadControl) {
        *self.inner.lock().expect("install manager poisoned") = Some(InstallInner { control });
    }

    fn unregister(&self) {
        *self.inner.lock().expect("install manager poisoned") = None;
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Start installing (or updating) the given model. Returns immediately; the
/// pipeline runs on a tokio task and reports through events.
pub(crate) fn start_install(
    app: AppHandle,
    store: MeetingStore,
    manager: Arc<InstallManager>,
    model_key: ModelKey,
) -> Result<(), String> {
    let model = find_model(model_key).ok_or_else(|| "未知的模型".to_string())?;

    let mut resource = store.load_resource();
    if !resource.state.is_idle() {
        return Err("当前已有模型安装任务在进行中".to_string());
    }
    if resource.state == ResourceState::Ready {
        // Reinstall (e.g. user confirmed an update): remove the old copy first.
        let freed = uninstall_model_files(&store, &resource)?;
        log_install(
            &app,
            "info",
            "replacing installed model",
            serde_json::json!({ "freedBytes": freed }),
        );
    }

    resource = ResourceStatus::default().with_state(ResourceState::Checking);
    resource.model_key = Some(model.key.as_str().to_string());
    resource.model_size_bytes = Some(model.size_bytes);
    store.save_resource(&resource)?;
    crate::meeting::emit_state(&app, &store);

    let (pause, cancel) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let progress = Arc::new(Mutex::new(DownloadProgress {
        total_bytes: model.size_bytes,
        ..Default::default()
    }));
    manager.register(DownloadControl {
        pause: Arc::clone(&pause),
        cancel: Arc::clone(&cancel),
    });

    let app_for_task = app.clone();
    let store_for_task = store.clone_for_task();
    let manager_for_task = Arc::clone(&manager);
    let model_for_task = model.clone();
    tauri::async_runtime::spawn(async move {
        let result = run_install_pipeline(
            app_for_task.clone(),
            store_for_task.clone(),
            manager_for_task.clone(),
            model_for_task.clone(),
            pause,
            cancel,
            progress,
        )
        .await;
        manager_for_task.unregister();
        match result {
            Ok(()) => {
                log_install(
                    &app_for_task,
                    "info",
                    "model installed",
                    serde_json::json!({}),
                );
                let mut resource = store_for_task.load_resource();
                if resource.state != ResourceState::Ready {
                    resource = resource.with_state(ResourceState::Ready);
                    resource.error = None;
                    store_for_task.save_resource(&resource).ok();
                }
                crate::meeting::emit_state(&app_for_task, &store_for_task);
                crate::meeting::notifications::notify_model_ready(&app_for_task);
            }
            Err((state, message)) => {
                log_install(
                    &app_for_task,
                    "error",
                    "model install failed",
                    serde_json::json!({ "message": message }),
                );
                let mut resource = store_for_task.load_resource();
                resource = resource.with_state(state);
                resource.error = Some(message);
                store_for_task.save_resource(&resource).ok();
                crate::meeting::emit_state(&app_for_task, &store_for_task);
            }
        }
    });

    Ok(())
}

/// The install pipeline. Returns Err((terminal_state, message)) on failure.
async fn run_install_pipeline(
    app: AppHandle,
    store: MeetingStore,
    _manager: Arc<InstallManager>,
    model: CatalogModel,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<DownloadProgress>>,
) -> Result<(), (ResourceState, String)> {
    let requirement = model.install_requirement_bytes();
    let root = store.root().clone();

    // 1. Disk space check (before any download).
    set_resource(&store, ResourceState::Checking, None);
    match disk_has_room(&root, requirement) {
        Ok(true) => {}
        Ok(false) => {
            return Err((
                ResourceState::InsufficientDisk,
                format!(
                    "磁盘空间不足：安装需要约 {}，请释放空间后重试",
                    format_bytes(requirement)
                ),
            ))
        }
        Err(error) => return Err((ResourceState::Failed, format!("无法检查磁盘空间: {error}"))),
    }

    // 2. Download every file in the native model bundle (resumable).
    set_resource(&store, ResourceState::Downloading, None);
    let mut downloaded_before = 0u64;
    let mut part_paths = Vec::with_capacity(model.artifacts.len());
    for artifact in &model.artifacts {
        let part_path = store.downloads_dir().join(format!(
            "{}-{}.{PART_EXTENSION}",
            model.key.as_str(),
            artifact.filename
        ));
        if let Err(message) = download_model(
            &app,
            &store,
            artifact,
            model.size_bytes,
            downloaded_before,
            &part_path,
            &pause,
            &cancel,
            &progress,
        )
        .await
        {
            if cancel.load(Ordering::SeqCst) {
                for path in &part_paths {
                    let _ = fs::remove_file(path);
                }
                let _ = fs::remove_file(&part_path);
                let mut resource = store.load_resource();
                resource = resource.with_state(ResourceState::NotInstalled);
                resource.error = Some("已取消安装".to_string());
                store.save_resource(&resource).ok();
                crate::meeting::emit_state(&app, &store);
                return Err((ResourceState::NotInstalled, "已取消安装".to_string()));
            }
            return Err((ResourceState::Failed, message));
        }
        downloaded_before += artifact.size_bytes;
        part_paths.push(part_path);
    }

    if cancel.load(Ordering::SeqCst) {
        for path in &part_paths {
            let _ = fs::remove_file(path);
        }
        return Err((ResourceState::NotInstalled, "已取消安装".to_string()));
    }

    // 3. Integrity verification.
    set_resource(&store, ResourceState::Verifying, None);
    for (artifact, part_path) in model.artifacts.iter().zip(&part_paths) {
        let digest = sha256_file(part_path)
            .map_err(|error| (ResourceState::Corrupted, format!("校验失败: {error}")))?;
        if !digest.eq_ignore_ascii_case(artifact.sha256) {
            let _ = fs::remove_file(part_path);
            return Err((
                ResourceState::Corrupted,
                format!(
                    "{} 校验失败（SHA-256 不匹配），请重新下载",
                    artifact.filename
                ),
            ));
        }
    }

    // 4. Install the private Snack-owned bundle + write manifest.
    set_resource(&store, ResourceState::Installing, None);
    let model_dir = store.models_dir().join(model.key.as_str());
    fs::create_dir_all(&model_dir)
        .map_err(|error| (ResourceState::Failed, format!("无法创建模型目录: {error}")))?;
    for (artifact, part_path) in model.artifacts.iter().zip(&part_paths) {
        let final_path = model_dir.join(artifact.filename);
        let _ = fs::remove_file(&final_path);
        fs::rename(part_path, &final_path)
            .map_err(|error| (ResourceState::Failed, format!("无法安装模型: {error}")))?;
    }
    let manifest = ModelManifest {
        model_key: model.key.as_str().to_string(),
        catalog_version: model.catalog_version,
        filename: model.filename.to_string(),
        size_bytes: model.size_bytes,
        sha256: model.sha256.to_string(),
        installed_at: now_rfc3339(),
        artifacts: model
            .artifacts
            .iter()
            .map(|artifact| ModelArtifactManifest {
                filename: artifact.filename.to_string(),
                size_bytes: artifact.size_bytes,
                sha256: artifact.sha256.to_string(),
            })
            .collect(),
    };
    persist_json_atomic(&model_dir.join(MANIFEST_FILE), &manifest)
        .map_err(|error| (ResourceState::Failed, format!("无法写入模型清单: {error}")))?;

    // 5. Runtime + inference self-check.
    set_resource(&store, ResourceState::Validating, None);
    crate::meeting::transcribe::validate_model(model.key, &model_dir)
        .map_err(|error| (ResourceState::Failed, format!("推理自检失败: {error}")))?;

    let mut resource = store.load_resource();
    resource.installed_size_bytes = Some(model.installed_size_bytes());
    resource.error = None;
    store
        .save_resource(&resource)
        .map_err(|error| (ResourceState::Failed, format!("无法保存模型状态: {error}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

async fn download_model(
    app: &AppHandle,
    store: &MeetingStore,
    artifact: &ModelArtifact,
    package_size_bytes: u64,
    package_downloaded_before: u64,
    part_path: &Path,
    pause: &AtomicBool,
    cancel: &AtomicBool,
    progress: &Arc<Mutex<DownloadProgress>>,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "无法创建下载客户端".to_string())?;

    let mut downloaded_bytes = fs::metadata(part_path).map(|meta| meta.len()).unwrap_or(0);
    // A stale part file that is already complete or oversized is discarded.
    if downloaded_bytes >= artifact.size_bytes {
        downloaded_bytes = 0;
        let _ = fs::remove_file(part_path);
    }

    let mut retries = 0u32;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("下载已取消".to_string());
        }
        // Honor pause before starting a (re)connect.
        wait_while_paused(pause, cancel).await?;

        let range = if downloaded_bytes > 0 {
            format!("bytes={downloaded_bytes}-")
        } else {
            String::new()
        };
        let mut request = client.get(artifact.url);
        if !range.is_empty() {
            request = request.header(RANGE, range);
        }

        let response = request
            .send()
            .await
            .map_err(|error| format!("下载失败: {error}"))?;

        if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // Server lost the file; restart from scratch.
            downloaded_bytes = 0;
            let _ = fs::remove_file(part_path);
            continue;
        }

        // A server may ignore Range and return the whole file. Never append a
        // full response to a partial bundle component.
        if downloaded_bytes > 0 && response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            downloaded_bytes = 0;
            let _ = fs::remove_file(part_path);
            continue;
        }

        let status = response.status();
        let mut stream = if status.is_success() {
            response.bytes_stream()
        } else {
            return Err(format!("下载请求失败 (HTTP {status})"));
        };
        {
            let mut progress = progress.lock().expect("progress poisoned");
            progress.total_bytes = package_size_bytes;
            progress.downloaded_bytes = package_downloaded_before + downloaded_bytes;
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part_path)
            .map_err(|error| format!("无法写入下载文件: {error}"))?;

        let mut speed_window: VecDeque<(Instant, u64)> = Default::default();
        let mut last_emit = Instant::now();

        while let Some(chunk) = stream.next().await {
            if cancel.load(Ordering::SeqCst) {
                return Err("下载已取消".to_string());
            }
            let chunk = chunk.map_err(|error| format!("下载中断: {error}"))?;
            file.write_all(&chunk)
                .map_err(|error| format!("无法写入下载文件: {error}"))?;
            downloaded_bytes += chunk.len() as u64;
            speed_window.push_back((Instant::now(), chunk.len() as u64));
            while speed_window
                .front()
                .is_some_and(|(at, _)| at.elapsed() > Duration::from_secs(2))
            {
                speed_window.pop_front();
            }

            if last_emit.elapsed() >= Duration::from_millis(150) {
                let speed = speed_window
                    .iter()
                    .map(|(_, bytes)| *bytes)
                    .sum::<u64>()
                    .saturating_div(2);
                let package_downloaded = package_downloaded_before + downloaded_bytes;
                let percent = if package_size_bytes > 0 {
                    ((package_downloaded * 100 / package_size_bytes).min(100)) as u8
                } else {
                    0
                };
                {
                    let mut progress = progress.lock().expect("progress poisoned");
                    progress.downloaded_bytes = package_downloaded;
                    progress.speed_bytes_per_sec = speed;
                    progress.percent = percent;
                }
                let _ = app.emit(
                    INSTALL_PROGRESS_EVENT,
                    InstallProgressPayload {
                        stage: "downloading",
                        percent,
                        downloaded_bytes: package_downloaded,
                        total_bytes: package_size_bytes,
                        speed_bytes_per_sec: speed,
                    },
                );
                last_emit = Instant::now();
            }

            // Pause check: wait until resumed or cancelled, then reconnect
            // with a Range header from the current position.
            if pause.load(Ordering::SeqCst) {
                file.flush().ok();
                {
                    let mut progress = progress.lock().expect("progress poisoned");
                    let package_downloaded = package_downloaded_before + downloaded_bytes;
                    progress.downloaded_bytes = package_downloaded;
                    progress.percent = if package_size_bytes > 0 {
                        ((package_downloaded * 100 / package_size_bytes).min(100)) as u8
                    } else {
                        0
                    };
                }
                set_resource(store, ResourceState::Paused, None);
                wait_while_paused(pause, cancel).await?;
                set_resource(store, ResourceState::Downloading, None);
                break;
            }
        }

        if downloaded_bytes >= artifact.size_bytes {
            file.flush().ok();
            return Ok(());
        }

        // Reached end of stream without completing: retry with Range, unless
        // paused (handled above by break → outer loop).
        if retries >= MAX_DOWNLOAD_RETRIES {
            return Err("下载多次中断，请重试".to_string());
        }
        retries += 1;
        tokio::time::sleep(Duration::from_secs(1 << retries.min(4))).await;
    }
}

async fn wait_while_paused(pause: &AtomicBool, cancel: &AtomicBool) -> Result<(), String> {
    while pause.load(Ordering::SeqCst) {
        if cancel.load(Ordering::SeqCst) {
            return Err("下载已取消".to_string());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pause / resume / cancel / uninstall
// ---------------------------------------------------------------------------

pub(crate) fn pause_install(store: &MeetingStore, manager: &InstallManager) -> Result<(), String> {
    let control = manager
        .control()
        .ok_or_else(|| "当前没有进行中的下载".to_string())?;
    control.pause.store(true, Ordering::SeqCst);
    set_resource(store, ResourceState::Paused, None);
    Ok(())
}

pub(crate) fn resume_install(store: &MeetingStore, manager: &InstallManager) -> Result<(), String> {
    let control = manager
        .control()
        .ok_or_else(|| "当前没有暂停的下载".to_string())?;
    control.pause.store(false, Ordering::SeqCst);
    if store.load_resource().state == ResourceState::Paused {
        set_resource(store, ResourceState::Downloading, None);
    }
    Ok(())
}

pub(crate) fn cancel_install(store: &MeetingStore, manager: &InstallManager) -> Result<(), String> {
    let control = manager
        .control()
        .ok_or_else(|| "当前没有进行中的下载".to_string())?;
    control.cancel.store(true, Ordering::SeqCst);
    control.pause.store(false, Ordering::SeqCst);
    set_resource(store, ResourceState::NotInstalled, None);
    Ok(())
}

/// Uninstall the installed model. Returns the freed bytes. Blocked while a
/// meeting task is recording or transcribing.
pub(crate) fn uninstall_model(
    store: &MeetingStore,
    resource: &ResourceStatus,
) -> Result<u64, String> {
    let freed = uninstall_model_files(store, resource)?;
    let mut updated = store.load_resource();
    updated = updated.with_state(ResourceState::NotInstalled);
    updated.model_key = None;
    updated.model_size_bytes = None;
    updated.installed_size_bytes = None;
    updated.download = None;
    updated.error = None;
    store.save_resource(&updated)?;
    Ok(freed)
}

fn uninstall_model_files(store: &MeetingStore, resource: &ResourceStatus) -> Result<u64, String> {
    let Some(model_key) = resource.model_key.as_deref() else {
        return Ok(0);
    };
    let model_dir = store.models_dir().join(model_key);
    if !model_dir.exists() {
        return Ok(0);
    }
    let freed = directory_size(&model_dir);
    fs::remove_dir_all(&model_dir).map_err(|error| format!("无法删除模型目录: {error}"))?;
    // Also clean any leftover partial downloads for this model bundle.
    if let Ok(entries) = fs::read_dir(store.downloads_dir()) {
        let prefix = format!("{model_key}-");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && name.ends_with(&format!(".{PART_EXTENSION}")) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(freed)
}

/// Reconcile resource state after an app restart (or crash).
pub(crate) fn reconcile_resource(store: &MeetingStore, manager: &InstallManager) -> ResourceStatus {
    let mut resource = store.load_resource();
    match resource.state {
        ResourceState::Downloading | ResourceState::Paused => {
            manager.unregister();
            resource = resource.with_state(ResourceState::Failed);
            resource.error = Some("下载被中断，请重试".to_string());
            let _ = store.save_resource(&resource);
        }
        ResourceState::Verifying | ResourceState::Installing | ResourceState::Validating => {
            manager.unregister();
            resource = resource.with_state(ResourceState::Failed);
            resource.error = Some("安装被中断，请重试".to_string());
            let _ = store.save_resource(&resource);
        }
        ResourceState::Ready => {
            // Verify the installed copy still exists and matches its manifest.
            match installed_model_path(store, &resource) {
                Ok(path) if path.exists() => {}
                _ => {
                    resource = resource.with_state(ResourceState::Corrupted);
                    resource.error = Some("模型文件缺失或损坏，请重新安装".to_string());
                    let _ = store.save_resource(&resource);
                }
            }
        }
        ResourceState::Checking => {
            manager.unregister();
            resource = resource.with_state(ResourceState::Failed);
            resource.error = Some("安装被中断，请重试".to_string());
            let _ = store.save_resource(&resource);
        }
        _ => {}
    }
    resource
}

/// Path to the installed model file, validated against its manifest.
pub(crate) fn installed_model_path(
    store: &MeetingStore,
    resource: &ResourceStatus,
) -> Result<PathBuf, String> {
    let Some(model_key) = resource.model_key.as_deref() else {
        return Err("模型未安装".to_string());
    };
    let Some(model) = find_model(ModelKey::parse(model_key).ok_or_else(|| "未知模型".to_string())?)
    else {
        return Err("未知模型".to_string());
    };
    let model_dir = store.models_dir().join(model.key.as_str());
    let manifest_path = model_dir.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        return Err("模型清单缺失".to_string());
    }
    let manifest: ModelManifest = serde_json::from_slice(
        &fs::read(&manifest_path).map_err(|_| "模型清单读取失败".to_string())?,
    )
    .map_err(|_| "模型清单无效".to_string())?;
    if manifest.model_key != model.key.as_str() || manifest.catalog_version < model.catalog_version
    {
        return Err("模型版本需要更新".to_string());
    }
    let path = model_dir.join(model.filename);
    if !path.exists() {
        return Err("模型文件缺失".to_string());
    }
    if manifest.artifacts.is_empty() {
        if manifest.size_bytes != 0
            && fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) != manifest.size_bytes
        {
            return Err("模型文件大小不符".to_string());
        }
    } else {
        if manifest.artifacts.len() != model.artifacts.len() {
            return Err("模型组件不完整".to_string());
        }
        for expected in &model.artifacts {
            let Some(saved) = manifest
                .artifacts
                .iter()
                .find(|artifact| artifact.filename == expected.filename)
            else {
                return Err(format!("模型组件缺失: {}", expected.filename));
            };
            let artifact_path = model_dir.join(expected.filename);
            let actual_size = fs::metadata(&artifact_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if actual_size != expected.size_bytes
                || saved.size_bytes != expected.size_bytes
                || !saved.sha256.eq_ignore_ascii_case(expected.sha256)
            {
                return Err(format!("模型组件损坏: {}", expected.filename));
            }
        }
    }
    Ok(path)
}

/// Whether the installed model needs an update (catalog version bumped).
pub(crate) fn installed_needs_update(store: &MeetingStore, resource: &ResourceStatus) -> bool {
    let Some(model_key) = resource.model_key.as_deref() else {
        return false;
    };
    let Some(model) = model_by_key(model_key) else {
        return false;
    };
    let model_dir = store.models_dir().join(model.key.as_str());
    let manifest_path = model_dir.join(MANIFEST_FILE);
    let Ok(bytes) = fs::read(&manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<ModelManifest>(&bytes) else {
        return false;
    };
    manifest.catalog_version < model.catalog_version
}

fn model_by_key(key: &str) -> Option<CatalogModel> {
    ModelKey::parse(key).and_then(find_model)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn set_resource(store: &MeetingStore, state: ResourceState, error: Option<String>) {
    let mut resource = store.load_resource();
    resource = resource.with_state(state);
    if error.is_some() {
        resource.error = error;
    }
    let _ = store.save_resource(&resource);
}

fn directory_size(path: &Path) -> u64 {
    fn walk(path: &Path, total: &mut u64) {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    walk(&entry_path, total);
                } else if let Ok(meta) = entry.metadata() {
                    *total += meta.len();
                }
            }
        }
    }
    let mut total = 0u64;
    walk(path, &mut total);
    total
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= GB {
        format!("{:.2} GB", bytes as f64 / GB)
    } else {
        format!("{:.1} MB", bytes as f64 / MB)
    }
}

fn log_install(app: &AppHandle, level: &str, message: &str, details: serde_json::Value) {
    crate::logging::write_app_log(
        app,
        level,
        "meeting-model",
        message,
        Some(&serde_json::json!({
            "platform": platform_label(),
            "catalogVersion": CATALOG_VERSION,
            "details": details,
        })),
    );
}

/// Estimated free disk space at the meeting data root.
pub(crate) fn free_disk_bytes(store: &MeetingStore) -> Result<u64, String> {
    available_bytes(store.root())
}

#[cfg(test)]
mod tests {
    use super::{format_bytes, ModelManifest, PART_EXTENSION};

    #[test]
    fn format_bytes_is_readable() {
        assert!(format_bytes(3_095_033_483).contains("GB"));
        assert!(format_bytes(487_601_967).contains("MB"));
    }

    #[test]
    fn manifest_roundtrip() {
        let manifest = ModelManifest {
            model_key: "large-v3".to_string(),
            catalog_version: 1,
            filename: "ggml-large-v3.bin".to_string(),
            size_bytes: 3_095_033_483,
            sha256: "ab".repeat(32),
            installed_at: "2026-08-01T00:00:00Z".to_string(),
            artifacts: Vec::new(),
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let decoded: ModelManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.model_key, "large-v3");
        assert_eq!(decoded.size_bytes, 3_095_033_483);
    }

    #[test]
    fn part_extension_constant() {
        assert_eq!(PART_EXTENSION, "part");
    }
}
