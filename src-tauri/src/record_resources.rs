use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager, WebviewWindow};

use crate::{logging, web::is_allowed_web_origin};

const MODEL_IDS: [&str; 4] = [
    "iic--speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch",
    "iic--speech_fsmn_vad_zh-cn-16k-common-pytorch",
    "iic--punc_ct-transformer_cn-en-common-vocab471067-large",
    "iic--speech_campplus_sv_zh-cn_16k-common",
];
const FFMPEG_CANDIDATES: [&str; 3] = [
    "/opt/homebrew/bin/ffmpeg",
    "/usr/local/bin/ffmpeg",
    "/usr/bin/ffmpeg",
];

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingResourceStatus {
    available: bool,
    detail: String,
}

#[tauri::command]
pub(crate) fn get_recording_resource_status(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<RecordingResourceStatus, String> {
    ensure_allowed_origin(&window)?;
    let status = inspect_installed_resources(&app)?;
    log_resource_status(&app, "resource status inspected", &status);
    Ok(status)
}

#[tauri::command]
pub(crate) fn install_recording_resources(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<RecordingResourceStatus, String> {
    ensure_allowed_origin(&window)?;
    let status = inspect_installed_resources(&app)?;
    if status.available {
        log_resource_status(&app, "resource install reused Snack Record files", &status);
        return Ok(status);
    }
    log_resource_status(&app, "resource repair delegated to Snack Record", &status);
    Err(format!(
        "{}。请通过 Snack Record 安装或修复本地资源，Snack 不会重复下载资源包",
        status.detail
    ))
}

fn ensure_allowed_origin(window: &WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|error| error.to_string())?;
    if is_allowed_web_origin(&url) {
        return Ok(());
    }
    Err("origin is not allowed to inspect Snack Record resources".to_string())
}

fn log_resource_status(app: &AppHandle, message: &str, status: &RecordingResourceStatus) {
    logging::write_app_log(
        app,
        "info",
        "snack-record-resources",
        message,
        Some(&serde_json::json!({
            "available": status.available,
            "detail": status.detail,
        })),
    );
}

fn inspect_installed_resources(app: &AppHandle) -> Result<RecordingResourceStatus, String> {
    #[cfg(target_os = "macos")]
    {
        let root = snack_record_resource_root(app)?;
        Ok(inspect_resource_root(&root, has_ffmpeg()))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(RecordingResourceStatus {
            available: false,
            detail: "当前桌面系统暂不支持复用 Snack Record 本地资源".to_string(),
        })
    }
}

#[cfg(target_os = "macos")]
fn snack_record_resource_root(app: &AppHandle) -> Result<PathBuf, String> {
    let home = app.path().home_dir().map_err(|error| error.to_string())?;
    Ok(home.join("Library/Application Support/Snack Record"))
}

fn inspect_resource_root(root: &Path, ffmpeg_available: bool) -> RecordingResourceStatus {
    let mut missing = missing_runtime(root);
    missing.extend(missing_models(root));
    if !ffmpeg_available {
        missing.push("FFmpeg".to_string());
    }
    if missing.is_empty() {
        return RecordingResourceStatus {
            available: true,
            detail: "已复用本机 Snack Record 资源：Python、4 个语音模型和 FFmpeg 均可用"
                .to_string(),
        };
    }
    RecordingResourceStatus {
        available: false,
        detail: format!("缺少：{}", missing.join("、")),
    }
}

fn missing_runtime(root: &Path) -> Vec<String> {
    let python = root.join("Runtime/venv/bin/python");
    if python.is_file() {
        return Vec::new();
    }
    vec!["Python 运行时".to_string()]
}

fn missing_models(root: &Path) -> Vec<String> {
    MODEL_IDS
        .iter()
        .filter(|model| !model_configuration_path(root, model).is_file())
        .map(|model| format!("模型 {model}"))
        .collect()
}

fn model_configuration_path(root: &Path, model: &str) -> PathBuf {
    root.join("Models/models")
        .join(model)
        .join("snapshots/master/configuration.json")
}

fn has_ffmpeg() -> bool {
    FFMPEG_CANDIDATES
        .iter()
        .any(|candidate| Path::new(candidate).is_file())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn complete_resource_root_is_available() {
        let root = create_test_root("complete");
        create_complete_resource_root(&root);

        let status = inspect_resource_root(&root, true);

        assert!(status.available);
        assert!(status.detail.contains("4 个语音模型"));
        cleanup_test_root(&root);
    }

    #[test]
    fn missing_resource_root_reports_concrete_gaps() {
        let root = create_test_root("missing");

        let status = inspect_resource_root(&root, false);

        assert!(!status.available);
        assert!(status.detail.contains("Python 运行时"));
        assert!(status.detail.contains("FFmpeg"));
        cleanup_test_root(&root);
    }

    fn create_complete_resource_root(root: &Path) {
        fs::create_dir_all(root.join("Runtime/venv/bin")).unwrap();
        fs::write(root.join("Runtime/venv/bin/python"), "").unwrap();
        for model in MODEL_IDS {
            let configuration = model_configuration_path(root, model);
            fs::create_dir_all(configuration.parent().unwrap()).unwrap();
            fs::write(configuration, "{}").unwrap();
        }
    }

    fn create_test_root(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "snack-record-resource-test-{}-{label}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn cleanup_test_root(root: &Path) {
        fs::remove_dir_all(root).unwrap();
    }
}
