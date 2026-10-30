use futures_util::StreamExt;
use reqwest::header::{COOKIE, LOCATION};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, WebviewWindow};
use tokio::io::AsyncWriteExt;

use crate::download::{
    ensure_supported_file_endpoint, read_auth_token_cookie, resolve_snack_file_url,
    sanitize_filename,
};

const MAX_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreviewRequest {
    url: String,
    filename: String,
    mime_type: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PreviewResult {
    status: &'static str,
}

/// Authenticate using the source WebView, then hand a private local file to the system viewer.
pub(crate) async fn preview(
    app: AppHandle,
    window: WebviewWindow,
    request: PreviewRequest,
) -> Result<PreviewResult, String> {
    let extension = preview_extension(&request)?;
    let current_url = window.url().map_err(|_| "PREVIEW_WINDOW_UNAVAILABLE")?;
    let url = resolve_snack_file_url(&current_url, &request.url)?;
    if url.origin() != current_url.origin() {
        return Err("INVALID_PREVIEW_ORIGIN".into());
    }
    ensure_supported_file_endpoint(&url)?;
    let token = read_auth_token_cookie(&window, url.clone())?;
    let folder = create_preview_folder(&app)?;
    let name = sanitize_filename(&request.filename)?;
    let stem: String = Path::new(&name)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("image")
        .chars()
        .take(60)
        .collect();
    let file = folder.join(format!("{stem}.{extension}"));
    let result = download(&file, url, &token).await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_dir_all(&folder).await;
        return Err(error);
    }
    let same_session = window.url().ok().is_some_and(|url| {
        url.origin() == current_url.origin()
            && read_auth_token_cookie(&window, url).ok().as_deref() == Some(token.as_str())
    });
    if !same_session {
        let _ = tokio::fs::remove_dir_all(&folder).await;
        return Err("PREVIEW_SESSION_CHANGED".into());
    }
    if let Err(error) = open_viewer(app.clone(), file.clone()).await {
        let _ = tokio::fs::remove_dir_all(&folder).await;
        return Err(error);
    }
    watch_session(app, window, token, file);
    Ok(PreviewResult { status: "opened" })
}

/// Revoke a transient native preview after logout or a site/account switch.
fn watch_session(app: AppHandle, window: WebviewWindow, token: String, file: PathBuf) {
    let original_origin = window.url().ok().map(|url| url.origin());
    tauri::async_runtime::spawn(async move {
        while file.exists() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let current = window.url().ok();
            let authenticated = current.as_ref().is_some_and(|url| {
                Some(url.origin()) == original_origin
                    && read_auth_token_cookie(&window, url.clone()).ok().as_deref()
                        == Some(token.as_str())
            });
            if authenticated {
                continue;
            }
            close_viewer(&app, &file);
            if let Some(folder) = file.parent() {
                let _ = tokio::fs::remove_dir_all(folder).await;
            }
            break;
        }
    });
}

#[cfg(target_os = "macos")]
fn close_viewer(app: &AppHandle, path: &Path) {
    use std::ffi::CString;
    extern "C" {
        fn snack_close_file_preview(path: *const std::ffi::c_char);
    }
    if let Ok(path) = CString::new(path.to_string_lossy().as_bytes()) {
        let _ = app.run_on_main_thread(move || unsafe {
            snack_close_file_preview(path.as_ptr());
        });
    }
}

#[cfg(not(target_os = "macos"))]
fn close_viewer(_app: &AppHandle, _path: &Path) {}

fn preview_extension(request: &PreviewRequest) -> Result<&'static str, String> {
    let extension = Path::new(request.filename.trim())
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = request
        .mime_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if extension == "heif" || mime == "image/heif" {
        return Ok("heif");
    }
    if extension == "heic" || mime == "image/heic" {
        return Ok("heic");
    }
    Err("UNSUPPORTED_PREVIEW_TYPE".into())
}

fn create_preview_folder(app: &AppHandle) -> Result<PathBuf, String> {
    let root = app
        .path()
        .app_cache_dir()
        .map_err(|_| "PREVIEW_CACHE_UNAVAILABLE")?
        .join("file-previews");
    std::fs::create_dir_all(&root).map_err(|_| "PREVIEW_CACHE_UNAVAILABLE")?;
    // Old, inactive viewers from a previous process must not leave permanent photo copies.
    for entry in std::fs::read_dir(&root)
        .map_err(|_| "PREVIEW_CACHE_UNAVAILABLE")?
        .take(100)
        .flatten()
    {
        if entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(86400))
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "PREVIEW_CLOCK_ERROR")?
        .as_nanos();
    let folder = root.join(format!("{}-{id}", std::process::id()));
    std::fs::create_dir(&folder).map_err(|_| "PREVIEW_CACHE_UNAVAILABLE")?;
    Ok(folder)
}

async fn download(path: &Path, url: tauri::Url, token: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| "PREVIEW_DOWNLOAD_FAILED")?;
    let first = client
        .get(url.clone())
        .header(COOKIE, format!("auth_token={token}"))
        .send()
        .await
        .map_err(|_| "PREVIEW_DOWNLOAD_FAILED")?;
    let response = if first.status().is_redirection() {
        let location = first
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("PREVIEW_REDIRECT_INVALID")?;
        let signed = url.join(location).map_err(|_| "PREVIEW_REDIRECT_INVALID")?;
        if signed.scheme() != "https"
            || !signed.username().is_empty()
            || signed.password().is_some()
        {
            return Err("PREVIEW_REDIRECT_INVALID".into());
        }
        // Never forward session cookies to the object storage host.
        client
            .get(signed)
            .send()
            .await
            .map_err(|_| "PREVIEW_DOWNLOAD_FAILED")?
    } else {
        first
    };
    if response.status() != reqwest::StatusCode::OK {
        return Err("PREVIEW_DOWNLOAD_FAILED".into());
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_BYTES)
    {
        return Err("PREVIEW_TOO_LARGE".into());
    }
    write_response(path, response).await
}

async fn write_response(path: &Path, response: reqwest::Response) -> Result<(), String> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|_| "PREVIEW_CACHE_UNAVAILABLE")?;
    let mut stream = response.bytes_stream();
    let mut size = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "PREVIEW_DOWNLOAD_FAILED")?;
        size += chunk.len() as u64;
        if size > MAX_BYTES {
            return Err("PREVIEW_TOO_LARGE".into());
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| "PREVIEW_CACHE_UNAVAILABLE")?;
    }
    if size == 0 {
        return Err("PREVIEW_EMPTY_FILE".into());
    }
    file.flush()
        .await
        .map_err(|_| "PREVIEW_CACHE_UNAVAILABLE".into())
}

#[cfg(target_os = "macos")]
async fn open_viewer(app: AppHandle, path: PathBuf) -> Result<(), String> {
    use std::ffi::CString;
    extern "C" {
        fn snack_preview_file(path: *const std::ffi::c_char) -> bool;
    }
    let path =
        CString::new(path.to_string_lossy().as_bytes()).map_err(|_| "PREVIEW_PATH_INVALID")?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = sender.send(unsafe { snack_preview_file(path.as_ptr()) });
    })
    .map_err(|_| "PREVIEW_WINDOW_UNAVAILABLE")?;
    if receiver.await.unwrap_or(false) {
        Ok(())
    } else {
        Err("PREVIEW_UNAVAILABLE".into())
    }
}

#[cfg(not(target_os = "macos"))]
async fn open_viewer(_app: AppHandle, path: PathBuf) -> Result<(), String> {
    // Other platforms delegate decoding to their installed native file viewer.
    crate::platform::open_path(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_mime_aliases_and_case_insensitive_extensions_only_for_heic_heif() {
        for (filename, mime, expected) in [
            ("PHOTO.HEIC", None, "heic"),
            ("photo", Some("image/heif; codecs=hevc"), "heif"),
        ] {
            let request = PreviewRequest {
                url: String::new(),
                filename: filename.into(),
                mime_type: mime.map(str::to_string),
            };
            assert_eq!(preview_extension(&request).unwrap(), expected);
        }
        let request = PreviewRequest {
            url: String::new(),
            filename: "file.exe".into(),
            mime_type: None,
        };
        assert!(preview_extension(&request).is_err());
    }
}
