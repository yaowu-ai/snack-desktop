use std::{
    collections::HashMap,
    fs,
    sync::{Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Locale {
    Zh,
    En,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocaleSnapshot {
    pub(crate) locale: Locale,
    preference: Option<Locale>,
    pub(crate) default_locale: Locale,
    system_locale: Option<String>,
}

struct LocaleState(Mutex<Option<Locale>>);

pub(crate) fn normalize(value: &str) -> Option<Locale> {
    let value = value.trim().to_lowercase().replace('_', "-");
    if value == "en" || value.starts_with("en-") {
        return Some(Locale::En);
    }
    match value.as_str() {
        "zh" | "zh-cn" | "zh-sg" | "zh-hans" | "zh-hans-cn" | "zh-hans-sg" => Some(Locale::Zh),
        _ => None,
    }
}

fn default_locale() -> Locale {
    normalize(env!("NEXT_PUBLIC_DEFAULT_LOCALE")).expect("validated deployment locale")
}

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    let preference = fs::read(directory.join("locale-preference.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Option<Locale>>(&bytes).ok())
        .flatten();
    app.manage(LocaleState(Mutex::new(preference)));
    Ok(())
}

pub(crate) fn snapshot(app: &AppHandle) -> LocaleSnapshot {
    let preference = app
        .try_state::<LocaleState>()
        .and_then(|state| state.0.lock().ok().and_then(|value| *value));
    let system_locale = sys_locale::get_locale();
    let default_locale = default_locale();
    let locale = preference
        .or_else(|| system_locale.as_deref().and_then(normalize))
        .unwrap_or(default_locale);
    LocaleSnapshot {
        locale,
        preference,
        default_locale,
        system_locale,
    }
}

pub(crate) fn set_preference(
    app: &AppHandle,
    preference: Option<Locale>,
) -> Result<LocaleSnapshot, String> {
    let state = app.state::<LocaleState>();
    let mut stored = state.0.lock().map_err(|error| error.to_string())?;
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let temporary = directory.join("locale-preference.pending");
    fs::write(
        &temporary,
        serde_json::to_vec(&preference).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, directory.join("locale-preference.json"))
        .map_err(|error| error.to_string())?;
    *stored = preference;
    drop(stored);
    crate::app_menu::refresh_localized_menus(app).map_err(|error| error.to_string())?;
    crate::meeting::overlay_locale::refresh(app);
    let current = snapshot(app);
    app.emit("snack-locale-changed", &current)
        .map_err(|error| error.to_string())?;
    Ok(current)
}

fn dictionary(locale: Locale) -> &'static Value {
    static EN: OnceLock<Value> = OnceLock::new();
    static ZH: OnceLock<Value> = OnceLock::new();
    match locale {
        Locale::En => EN.get_or_init(|| {
            serde_json::from_str(include_str!("../resources/i18n/en.json"))
                .expect("validated English resource")
        }),
        Locale::Zh => ZH.get_or_init(|| {
            serde_json::from_str(include_str!("../resources/i18n/zh.json"))
                .expect("validated Chinese resource")
        }),
    }
}

fn lookup(locale: Locale, key: &str) -> Option<&'static str> {
    let mut value = dictionary(locale);
    for segment in key.split('.') {
        value = value.get(segment)?;
    }
    value.as_str().filter(|value| !value.is_empty())
}

pub(crate) fn text(locale: Locale, fallback: Locale, key: &str) -> String {
    lookup(locale, key)
        .or_else(|| lookup(fallback, key))
        .unwrap_or_else(|| lookup(fallback, "common.unavailable").expect("fallback resource"))
        .to_string()
}

/// Resolve a complete native resource section using the configured default per key.
pub(crate) fn resource(locale: Locale, fallback: Locale, key: &str) -> Value {
    let path = format!("/{}", key.replace('.', "/"));
    let Some(base) = dictionary(fallback).pointer(&path) else {
        return Value::Null;
    };
    if let Some(object) = base.as_object() {
        return Value::Object(
            object
                .keys()
                .map(|child| {
                    (
                        child.clone(),
                        resource(locale, fallback, &format!("{key}.{child}")),
                    )
                })
                .collect(),
        );
    }
    Value::String(text(locale, fallback, key))
}

pub(crate) fn t(app: &AppHandle, key: &str) -> String {
    let current = snapshot(app);
    text(current.locale, current.default_locale, key)
}

/// Native messages use the static/argument subset; ICU plurals render in the JS engine.
pub(crate) fn format(app: &AppHandle, key: &str, values: &[(&str, String)]) -> String {
    let arguments = values
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect::<HashMap<_, _>>();
    strfmt::strfmt(&t(app, key), &arguments).unwrap_or_else(|_| t(app, "common.unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_supported_scripts_without_collapsing_traditional_chinese() {
        assert_eq!(normalize("en_GB"), Some(Locale::En));
        assert_eq!(normalize("zh-Hans-CN"), Some(Locale::Zh));
        assert_eq!(normalize("zh-TW"), None);
        assert_eq!(normalize("../../en"), None);
    }

    #[test]
    fn menus_and_unknown_messages_use_language_resources() {
        assert_eq!(text(Locale::En, Locale::Zh, "desktop.quit"), "Quit");
        assert_eq!(text(Locale::Zh, Locale::En, "desktop.quit"), "退出");
        assert_eq!(
            text(Locale::En, Locale::Zh, "missing.key"),
            lookup(Locale::Zh, "common.unavailable").unwrap()
        );
    }
}
