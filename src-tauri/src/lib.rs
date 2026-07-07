#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::Arc;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
#[cfg(any(target_os = "macos", windows))]
use tauri::include_image;
use futures_util::StreamExt;
use reqwest::header::{COOKIE, LOCATION, USER_AGENT};
use tauri::path::BaseDirectory;
use tauri::window::{ProgressBarState, ProgressBarStatus};
use tauri::{
    AppHandle, Emitter, Manager, Url, UserAttentionType, WebviewWindow, WebviewWindowBuilder,
};
use tokio::io::AsyncWriteExt;

#[cfg(any(target_os = "macos", windows))]
const TRAY_ID: &str = "main-tray";
#[cfg(any(target_os = "macos", windows))]
const TRAY_MENU_SHOW_ID: &str = "show";
#[cfg(any(target_os = "macos", windows))]
const TRAY_MENU_QUIT_ID: &str = "quit";
#[cfg(target_os = "macos")]
const TRAY_DEFAULT_ICON: tauri::image::Image<'_> = include_image!("./icons/white.png");
#[cfg(windows)]
const TRAY_DEFAULT_ICON: tauri::image::Image<'_> = include_image!("./icons/32x32.png");
#[cfg(any(target_os = "macos", windows))]
const TRAY_ATTENTION_ICON: tauri::image::Image<'_> = include_image!("./icons/tray-attention.png");

const ALLOWED_WEB_ORIGINS: &[&str] = &[
    "https://snack.mechlabs.cn",
    "https://qasnack.mechlabs.cn",
    "http://localhost:3000",
    "http://127.0.0.1:3000",
];

const DOWNLOAD_PROGRESS_EVENT: &str = "snack-download-progress";
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadSnackFileRequest {
    download_id: String,
    url: String,
    filename: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadSnackFileResult {
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

#[cfg(windows)]
struct DesktopAttentionState {
    active: AtomicBool,
}

#[tauri::command]
fn exit_after_force_update_cancel(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to close the desktop shell".to_string());
    }

    app.exit(0);
    Ok(())
}

#[tauri::command]
fn set_desktop_attention(
    window: WebviewWindow,
    unread_count: u32,
    level: String,
) -> Result<(), String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to update desktop attention".to_string());
    }

    let badge_count = match unread_count {
        0 => None,
        1..=99 => Some(i64::from(unread_count)),
        _ => Some(99),
    };
    let _ = window.set_badge_count(badge_count);
    set_badge_label(&window, unread_count);
    update_tray_attention(&window.app_handle(), unread_count);

    if unread_count > 0 && should_request_window_attention() {
        let request_type = if level == "critical" || level == "mention" {
            UserAttentionType::Critical
        } else {
            UserAttentionType::Informational
        };
        let _ = window.request_user_attention(Some(request_type));
    } else {
        let _ = window.request_user_attention(None);
    }

    Ok(())
}

#[tauri::command]
async fn download_snack_file(
    app: AppHandle,
    window: WebviewWindow,
    request: DownloadSnackFileRequest,
) -> Result<DownloadSnackFileResult, String> {
    let download_id = request.download_id.clone();
    let filename = request.filename.clone();
    let result = download_snack_file_inner(app, window.clone(), request).await;
    if let Err(message) = &result {
        emit_download_progress(
            &window,
            DownloadProgressPayload {
                download_id,
                status: "failed",
                filename: Some(filename),
                downloaded_bytes: 0,
                total_bytes: None,
                percent: None,
                path: None,
                message: Some(message.clone()),
            },
        );
        let _ = window.set_progress_bar(ProgressBarState {
            status: Some(ProgressBarStatus::Error),
            progress: None,
        });
    }
    result
}

async fn download_snack_file_inner(
    app: AppHandle,
    window: WebviewWindow,
    request: DownloadSnackFileRequest,
) -> Result<DownloadSnackFileResult, String> {
    validate_download_id(&request.download_id)?;

    let current_url = window.url().map_err(|_| "failed to read current window URL".to_string())?;
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

#[cfg(target_os = "macos")]
fn should_request_window_attention() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn should_request_window_attention() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn set_badge_label(window: &WebviewWindow, unread_count: u32) {
    let badge_label = if unread_count == 0 {
        None
    } else if unread_count > 99 {
        Some("99+".to_string())
    } else {
        Some(unread_count.to_string())
    };
    let _ = window.set_badge_label(badge_label);
}

#[cfg(not(target_os = "macos"))]
fn set_badge_label(_window: &WebviewWindow, _unread_count: u32) {}

#[cfg(target_os = "macos")]
fn update_tray_attention(app: &AppHandle, unread_count: u32) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if unread_count > 0 {
            let _ = tray.set_icon(Some(TRAY_ATTENTION_ICON));
            let _ = tray.set_tooltip(Some("Snack - 有未读消息"));
        } else {
            let _ = tray.set_icon_with_as_template(Some(TRAY_DEFAULT_ICON), true);
            let _ = tray.set_tooltip(Some("Snack"));
        }
    }
}

#[cfg(windows)]
fn update_tray_attention(app: &AppHandle, unread_count: u32) {
    let active = unread_count > 0;
    if let Some(state) = app.try_state::<Arc<DesktopAttentionState>>() {
        state.active.store(active, Ordering::Relaxed);
    }
    if !active {
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            let _ = tray.set_icon(Some(TRAY_DEFAULT_ICON));
            let _ = tray.set_tooltip(Some("Snack"));
        }
    } else if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let label = format_unread_count(unread_count);
        let _ = tray.set_tooltip(Some(format!("Snack - {label} 条未读")));
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn update_tray_attention(_app: &AppHandle, _unread_count: u32) {}

#[cfg(windows)]
fn format_unread_count(unread_count: u32) -> String {
    if unread_count > 99 {
        "99+".to_string()
    } else {
        unread_count.to_string()
    }
}

fn is_allowed_web_origin(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };

    let origin = match url.port() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    };

    ALLOWED_WEB_ORIGINS.contains(&origin.as_str())
}

fn desktop_user_agent() -> String {
    format!(
        "{} SnackDesktop/{}/{}",
        env!("SNACK_DESKTOP_BASE_UA").trim(),
        env!("SNACK_DESKTOP_ARCH"),
        env!("SNACK_DESKTOP_VERSION")
    )
}

#[cfg(windows)]
fn setup_windows_tray(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let attention_state = Arc::new(DesktopAttentionState {
        active: AtomicBool::new(false),
    });
    app.manage(attention_state.clone());

    let show = MenuItem::with_id(app, TRAY_MENU_SHOW_ID, "显示 Snack", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TRAY_DEFAULT_ICON)
        .tooltip("Snack")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};

            let should_show = match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => true,
                _ => false,
            };

            if should_show {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    register_status_menu_events(app);

    let handle = app.handle().clone();
    std::thread::spawn(move || {
        let mut attention_frame = false;
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let active = attention_state.active.load(Ordering::Relaxed);
            let Some(tray) = handle.tray_by_id(TRAY_ID) else {
                continue;
            };
            if active {
                attention_frame = !attention_frame;
                let icon = if attention_frame {
                    TRAY_ATTENTION_ICON
                } else {
                    TRAY_DEFAULT_ICON
                };
                let _ = tray.set_icon(Some(icon));
            } else if attention_frame {
                attention_frame = false;
                let _ = tray.set_icon(Some(TRAY_DEFAULT_ICON));
            }
        }
    });

    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(target_os = "macos")]
fn should_show_window_on_reopen(has_visible_windows: bool) -> bool {
    !has_visible_windows
}

#[cfg(any(target_os = "macos", windows))]
fn register_status_menu_events(app: &mut tauri::App) {
    app.on_menu_event(|app, event| match event.id().as_ref() {
        TRAY_MENU_SHOW_ID => show_main_window(app),
        TRAY_MENU_QUIT_ID => app.exit(0),
        _ => {}
    });
}

#[cfg(any(target_os = "macos", windows))]
fn install_close_to_status_menu(window: &WebviewWindow) {
    let close_window = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = close_window.hide();
        }
    });
}

#[cfg(not(any(target_os = "macos", windows)))]
fn install_close_to_status_menu(_window: &WebviewWindow) {}

#[cfg(target_os = "macos")]
fn setup_macos_status_menu(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let show = MenuItem::with_id(app, TRAY_MENU_SHOW_ID, "显示 Snack", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TRAY_DEFAULT_ICON)
        .icon_as_template(true)
        .tooltip("Snack")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .build(app)?;

    register_status_menu_events(app);

    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            download_snack_file,
            exit_after_force_update_cancel,
            set_desktop_attention
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            setup_macos_status_menu(app)?;

            #[cfg(windows)]
            setup_windows_tray(app)?;

            let window_config = app
                .config()
                .app
                .windows
                .first()
                .expect("missing main window config");

            let user_agent = desktop_user_agent();

            let window = WebviewWindowBuilder::from_config(app, window_config)?
                .user_agent(&user_agent)
                .build()?;

            install_close_to_status_menu(&window);

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Snack desktop client")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = event
            {
                if should_show_window_on_reopen(has_visible_windows) {
                    show_main_window(app);
                }
            }
        });
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::should_show_window_on_reopen;

    #[test]
    fn reopens_hidden_main_window_from_dock() {
        assert!(should_show_window_on_reopen(false));
    }

    #[test]
    fn does_not_steal_focus_when_reopen_finds_visible_windows() {
        assert!(!should_show_window_on_reopen(true));
    }
}
