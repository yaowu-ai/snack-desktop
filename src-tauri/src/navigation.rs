use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, Url, WebviewWindow};

use crate::platform::open_external_url;
use crate::web::is_allowed_web_origin;

pub(crate) fn handle_new_window_request(
    app: &AppHandle,
    url: Url,
    features: tauri::webview::NewWindowFeatures,
) -> NewWindowResponse<tauri::Wry> {
    if let Some(id) = url
        .as_str()
        .strip_prefix("about:blank#snack-image-preview-")
    {
        let Some(label) = preview_label(id) else {
            return NewWindowResponse::Deny;
        };
        let target = url.clone();
        // Only an empty, opener-related view. Never grant it native command permissions.
        let trusted = app
            .get_webview_window("main")
            .and_then(|w| w.url().ok())
            .is_some_and(|u| is_allowed_web_origin(&u));
        if !trusted {
            return NewWindowResponse::Deny;
        }
        return tauri::WebviewWindowBuilder::new(app, label, tauri::WebviewUrl::External(url))
            .window_features(features)
            .title("Snack")
            .inner_size(960., 720.)
            .center()
            .on_navigation(move |u| u.as_str() == "about:blank" || u == &target)
            .on_new_window(|_, _| NewWindowResponse::Deny)
            .build()
            .map(|window| NewWindowResponse::Create { window })
            .unwrap_or(NewWindowResponse::Deny);
    }
    if is_allowed_web_origin(&url) {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.navigate(url);
        }
    } else if is_supported_external_web_url(&url) {
        let _ = open_external_url(&url);
    }

    NewWindowResponse::Deny
}

/// A reload or site change invalidates the opener, even if JS teardown didn't run.
pub(crate) fn close_image_previews(app: &AppHandle) {
    for (label, window) in app.webview_windows() {
        if label.starts_with("chat_image_") {
            let _ = window.destroy();
        }
    }
}

fn preview_label(id: &str) -> Option<String> {
    (id.len() == 36 && id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'))
        .then(|| format!("chat_image_{id}"))
}

#[tauri::command]
pub(crate) async fn close_image_preview_window(
    app: AppHandle,
    window: WebviewWindow,
    id: String,
) -> Result<(), String> {
    if window.label() != "main" || !window.url().is_ok_and(|url| is_allowed_web_origin(&url)) {
        return Err("untrusted preview owner".into());
    }
    let label = preview_label(&id).ok_or("invalid preview id")?;
    if let Some(child) = app.get_webview_window(&label) {
        child.destroy().map_err(|_| "unable to close preview")?;
    }
    Ok(())
}

fn is_supported_external_web_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

pub(crate) fn navigate_back(window: &WebviewWindow) {
    let _ = window.eval("window.history.back();");
}
