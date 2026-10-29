//! Localized recording surfaces use bundled resources, including when offline.
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::i18n::{self, Locale};

static REMINDER_APPLICATION: Mutex<String> = Mutex::new(String::new());

pub(crate) fn set_reminder_application(name: &str) {
    if let Ok(mut application) = REMINDER_APPLICATION.lock() {
        *application = name.to_owned();
    }
}

pub(crate) fn payload(locale: Locale, fallback: Locale) -> Value {
    let messages = i18n::resource(locale, fallback, "desktop.overlay");
    let application = REMINDER_APPLICATION
        .lock()
        .ok()
        .map(|name| name.clone())
        .unwrap_or_default();
    let name = if application.is_empty() {
        i18n::text(locale, fallback, "desktop.overlay.meetingApplication")
    } else {
        application
    };
    let arguments = [("application".to_owned(), name)].into_iter().collect();
    let description = strfmt::strfmt(
        &i18n::text(locale, fallback, "desktop.overlay.reminderDescription"),
        &arguments,
    )
    .unwrap_or_else(|_| i18n::text(locale, fallback, "common.unavailable"));
    json!({ "locale": i18n::module_locale(locale, "desktop"), "messages": messages, "reminderDescription": description })
}

pub(crate) fn app_payload(app: &AppHandle) -> Value {
    let state = i18n::snapshot(app);
    payload(state.locale, state.default_locale)
}

/// Escape HTML delimiters even when a detected application name contains script markup.
pub(crate) fn serialize(payload: &Value) -> String {
    payload
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

pub(crate) fn render(html: &str, payload: &Value) -> String {
    html.replace(
        "<!--SNACK_LOCALE-->",
        &format!(
            "<script>window.__SNACK_LOCALE__={};</script><script>{}</script>",
            serialize(payload),
            include_str!("overlay-locale.js"),
        ),
    )
}

/// Updating resources never reloads a recording window or resets its local state.
pub(crate) fn refresh(app: &AppHandle) {
    let payload = app_payload(app);
    let script = format!(
        "window.setSnackLocale && window.setSnackLocale({});",
        serialize(&payload)
    );
    for (label, title_key) in [
        (super::overlay::OVERLAY_LABEL, "desktop.overlay.windowTitle"),
        (super::note_overlay::LABEL, "desktop.overlay.windowTitle"),
        (
            super::overlay::REMINDER_LABEL,
            "desktop.overlay.reminderWindowTitle",
        ),
    ] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.eval(&script);
            let _ = window.set_title(&i18n::t(app, title_key));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_payload_cannot_close_the_script_element() {
        let result = serialize(&json!({ "name": "</script><script>alert(1)</script>&" }));
        assert!(!result.contains('<'));
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap()["name"],
            "</script><script>alert(1)</script>&"
        );
    }

    #[test]
    fn payload_has_both_locales_and_a_complete_reminder_sentence() {
        let en = payload(Locale::En, Locale::Zh);
        let zh = payload(Locale::Zh, Locale::En);
        assert_eq!(en["messages"]["pause"], i18n::text(Locale::En, Locale::Zh, "desktop.overlay.pause"));
        assert_eq!(zh["messages"]["pause"], "暂停录音");
        assert_eq!(en["locale"], serde_json::to_value(i18n::module_locale(Locale::En, "desktop")).unwrap());
        assert!(!en["reminderDescription"].as_str().unwrap().contains("{application}"));
        assert_eq!(
            en["messages"].as_object().unwrap().len(),
            zh["messages"].as_object().unwrap().len()
        );
    }
}
