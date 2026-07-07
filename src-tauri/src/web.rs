use tauri::Url;

const ALLOWED_WEB_ORIGINS: &[&str] = &[
    "https://snack.mechlabs.cn",
    "https://qasnack.mechlabs.cn",
    "http://localhost:3000",
    "http://127.0.0.1:3000",
];

pub(crate) fn is_allowed_web_origin(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };

    let origin = match url.port() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    };

    ALLOWED_WEB_ORIGINS.contains(&origin.as_str())
}

pub(crate) fn desktop_user_agent() -> String {
    format!(
        "{} SnackDesktop/{}/{}",
        env!("SNACK_DESKTOP_BASE_UA").trim(),
        env!("SNACK_DESKTOP_ARCH"),
        env!("SNACK_DESKTOP_VERSION")
    )
}
