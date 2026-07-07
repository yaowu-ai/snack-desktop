use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, Url, WebviewWindow};

use crate::platform::open_external_url;
use crate::web::is_allowed_web_origin;

pub(crate) fn handle_new_window_request(
    app: &AppHandle,
    url: Url,
) -> NewWindowResponse<tauri::Wry> {
    if is_allowed_web_origin(&url) {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.navigate(url);
        }
    } else if is_supported_external_web_url(&url) {
        let _ = open_external_url(&url);
    }

    NewWindowResponse::Deny
}

fn is_supported_external_web_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

pub(crate) fn navigate_back(window: &WebviewWindow) {
    let _ = window.eval("window.history.back();");
}
