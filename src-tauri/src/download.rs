mod inline;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use futures_util::StreamExt;
pub(crate) use inline::clear_window as clear_inline_downloads;
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
const MAX_GENERATED_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DownloadSnackFileRequest {
    pub(crate) download_id: String,
    pub(crate) url: String,
    chunk: Option<inline::Chunk>,
    pub(crate) filename: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveGeneratedShareImageRequest {
    pub(crate) data_base64: String,
    pub(crate) download_id: String,
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

    if request.url == "inline:" {
        return inline::receive(app, window, current_url, request).await;
    }
    let mut file_url = download_url(&current_url, &request.url)?;
    // Only Snack content endpoints require the download flag; preserve arbitrary signed URLs.
    if file_url.origin() == current_url.origin()
        && ensure_supported_file_endpoint(&file_url).is_ok()
        && file_url.query().is_none()
    {
        ensure_download_query(&mut file_url);
    }
    let filename = sanitize_filename(&request.filename)?;
    let destination_dir = resolve_destination_dir(&app)?;
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

    let mut send_cookie = may_send_auth(true, &current_url, &file_url);
    let mut redirects = 0;
    let response = loop {
        let mut get = client
            .get(file_url.clone())
            .header(USER_AGENT, desktop_user_agent());
        if send_cookie {
            if let Ok(token) = read_auth_token_cookie(&window, file_url.clone()) {
                get = get.header(COOKIE, format!("auth_token={token}"));
            }
        }
        let response = get
            .send()
            .await
            .map_err(|err| format!("download failed: {}", err.without_url()))?;
        if !response.status().is_redirection() {
            break response;
        }
        redirects += 1;
        if redirects > 5 {
            return Err("too many download redirects".into());
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|h| h.to_str().ok())
            .ok_or("missing download redirect")?;
        let next = download_redirect(&file_url, location)?;
        send_cookie = may_send_auth(send_cookie, &current_url, &next);
        file_url = next;
    };
    if !response.status().is_success() {
        return Err(format!(
            "file download request failed with status {}",
            response.status().as_u16()
        ));
    }

    let total_bytes = response.content_length();
    set_window_progress(&window, total_bytes, 0);

    let (mut file, staging) = stage_download(&destination_dir).await?;
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

    let final_path = finish_download(staging, &destination_dir, &filename)?;
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

pub(crate) async fn save_generated_share_image_inner(
    app: AppHandle,
    window: WebviewWindow,
    request: SaveGeneratedShareImageRequest,
) -> Result<DownloadSnackFileResult, String> {
    validate_download_id(&request.download_id)?;
    validate_download_origin(&window)?;
    let filename = sanitize_png_filename(&request.filename)?;
    let bytes = decode_generated_png(&request.data_base64)?;
    save_download_bytes(app, window, request.download_id, filename, bytes).await
}

fn may_send_auth(previously_trusted: bool, owner: &Url, target: &Url) -> bool {
    previously_trusted && owner.origin() == target.origin()
}

fn download_redirect(from: &Url, location: &str) -> Result<Url, String> {
    let next = download_url(from, location)?;
    if from.scheme() == "https" && next.scheme() != "https" {
        return Err("insecure download redirect".into());
    }
    Ok(next)
}

fn download_url(base: &Url, raw: &str) -> Result<Url, String> {
    if raw.trim().is_empty() {
        return Err("empty download URL".into());
    }
    let target = base.join(raw).map_err(|_| "invalid download URL")?;
    if !matches!(target.scheme(), "http" | "https")
        || !target.username().is_empty()
        || target.password().is_some()
    {
        return Err("unsupported download URL".into());
    }
    Ok(target)
}

async fn save_download_bytes(
    app: AppHandle,
    window: WebviewWindow,
    download_id: String,
    filename: String,
    bytes: Vec<u8>,
) -> Result<DownloadSnackFileResult, String> {
    let total_bytes = bytes.len() as u64;
    let destination_dir = resolve_destination_dir(&app)?;
    emit_generated_image_started(&window, &download_id, &filename, total_bytes);
    let (mut file, staging) = stage_download(&destination_dir).await?;
    file.write_all(&bytes)
        .await
        .map_err(|_| "failed to write local download")?;
    file.flush()
        .await
        .map_err(|_| "failed to flush local download")?;
    drop(file);
    let final_path = finish_download(staging, &destination_dir, &filename)?;
    let path = final_path.to_string_lossy().to_string();
    emit_generated_image_completed(&window, &download_id, &filename, &path, total_bytes);
    Ok(DownloadSnackFileResult {
        download_id,
        path,
        downloaded_bytes: total_bytes,
    })
}

fn validate_download_origin(window: &WebviewWindow) -> Result<(), String> {
    let current_url = window
        .url()
        .map_err(|_| "failed to read current window URL".to_string())?;
    if is_allowed_web_origin(&current_url) {
        Ok(())
    } else {
        Err("origin is not allowed to save generated images".to_string())
    }
}

fn sanitize_png_filename(filename: &str) -> Result<String, String> {
    let filename = sanitize_filename(filename)?;
    let is_png = Path::new(&filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"));
    if is_png {
        Ok(filename)
    } else {
        Err("generated image filename must use the .png extension".to_string())
    }
}

fn decode_generated_png(data_base64: &str) -> Result<Vec<u8>, String> {
    if data_base64.len() > MAX_GENERATED_IMAGE_BYTES.saturating_mul(4) / 3 + 4 {
        return Err("generated image exceeds the size limit".to_string());
    }
    let bytes = BASE64_STANDARD
        .decode(data_base64)
        .map_err(|_| "generated image data is invalid".to_string())?;
    if bytes.len() > MAX_GENERATED_IMAGE_BYTES {
        return Err("generated image exceeds the size limit".to_string());
    }
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err("generated image is not a valid PNG".to_string());
    }
    Ok(bytes)
}

fn emit_generated_image_started(
    window: &WebviewWindow,
    download_id: &str,
    filename: &str,
    total_bytes: u64,
) {
    emit_download_progress(
        window,
        DownloadProgressPayload {
            download_id: download_id.to_string(),
            status: "started",
            filename: Some(filename.to_string()),
            downloaded_bytes: 0,
            total_bytes: Some(total_bytes),
            percent: Some(0),
            path: None,
            message: None,
        },
    );
    set_window_progress(window, Some(total_bytes), 0);
}

fn emit_generated_image_completed(
    window: &WebviewWindow,
    download_id: &str,
    filename: &str,
    path: &str,
    total_bytes: u64,
) {
    emit_download_progress(
        window,
        DownloadProgressPayload {
            download_id: download_id.to_string(),
            status: "completed",
            filename: Some(filename.to_string()),
            downloaded_bytes: total_bytes,
            total_bytes: Some(total_bytes),
            percent: Some(100),
            path: Some(path.to_string()),
            message: None,
        },
    );
    let _ = window.set_progress_bar(ProgressBarState {
        status: Some(ProgressBarStatus::None),
        progress: None,
    });
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

pub(crate) fn delete_downloaded_path(
    app: &AppHandle,
    window: &WebviewWindow,
    raw_path: &str,
) -> Result<(), String> {
    let path = validate_downloaded_path(app, window, raw_path)?;
    delete_file(&path)
}

fn delete_file(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|_| "failed to delete downloaded file".to_string())
}

fn validate_download_id(download_id: &str) -> Result<(), String> {
    if download_id.trim().is_empty() || download_id.len() > 128 {
        return Err("download id is invalid".to_string());
    }
    Ok(())
}

pub(crate) fn resolve_snack_file_url(current_url: &Url, raw_url: &str) -> Result<Url, String> {
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

pub(crate) fn ensure_supported_file_endpoint(url: &Url) -> Result<(), String> {
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();

    let direct_chat = segments.len() == 6
        && segments[0..3] == ["api", "snack", "direct-chats"]
        && !segments[3].is_empty()
        && segments[3].chars().all(|ch| ch.is_ascii_digit())
        && segments[4] == "attachments"
        && !segments[5].is_empty()
        && segments[5].chars().all(|ch| ch.is_ascii_digit());
    let supported = direct_chat
        || (segments.len() == 5
            && segments[0] == "api"
            && is_supported_file_namespace(&segments)
            && segments[3].chars().all(|ch| ch.is_ascii_digit())
            && !segments[3].is_empty()
            && segments[4] == "content");

    if supported {
        Ok(())
    } else {
        Err("download URL path is not supported".to_string())
    }
}

fn is_supported_file_namespace(segments: &[&str]) -> bool {
    ((segments[1] == "snack" || segments[1] == "jxxq") && segments[2] == "files")
        || (segments[1] == "task-hub" && segments[2] == "project-assets")
}

fn ensure_download_query(url: &mut Url) {
    let has_download = url.query_pairs().any(|(key, _)| key == "download");
    if !has_download {
        url.query_pairs_mut().append_pair("download", "1");
    }
}

pub(crate) fn read_auth_token_cookie(window: &WebviewWindow, url: Url) -> Result<String, String> {
    let cookies = window
        .cookies_for_url(url.clone())
        .map_err(|_| "failed to read desktop session cookies".to_string())?;

    find_auth_token(cookies)
        .or_else(|| read_loopback_auth_token_cookie(window, &url))
        .ok_or_else(|| "desktop session is not authenticated".to_string())
}

fn find_auth_token(cookies: Vec<tauri::webview::Cookie<'static>>) -> Option<String> {
    cookies
        .into_iter()
        .find(|cookie| cookie.name() == "auth_token")
        .map(|cookie| cookie.value().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(debug_assertions)]
fn read_loopback_auth_token_cookie(window: &WebviewWindow, url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let ip = host.parse::<std::net::IpAddr>().ok()?;
    if !ip.is_loopback() {
        return None;
    }
    let cookies = window.cookies().ok()?;
    find_auth_token(
        cookies
            .into_iter()
            .filter(|cookie| cookie_domain_matches(cookie.domain(), host))
            .collect(),
    )
}

#[cfg(not(debug_assertions))]
fn read_loopback_auth_token_cookie(_window: &WebviewWindow, _url: &Url) -> Option<String> {
    None
}

fn cookie_domain_matches(cookie_domain: Option<&str>, host: &str) -> bool {
    cookie_domain
        .map(|domain| domain.trim_start_matches('.') == host)
        .unwrap_or(false)
}

pub(crate) fn sanitize_filename(filename: &str) -> Result<String, String> {
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

// Unique staging files clean themselves up on failure. Publish atomically without replacing
// an existing file, including when two requests with the same filename finish together.
async fn stage_download(dir: &Path) -> Result<(tokio::fs::File, tempfile::TempPath), String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|_| "failed to create downloads directory")?;
    let (file, staging) = tempfile::NamedTempFile::new_in(dir)
        .map_err(|_| "failed to stage download")?
        .into_parts();
    Ok((tokio::fs::File::from_std(file), staging))
}

fn finish_download(
    mut staging: tempfile::TempPath,
    dir: &Path,
    filename: &str,
) -> Result<PathBuf, String> {
    for _ in 0..32 {
        let target = resolve_available_path(dir, filename);
        match staging.persist_noclobber(&target) {
            Ok(()) => return Ok(target),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                staging = error.path
            }
            Err(_) => return Err("failed to finish local download".into()),
        }
    }
    Err("unable to allocate a unique download filename".into())
}

fn resolve_available_path(destination_dir: &Path, filename: &str) -> PathBuf {
    let candidate = destination_dir.join(filename);
    if candidate.symlink_metadata().is_err() {
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
        if candidate.symlink_metadata().is_err() {
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

#[cfg(test)]
mod tests {
    use super::{
        decode_generated_png, ensure_supported_file_endpoint, sanitize_png_filename, PNG_SIGNATURE,
    };
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
    use tauri::Url;

    #[test]
    fn download_link_policy_preserves_signatures_and_drops_cross_origin_auth() {
        let owner = Url::parse("https://snack.mechlabs.cn/sessions/12").unwrap();
        let local =
            super::download_url(&owner, "/api/snack/direct-chats/12/attachments/7").unwrap();
        assert!(super::may_send_auth(true, &owner, &local));
        let signed = "https://cdn.example/image?sig=a%2Fb";
        let external = super::download_redirect(&local, signed).unwrap();
        assert_eq!(external.as_str(), signed);
        assert!(!super::may_send_auth(true, &owner, &external));
        assert!(!super::may_send_auth(false, &owner, &local));
        for bad in [
            "http://cdn.example/downgrade",
            "file:///tmp/x",
            "https://user:secret@cdn.example/x",
        ] {
            assert!(super::download_redirect(&owner, bad).is_err());
        }
    }

    #[test]
    fn identically_named_downloads_do_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        for content in ["one", "two"] {
            let stage = tempfile::NamedTempFile::new_in(dir.path())
                .unwrap()
                .into_temp_path();
            std::fs::write(&stage, content).unwrap();
            super::finish_download(stage, dir.path(), "document.pdf").unwrap();
        }
        assert_eq!(
            std::fs::read(dir.path().join("document.pdf")).unwrap(),
            b"one"
        );
        assert_eq!(
            std::fs::read(dir.path().join("document (1).pdf")).unwrap(),
            b"two"
        );
    }

    #[test]
    fn accepts_generated_png_data() {
        let encoded = BASE64_STANDARD.encode([PNG_SIGNATURE.as_slice(), b"content"].concat());
        assert!(decode_generated_png(&encoded).is_ok());
    }

    #[test]
    fn rejects_non_png_data() {
        let encoded = BASE64_STANDARD.encode(b"not a png");
        assert_eq!(
            decode_generated_png(&encoded).unwrap_err(),
            "generated image is not a valid PNG"
        );
    }

    #[test]
    fn requires_png_filename_after_sanitizing() {
        assert_eq!(sanitize_png_filename("share.PNG").unwrap(), "share.PNG");
        assert!(sanitize_png_filename("share.jpg").is_err());
    }

    #[test]
    fn accepts_project_asset_content_endpoint() {
        let url =
            Url::parse("https://snack.test/api/task-hub/project-assets/3001/content?download")
                .unwrap();

        assert!(ensure_supported_file_endpoint(&url).is_ok());
    }

    #[test]
    fn accepts_direct_chat_content_without_losing_the_session_context() {
        let url =
            Url::parse("https://snack.test/api/snack/direct-chats/7/attachments/42?context=keep")
                .unwrap();
        assert!(ensure_supported_file_endpoint(&url).is_ok());
        for path in [
            "/api/snack/direct-chats/not-a-session/attachments/42",
            "/api/snack/direct-chats/7/attachments/not-a-file",
        ] {
            assert!(ensure_supported_file_endpoint(
                &Url::parse(&format!("https://snack.test{path}")).unwrap()
            )
            .is_err());
        }
    }

    #[test]
    fn rejects_non_numeric_project_asset_endpoint() {
        let url =
            Url::parse("https://snack.test/api/task-hub/project-assets/asset-1/content").unwrap();

        assert_eq!(
            ensure_supported_file_endpoint(&url).unwrap_err(),
            "download URL path is not supported"
        );
    }

    #[test]
    fn matches_only_the_same_cookie_domain_for_loopback_fallback() {
        assert!(super::cookie_domain_matches(Some("127.0.0.1"), "127.0.0.1"));
        assert!(super::cookie_domain_matches(
            Some(".127.0.0.1"),
            "127.0.0.1"
        ));
        assert!(!super::cookie_domain_matches(
            Some("localhost"),
            "127.0.0.1"
        ));
        assert!(!super::cookie_domain_matches(None, "127.0.0.1"));
    }

    #[test]
    fn deletes_a_downloaded_file() {
        let directory =
            std::env::temp_dir().join(format!("snack-download-delete-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("report.pdf");
        std::fs::write(&path, b"download").unwrap();

        assert!(super::delete_file(&path).is_ok());
        assert!(!path.exists());
        std::fs::remove_dir(directory).unwrap();
    }
}
