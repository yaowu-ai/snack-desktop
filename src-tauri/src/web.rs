use tauri::Url;

pub(crate) const ALLOWED_WEB_ORIGINS: &[&str] = &[
    "https://snack.mechlabs.cn",
    "https://snack.globalnexus-co.com",
    "https://snack.mechandlink.com",
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

    ALLOWED_WEB_ORIGINS.contains(&origin.as_str()) || is_allowed_development_origin(url)
}

#[cfg(debug_assertions)]
fn is_allowed_development_origin(url: &Url) -> bool {
    url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
}

#[cfg(not(debug_assertions))]
fn is_allowed_development_origin(_url: &Url) -> bool {
    false
}

pub(crate) fn desktop_user_agent() -> String {
    format!(
        "{} SnackDesktop/{}/{}",
        env!("SNACK_DESKTOP_BASE_UA").trim(),
        env!("SNACK_DESKTOP_ARCH"),
        env!("SNACK_DESKTOP_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_only_configured_snack_origins() {
        for origin in [
            "https://snack.mechlabs.cn",
            "https://snack.globalnexus-co.com",
            "https://snack.mechandlink.com",
            "https://qasnack.mechlabs.cn",
            "http://localhost:3000",
        ] {
            assert!(is_allowed_web_origin(&Url::parse(origin).unwrap()));
        }
        assert!(!is_allowed_web_origin(
            &Url::parse("https://snack.mechandlink.com.evil.example").unwrap()
        ));
    }

    #[test]
    fn allows_loopback_ports_in_debug_builds() {
        assert!(is_allowed_web_origin(
            &Url::parse("http://127.0.0.1:3003/task-hub").unwrap()
        ));
        assert!(is_allowed_web_origin(
            &Url::parse("http://localhost:4317/task-hub").unwrap()
        ));
        assert!(!is_allowed_web_origin(
            &Url::parse("http://192.168.1.10:3003/task-hub").unwrap()
        ));
    }
}
