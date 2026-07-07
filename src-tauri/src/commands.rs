use tauri::{AppHandle, WebviewWindow};

use crate::attention::update_desktop_attention;
use crate::download::{
    download_snack_file_inner, emit_failed_download, open_downloaded_path, reveal_downloaded_path,
    DownloadSnackFileRequest, DownloadSnackFileResult,
};
use crate::logging::{log_dir, log_path, write_app_log};
use crate::platform::open_path;
use crate::web::is_allowed_web_origin;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DownloadedPathRequest {
    path: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopLogRequest {
    level: String,
    target: String,
    message: String,
    details: Option<serde_json::Value>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopLogInfo {
    path: String,
    directory: String,
}

#[tauri::command]
pub(crate) fn exit_after_force_update_cancel(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<(), String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to close the desktop shell".to_string());
    }

    app.exit(0);
    Ok(())
}

#[tauri::command]
pub(crate) fn set_desktop_attention(
    window: WebviewWindow,
    unread_count: u32,
    level: String,
) -> Result<(), String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to update desktop attention".to_string());
    }

    update_desktop_attention(&window, unread_count, &level);
    Ok(())
}

#[tauri::command]
pub(crate) fn write_desktop_log(
    app: AppHandle,
    window: WebviewWindow,
    request: DesktopLogRequest,
) -> Result<DesktopLogInfo, String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to write desktop logs".to_string());
    }

    write_app_log(
        &app,
        &request.level,
        &request.target,
        &request.message,
        request.details.as_ref(),
    );

    Ok(resolve_desktop_log_info(&app))
}

#[tauri::command]
pub(crate) fn reveal_desktop_log_dir(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<DesktopLogInfo, String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to open desktop logs".to_string());
    }

    let dir = log_dir(&app);
    std::fs::create_dir_all(&dir).map_err(|_| "failed to create desktop log directory".to_string())?;
    open_path(&dir)?;

    Ok(resolve_desktop_log_info(&app))
}

#[tauri::command]
pub(crate) async fn download_snack_file(
    app: AppHandle,
    window: WebviewWindow,
    request: DownloadSnackFileRequest,
) -> Result<DownloadSnackFileResult, String> {
    let download_id = request.download_id.clone();
    let filename = request.filename.clone();
    let result = download_snack_file_inner(app, window.clone(), request).await;
    if let Err(message) = &result {
        emit_failed_download(&window, download_id, filename, message.clone());
    }
    result
}

#[tauri::command]
pub(crate) fn open_downloaded_file(
    app: AppHandle,
    window: WebviewWindow,
    request: DownloadedPathRequest,
) -> Result<(), String> {
    open_downloaded_path(&app, &window, &request.path)
}

#[tauri::command]
pub(crate) fn reveal_downloaded_file(
    app: AppHandle,
    window: WebviewWindow,
    request: DownloadedPathRequest,
) -> Result<(), String> {
    reveal_downloaded_path(&app, &window, &request.path)
}

fn resolve_desktop_log_info(app: &AppHandle) -> DesktopLogInfo {
    DesktopLogInfo {
        path: log_path(app).to_string_lossy().to_string(),
        directory: log_dir(app).to_string_lossy().to_string(),
    }
}
