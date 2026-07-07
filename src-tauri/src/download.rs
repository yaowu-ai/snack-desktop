use futures_util::StreamExt;
use reqwest::header::{COOKIE, LOCATION, USER_AGENT};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tauri::path::BaseDirectory;
use tauri::window::{ProgressBarState, ProgressBarStatus};
use tauri::{AppHandle, Emitter, Manager, Url, WebviewWindow};
use tokio::io::AsyncWriteExt;

use crate::platform::{open_path, reveal_path};
use crate::web::{desktop_user_agent, is_allowed_web_origin};

const DOWNLOAD_PROGRESS_EVENT: &str = "snack-download-progress";
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DownloadSnackFileRequest {
    pub(crate) download_id: String,
    pub(crate) url: String,
    pub(crate) filename: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DownloadSnackFileResult {
    download_id: String,
    path: String,
    downloaded_bytes: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProgressPayload {
    download_id: String,
    status: &'static str,
    filename: Option<String>,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    percent: Option<u8>,
    path: Option<String>,
    message: Option<String>,
}

pub(crate) async fn download_snack_file_inner(
    app: AppHandle,
    window: WebviewWindow,
    request: DownloadSnackFileRequest,
) -> Result<DownloadSnackFileResult, String> {
    validate_download_id(&request.download_id)?;

    let current_url = window
        .url()
        .map_err(|_| "failed to read current window URL".to_string())?;
    if !is_allowed_web_origin(&current_url) {
        return Err("origin is not allowed to start native downloads".to_string());
    }

    let mut file_url = resolve_snack_file_url(&current_url, &request.url)?;
    ensure_supported_file_endpoint(&file_url)?;
    ensure_download_query(&mut file_url);

    let auth_token = read_auth_token_cookie(&window, file_url.clone())?;
    let filename = sanitize_filename(&request.filename)?;
    let destination_dir = resolve_destination_dir(&app)?;
    let final_path = resolve_available_path(&destination_dir, &filename);
    let part_path = final_path.with_extension(format!(
        "{}part",
        final_path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!("{value}."))
            .unwrap_or_default()
    ));

    emit_download_progress(
        &window,
        DownloadProgressPayload {
            download_id: request.download_id.clone(),
            status: "started",
            filename: Some(filename.clone()),
            downloaded_bytes: 0,
            total_bytes: None,
            percent: None,
            path: None,
            message: None,
        },
    );
    set_window_progress(&window, None, 0);

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "failed to prepare download client".to_string())?;

    let first_response = client
        .get(file_url.clone())
        .header(COOKIE, format!("auth_token={auth_token}"))
        .header(USER_AGENT, desktop_user_agent())
        .send()
        .await
        .map_err(|_| "failed to request file access URL".to_string())?;

    let response = if first_response.status().is_redirection() {
        let location = first_response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "download redirect missing location".to_string())?;
        let signed_url = file_url
            .join(location)
            .map_err(|_| "download redirect URL is invalid".to_string())?;
        if signed_url.scheme() != "https" {
            return Err("download redirect URL is not supported".to_string());
        }
        client
            .get(signed_url)
            .header(USER_AGENT, desktop_user_agent())
            .send()
            .await
            .map_err(|_| "failed to request signed file URL".to_string())?
    } else {
        first_response
    };

    if !response.status().is_success() {
        return Err("file download request failed".to_string());
    }

    let total_bytes = response.content_length();
    set_window_progress(&window, total_bytes, 0);

    if let Some(parent) = part_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|_| "failed to create download directory".to_string())?;
    }

    let mut file = tokio::fs::File::create(&part_path)
        .await
        .map_err(|_| "failed to create local download file".to_string())?;
    let mut stream = response.bytes_stream();
    let mut downloaded_bytes = 0_u64;
    let mut last_emit = Instant::now()
        .checked_sub(PROGRESS_EMIT_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_percent = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "failed while reading download stream".to_string())?;
        file.write_all(&chunk)
            .await
            .map_err(|_| "failed while writing local download file".to_string())?;
        downloaded_bytes += chunk.len() as u64;

        let percent = calculate_percent(downloaded_bytes, total_bytes);
        let should_emit = last_emit.elapsed() >= PROGRESS_EMIT_INTERVAL || percent != last_percent;
        if should_emit {
            emit_download_progress(
                &window,
                DownloadProgressPayload {
                    download_id: request.download_id.clone(),
                    status: "progress",
                    filename: Some(filename.clone()),
                    downloaded_bytes,
                    total_bytes,
                    percent,
                    path: None,
                    message: None,
                },
            );
            set_window_progress(&window, total_bytes, downloaded_bytes);
            last_emit = Instant::now();
            last_percent = percent;
        }
    }

    file.flush()
        .await
        .map_err(|_| "failed to flush local download file".to_string())?;
    drop(file);

    tokio::fs::rename(&part_path, &final_path)
        .await
        .map_err(|_| "failed to finish local download file".to_string())?;

    let path = final_path.to_string_lossy().to_string();
    emit_download_progress(
        &window,
        DownloadProgressPayload {
            download_id: request.download_id.clone(),
            status: "completed",
            filename: Some(filename),
            downloaded_bytes,
            total_bytes,
            percent: Some(100),
            path: Some(path.clone()),
            message: None,
        },
    );
    let _ = window.set_progress_bar(ProgressBarState {
        status: Some(ProgressBarStatus::None),
        progress: None,
    });

    Ok(DownloadSnackFileResult {
        download_id: request.download_id,
        path,
        downloaded_bytes,
    })
}

pub(crate) fn emit_failed_download(
    window: &WebviewWindow,
    download_id: String,
    filename: String,
    message: String,
) {
    emit_download_progress(
        window,
        DownloadProgressPayload {
            download_id,
            status: "failed",
            filename: Some(filename),
            downloaded_bytes: 0,
            total_bytes: None,
            percent: None,
            path: None,
            message: Some(message),
        },
    );
    let _ = window.set_progress_bar(ProgressBarState {
        status: Some(ProgressBarStatus::Error),
        progress: None,
    });
}

pub(crate) fn open_downloaded_path(
    app: &AppHandle,
    window: &WebviewWindow,
    raw_path: &str,
) -> Result<(), String> {
    let path = validate_downloaded_path(app, window, raw_path)?;
    open_path(&path)
}

pub(crate) fn reveal_downloaded_path(
    app: &AppHandle,
    window: &WebviewWindow,
    raw_path: &str,
) -> Result<(), String> {
    let path = validate_downloaded_path(app, window, raw_path)?;
    reveal_path(&path)
}

fn validate_download_id(download_id: &str) -> Result<(), String> {
    if download_id.trim().is_empty() || download_id.len() > 128 {
        return Err("download id is invalid".to_string());
    }
    Ok(())
}

fn resolve_snack_file_url(current_url: &Url, raw_url: &str) -> Result<Url, String> {
    let url = if raw_url.starts_with('/') {
        current_url
            .join(raw_url)
            .map_err(|_| "download URL is invalid".to_string())?
    } else {
        Url::parse(raw_url).map_err(|_| "download URL is invalid".to_string())?
    };

    if !is_allowed_web_origin(&url) {
        return Err("download URL origin is not allowed".to_string());
    }

    Ok(url)
}

fn ensure_supported_file_endpoint(url: &Url) -> Result<(), String> {
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();

    let supported = segments.len() == 5
        && segments[0] == "api"
        && (segments[1] == "snack" || segments[1] == "jxxq")
        && segments[2] == "files"
        && segments[3].chars().all(|ch| ch.is_ascii_digit())
        && segments[4] == "content";

    if supported {
        Ok(())
    } else {
        Err("download URL path is not supported".to_string())
    }
}

fn ensure_download_query(url: &mut Url) {
    let has_download = url.query_pairs().any(|(key, _)| key == "download");
    if !has_download {
        url.query_pairs_mut().append_pair("download", "1");
    }
}

fn read_auth_token_cookie(window: &WebviewWindow, url: Url) -> Result<String, String> {
    let cookies = window
        .cookies_for_url(url)
        .map_err(|_| "failed to read desktop session cookies".to_string())?;

    cookies
        .into_iter()
        .find(|cookie| cookie.name() == "auth_token")
        .map(|cookie| cookie.value().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "desktop session is not authenticated".to_string())
}

fn sanitize_filename(filename: &str) -> Result<String, String> {
    let trimmed = filename.trim();
    if trimmed.is_empty() {
        return Err("filename is required".to_string());
    }

    let sanitized = trimmed
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_string();

    if sanitized.is_empty() {
        Err("filename is invalid".to_string())
    } else {
        Ok(sanitized)
    }
}

fn resolve_destination_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve("", BaseDirectory::Download)
        .map_err(|_| "failed to resolve downloads directory".to_string())
}

fn validate_downloaded_path(
    app: &AppHandle,
    window: &WebviewWindow,
    raw_path: &str,
) -> Result<PathBuf, String> {
    let current_url = window
        .url()
        .map_err(|_| "failed to read current window URL".to_string())?;
    if !is_allowed_web_origin(&current_url) {
        return Err("origin is not allowed to open downloaded files".to_string());
    }

    let path = PathBuf::from(raw_path);
    if !path.is_absolute() {
        return Err("downloaded file path is invalid".to_string());
    }

    let canonical_path = path
        .canonicalize()
        .map_err(|_| "downloaded file does not exist".to_string())?;
    if !canonical_path.is_file() {
        return Err("downloaded path is not a file".to_string());
    }

    let downloads_dir = resolve_destination_dir(app)?;
    let canonical_downloads_dir = downloads_dir
        .canonicalize()
        .map_err(|_| "failed to resolve downloads directory".to_string())?;
    if !canonical_path.starts_with(canonical_downloads_dir) {
        return Err("downloaded file path is not allowed".to_string());
    }

    Ok(canonical_path)
}

fn resolve_available_path(destination_dir: &Path, filename: &str) -> PathBuf {
    let candidate = destination_dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("download");
    let extension = path.extension().and_then(|value| value.to_str());

    for index in 1..1000 {
        let filename = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = destination_dir.join(filename);
        if !candidate.exists() {
            return candidate;
        }
    }

    destination_dir.join(format!("{stem} ({})", uuid_fallback_suffix()))
}

fn uuid_fallback_suffix() -> String {
    format!("{:?}", Instant::now()).replace([' ', ':', '.'], "_")
}

fn calculate_percent(downloaded_bytes: u64, total_bytes: Option<u64>) -> Option<u8> {
    total_bytes
        .filter(|total| *total > 0)
        .map(|total| ((downloaded_bytes.saturating_mul(100) / total).min(100)) as u8)
}

fn emit_download_progress(window: &WebviewWindow, payload: DownloadProgressPayload) {
    let _ = window.emit(DOWNLOAD_PROGRESS_EVENT, payload);
}

fn set_window_progress(window: &WebviewWindow, total_bytes: Option<u64>, downloaded_bytes: u64) {
    let (status, progress) = match calculate_percent(downloaded_bytes, total_bytes) {
        Some(percent) => (Some(ProgressBarStatus::Normal), Some(u64::from(percent))),
        None => (Some(ProgressBarStatus::Indeterminate), None),
    };
    let _ = window.set_progress_bar(ProgressBarState { status, progress });
}
