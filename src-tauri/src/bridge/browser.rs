use serde::Deserialize;
use tauri::Url;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserOpenRequest {
    pub(super) url: String,
}

/// Validate web schemes without reflecting a sensitive launch URL in an error.
pub(super) fn validate_browser_url(value: &str) -> Result<Url, String> {
    if value.len() > 8192 || value.chars().any(char::is_control) {
        return Err("invalid browser URL".into());
    }
    let url = Url::parse(value).map_err(|_| "invalid browser URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("invalid browser URL".into());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_internal_and_external_web_urls_without_losing_parameters() {
        for value in [
            "https://snack.mechlabs.cn/api/sso/browser-launch?code=abc",
            "http://localhost:3000/api/sso/browser-launch?code=abc",
            "https://example.com/reports?page=2#details",
        ] {
            assert_eq!(validate_browser_url(value).unwrap().as_str(), value);
        }
    }

    #[test]
    fn rejects_non_web_schemes_credentials_and_control_characters() {
        for value in [
            "javascript:alert(1)",
            "file:///tmp/test",
            "snack://meeting",
            "https://user:secret@example.com",
            "https://example.com/\nsecret",
            "//example.com",
        ] {
            assert_eq!(
                validate_browser_url(value).unwrap_err(),
                "invalid browser URL"
            );
        }
    }
}
