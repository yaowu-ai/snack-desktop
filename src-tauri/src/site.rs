use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Url, WebviewUrl};

pub(crate) const SITE_MENU_ID: &str = "site";
pub(crate) const YAOWU_SITE_MENU_ID: &str = "site-yaowu";
pub(crate) const JIFUWU_SITE_MENU_ID: &str = "site-jifuwu";
pub(crate) const MECHLINK_SITE_MENU_ID: &str = "site-mechlink";

const SITE_PREFERENCE_FILE: &str = "site-preference.json";
const YAOWU_URL: &str = "https://snack.mechlabs.cn";
const JIFUWU_URL: &str = "https://snack.globalnexus-co.com";
const MECHLINK_URL: &str = "https://snack.mechandlink.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SiteKey {
    Yaowu,
    Jifuwu,
    Mechlink,
}

impl Default for SiteKey {
    fn default() -> Self {
        Self::Yaowu
    }
}

impl SiteKey {
    pub(crate) const ALL: [Self; 3] = [Self::Yaowu, Self::Jifuwu, Self::Mechlink];

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Yaowu => "要务",
            Self::Jifuwu => "即服务",
            Self::Mechlink => "Mechlink",
        }
    }

    pub(crate) fn window_title(self) -> &'static str {
        match self {
            Self::Yaowu => "Snack",
            Self::Jifuwu => "Snack - 即服务",
            Self::Mechlink => "Snack - Mechlink",
        }
    }

    pub(crate) fn menu_id(self) -> &'static str {
        match self {
            Self::Yaowu => YAOWU_SITE_MENU_ID,
            Self::Jifuwu => JIFUWU_SITE_MENU_ID,
            Self::Mechlink => MECHLINK_SITE_MENU_ID,
        }
    }

    pub(crate) fn url(self) -> Url {
        Url::parse(match self {
            Self::Yaowu => YAOWU_URL,
            Self::Jifuwu => JIFUWU_URL,
            Self::Mechlink => MECHLINK_URL,
        })
        .expect("configured Snack site URL must be valid")
    }

    pub(crate) fn desktop_login_url(self) -> Url {
        self.url()
            .join("/login/desktop")
            .expect("configured Snack desktop login URL must be valid")
    }

    pub(crate) fn from_menu_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|site| site.menu_id() == id)
    }

    pub(crate) fn from_url(url: &Url) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|site| same_origin(url, &site.url()))
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SitePreference {
    pub(crate) site: SiteKey,
    #[serde(default)]
    pub(crate) login_required: bool,
}

impl SitePreference {
    pub(crate) fn pending_login(site: SiteKey) -> Self {
        Self {
            site,
            login_required: true,
        }
    }

    pub(crate) fn after_load(site: SiteKey, path: &str) -> Self {
        Self {
            site,
            login_required: is_login_path(path),
        }
    }
}

pub(crate) fn initial_webview_url(app: &AppHandle, configured: &WebviewUrl) -> WebviewUrl {
    resolve_initial_webview_url(configured, load_site_preference(app))
}

pub(crate) fn selected_site(app: &AppHandle, configured: &WebviewUrl) -> SiteKey {
    let resolved = initial_webview_url(app, configured);
    site_from_webview_url(&resolved).unwrap_or_else(|| load_site_preference(app).site)
}

pub(crate) fn site_from_webview_url(url: &WebviewUrl) -> Option<SiteKey> {
    external_url(url).and_then(SiteKey::from_url)
}

fn resolve_initial_webview_url(configured: &WebviewUrl, saved: SitePreference) -> WebviewUrl {
    let Some(configured_url) = external_url(configured) else {
        return configured.clone();
    };
    if SiteKey::from_url(configured_url) != Some(SiteKey::Yaowu) {
        return configured.clone();
    }
    let saved_url = if saved.login_required {
        saved.site.desktop_login_url()
    } else {
        saved.site.url()
    };
    WebviewUrl::External(saved_url)
}

pub(crate) fn load_site(app: &AppHandle) -> SiteKey {
    load_site_preference(app).site
}

pub(crate) fn load_site_preference(app: &AppHandle) -> SitePreference {
    site_preference_path(app)
        .ok()
        .and_then(|path| read_site_preference(&path))
        .unwrap_or_default()
}

pub(crate) fn save_site_preference(
    app: &AppHandle,
    preference: SitePreference,
) -> Result<(), String> {
    let path = site_preference_path(app)?;
    let parent = path.parent().ok_or("站点配置路径无效")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    persist_site_preference(&path, preference)
}

fn external_url(configured: &WebviewUrl) -> Option<&Url> {
    match configured {
        WebviewUrl::External(url) => Some(url),
        _ => None,
    }
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn is_login_path(path: &str) -> bool {
    path == "/login" || path.starts_with("/login/")
}

fn site_preference_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|directory| directory.join(SITE_PREFERENCE_FILE))
        .map_err(|error| error.to_string())
}

fn read_site_preference(path: &Path) -> Option<SitePreference> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice::<SitePreference>(&bytes).ok()
}

fn persist_site_preference(path: &Path, preference: SitePreference) -> Result<(), String> {
    let temporary_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(&preference).map_err(|error| error.to_string())?;
    fs::write(&temporary_path, bytes).map_err(|error| error.to_string())?;
    fs::rename(temporary_path, path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("snack-desktop-site-{name}-{}", std::process::id()))
    }

    #[test]
    fn defaults_to_yaowu_for_missing_or_invalid_preference() {
        let missing = temporary_file("missing.json");
        let _ = fs::remove_file(&missing);
        assert_eq!(
            read_site_preference(&missing).unwrap_or_default(),
            SitePreference::default()
        );

        let invalid = temporary_file("invalid.json");
        fs::write(&invalid, br#"{"site":"unknown"}"#).unwrap();
        assert_eq!(
            read_site_preference(&invalid).unwrap_or_default(),
            SitePreference::default()
        );
        let _ = fs::remove_file(invalid);
    }

    #[test]
    fn persists_and_restores_pending_login_state() {
        let path = temporary_file("saved.json");
        let preference = SitePreference::pending_login(SiteKey::Mechlink);
        persist_site_preference(&path, preference).unwrap();
        assert_eq!(read_site_preference(&path), Some(preference));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reads_legacy_site_only_preference_as_settled() {
        let path = temporary_file("legacy.json");
        fs::write(&path, br#"{"site":"jifuwu"}"#).unwrap();
        assert_eq!(
            read_site_preference(&path),
            Some(SitePreference {
                site: SiteKey::Jifuwu,
                login_required: false,
            })
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn maps_fixed_menu_ids_and_origins() {
        assert_eq!(
            SiteKey::from_menu_id(JIFUWU_SITE_MENU_ID),
            Some(SiteKey::Jifuwu)
        );
        assert_eq!(
            SiteKey::from_url(&Url::parse("https://snack.mechandlink.com/login").unwrap()),
            Some(SiteKey::Mechlink)
        );
        assert_eq!(SiteKey::from_menu_id("site-custom"), None);
    }

    #[test]
    fn maps_sites_to_window_titles() {
        assert_eq!(SiteKey::Yaowu.window_title(), "Snack");
        assert_eq!(SiteKey::Jifuwu.window_title(), "Snack - 即服务");
        assert_eq!(SiteKey::Mechlink.window_title(), "Snack - Mechlink");
    }

    #[test]
    fn maps_sites_to_desktop_login_urls() {
        assert_eq!(
            SiteKey::Yaowu.desktop_login_url().as_str(),
            "https://snack.mechlabs.cn/login/desktop"
        );
        assert_eq!(
            SiteKey::Jifuwu.desktop_login_url().as_str(),
            "https://snack.globalnexus-co.com/login/desktop"
        );
        assert_eq!(
            SiteKey::Mechlink.desktop_login_url().as_str(),
            "https://snack.mechandlink.com/login/desktop"
        );
    }

    #[test]
    fn restores_saved_site_only_for_the_universal_production_build() {
        let production = WebviewUrl::External(SiteKey::Yaowu.url());
        let settled = SitePreference {
            site: SiteKey::Mechlink,
            login_required: false,
        };
        assert_eq!(
            resolve_initial_webview_url(&production, settled),
            WebviewUrl::External(SiteKey::Mechlink.url())
        );

        let pending = SitePreference::pending_login(SiteKey::Jifuwu);
        assert_eq!(
            resolve_initial_webview_url(&production, pending),
            WebviewUrl::External(SiteKey::Jifuwu.desktop_login_url())
        );

        let local = WebviewUrl::External(Url::parse("http://localhost:3000").unwrap());
        assert_eq!(resolve_initial_webview_url(&local, pending), local);

        let qa = WebviewUrl::External(Url::parse("https://qasnack.mechlabs.cn").unwrap());
        assert_eq!(resolve_initial_webview_url(&qa, pending), qa);
    }

    #[test]
    fn keeps_login_pending_until_a_non_login_page_finishes() {
        assert!(SitePreference::after_load(SiteKey::Jifuwu, "/login").login_required);
        assert!(SitePreference::after_load(SiteKey::Jifuwu, "/login/desktop").login_required);
        assert!(!SitePreference::after_load(SiteKey::Jifuwu, "/").login_required);
    }
}
