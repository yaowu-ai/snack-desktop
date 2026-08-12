use std::{fs, path::PathBuf, sync::Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::web::is_allowed_web_origin;

const RECORD_METADATA_TYPE: &str = "cn.yaowutech.snack.record-handoff+json";
const RECORD_IMPORT_READY_EVENT: &str = "snack-record-import-ready";
const MAX_TRANSCRIPT_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, PartialEq)]
struct MeetingNavigationTarget {
    path: &'static str,
    query: Option<&'static str>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardMetadata {
    version: u8,
    source: String,
    created_at: String,
    expires_at: String,
    byte_length: usize,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingRecordImport {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_text: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub auto_submit: bool,
    #[serde(default)]
    delivery_state: RecordImportDeliveryState,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RecordImportDeliveryState {
    #[default]
    Pending,
    Delivered,
}

pub(crate) struct RecordImportStore {
    path: PathBuf,
    pending: Mutex<Option<PendingRecordImport>>,
}

impl RecordImportStore {
    pub(crate) fn load(app: &AppHandle) -> Result<Self, String> {
        let directory = app
            .path()
            .app_data_dir()
            .map_err(|error| error.to_string())?;
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let path = directory.join("pending-record-import.json");
        let pending = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| "invalid pending record import".to_string())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self {
            path,
            pending: Mutex::new(pending),
        })
    }

    fn replace(&self, import: PendingRecordImport) -> Result<(), String> {
        let mut pending = self.pending.lock().expect("record import store poisoned");
        *pending = Some(import);
        self.persist(pending.as_ref())
    }

    fn claim(&self) -> Option<PendingRecordImport> {
        self.pending
            .lock()
            .expect("record import store poisoned")
            .clone()
    }

    fn claim_for_webview(&self) -> Option<PendingRecordImport> {
        self.pending
            .lock()
            .expect("record import store poisoned")
            .as_ref()
            .filter(|item| item.delivery_state == RecordImportDeliveryState::Pending)
            .cloned()
    }

    fn has_pending_automatic_notes(&self) -> bool {
        self.pending
            .lock()
            .expect("record import store poisoned")
            .as_ref()
            .is_some_and(|record_import| record_import.auto_submit)
    }

    fn mark_delivered(&self, id: &str) -> Result<bool, String> {
        let mut pending = self.pending.lock().expect("record import store poisoned");
        let Some(item) = pending.as_mut().filter(|item| item.id == id) else {
            return Ok(false);
        };
        item.delivery_state = RecordImportDeliveryState::Delivered;
        self.persist(pending.as_ref())?;
        Ok(true)
    }

    fn acknowledge(&self, id: &str) -> Result<bool, String> {
        let mut pending = self.pending.lock().expect("record import store poisoned");
        if pending.as_ref().is_some_and(|item| item.id == id) {
            *pending = None;
            self.persist(None)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn persist(&self, pending: Option<&PendingRecordImport>) -> Result<(), String> {
        match pending {
            Some(import) => {
                let temporary_path = self.path.with_extension("json.tmp");
                let bytes = serde_json::to_vec(import).map_err(|error| error.to_string())?;
                fs::write(&temporary_path, bytes).map_err(|error| error.to_string())?;
                fs::rename(&temporary_path, &self.path).map_err(|error| error.to_string())
            }
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.to_string()),
            },
        }
    }
}

pub(crate) fn has_pending_automatic_notes(app: &AppHandle) -> bool {
    app.try_state::<RecordImportStore>()
        .is_some_and(|store| store.has_pending_automatic_notes())
}

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    app.manage(RecordImportStore::load(app)?);
    Ok(())
}

/// Queue the meeting notes prompt separately from the transcript attachment.
pub(crate) fn open_prefill_with_attachment(
    app: &AppHandle,
    prompt: String,
    attachment_name: String,
    attachment_text: String,
    auto_submit: bool,
) -> Result<(), String> {
    let record_import =
        build_meeting_import(prompt, attachment_name, attachment_text, auto_submit)?;
    app.state::<RecordImportStore>()
        .replace(record_import.clone())?;
    show_main_window(app);
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main webview is unavailable".to_string())?;
    prefill_root_input(&window, &record_import)?;
    navigate_to_root(app)
}

/// Persist an automatic meeting-notes handoff and wake the existing webview
/// without showing, focusing, or navigating the user's active page.
pub(crate) fn queue_background_with_attachment(
    app: &AppHandle,
    prompt: String,
    attachment_name: String,
    attachment_text: String,
) -> Result<(), String> {
    let record_import = build_meeting_import(prompt, attachment_name, attachment_text, true)?;
    app.state::<RecordImportStore>()
        .replace(record_import.clone())?;
    app.emit(
        RECORD_IMPORT_READY_EVENT,
        serde_json::json!({ "id": record_import.id }),
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn handle_open_url(app: &AppHandle, url: &tauri::Url) {
    if let Some(target) = meeting_navigation_target(url) {
        open_meeting_target(app, target);
    } else if is_clipboard_import_url(url) {
        handle_clipboard_import(app);
    }
}

fn handle_clipboard_import(app: &AppHandle) {
    match read_clipboard_import() {
        Ok(import) => {
            if let Err(error) = app.state::<RecordImportStore>().replace(import) {
                crate::logging::write_app_log(
                    app,
                    "error",
                    "record-import",
                    "Clipboard import could not be persisted",
                    Some(&serde_json::json!({ "reason": error })),
                );
            } else {
                show_main_window(app);
                match navigate_to_root(app) {
                    Ok(()) => crate::logging::write_app_log(
                        app,
                        "info",
                        "record-import",
                        "Clipboard import is ready and the webview is navigating to the root page",
                        None,
                    ),
                    Err(error) => crate::logging::write_app_log(
                        app,
                        "warn",
                        "record-import",
                        "Clipboard import is ready but the webview could not navigate to the root page",
                        Some(&serde_json::json!({ "reason": error })),
                    ),
                }
            }
        }
        Err(error) => crate::logging::write_app_log(
            app,
            "warn",
            "record-import",
            "Clipboard import was rejected",
            Some(&serde_json::json!({ "reason": error })),
        ),
    }
}

fn open_meeting_target(app: &AppHandle, target: MeetingNavigationTarget) {
    show_main_window(app);
    if let Err(error) = navigate_to_web_path(app, target.path, target.query) {
        crate::logging::write_app_log(
            app,
            "warn",
            "meeting-deep-link",
            "Meeting deep link could not be opened",
            Some(&serde_json::json!({ "reason": error })),
        );
    }
}

pub(crate) fn handle_page_load(app: &AppHandle, window: &WebviewWindow) {
    let Ok(url) = window.url() else {
        return;
    };
    if !is_allowed_web_origin(&url) || url.path() != "/" {
        return;
    }
    let store = app.state::<RecordImportStore>();
    let Some(import) = store.claim_for_webview() else {
        return;
    };
    if let Err(error) = prefill_root_input(window, &import)
        .and_then(|_| store.mark_delivered(&import.id).map(|_| ()))
    {
        crate::logging::write_app_log(
            app,
            "warn",
            "record-import",
            "Clipboard import could not be written after root page loaded",
            Some(&serde_json::json!({ "reason": error })),
        );
    } else {
        crate::logging::write_app_log(
            app,
            "info",
            "record-import",
            "Clipboard import was written after root page loaded",
            None,
        );
    }
}

#[tauri::command]
pub(crate) fn claim_pending_record_import(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<Option<PendingRecordImport>, String> {
    require_allowed_origin(&window)?;
    Ok(app.state::<RecordImportStore>().claim())
}

#[tauri::command]
pub(crate) fn acknowledge_record_import_prefilled(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
) -> Result<bool, String> {
    require_allowed_origin(&window)?;
    app.state::<RecordImportStore>().acknowledge(&id)
}

fn require_allowed_origin(window: &WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|error| error.to_string())?;
    if is_allowed_web_origin(&url) {
        Ok(())
    } else {
        Err("origin is not allowed to access record imports".to_string())
    }
}

fn is_clipboard_import_url(url: &tauri::Url) -> bool {
    url.scheme() == "snack"
        && url.host_str() == Some("chat")
        && url
            .query_pairs()
            .any(|(key, value)| key == "source" && value == "clipboard")
}

fn meeting_navigation_target(url: &tauri::Url) -> Option<MeetingNavigationTarget> {
    if url.scheme() != "snack" || url.host_str() != Some("meeting") {
        return None;
    }
    let action = url
        .query_pairs()
        .find_map(|(key, value)| (key == "action").then(|| value.into_owned()));
    let ensure_latest = url
        .query_pairs()
        .any(|(key, value)| key == "ensureLatest" && value == "1");
    match action.as_deref() {
        Some("apps") => Some(MeetingNavigationTarget {
            path: "/apps",
            query: ensure_latest.then_some("ensureLatest=1"),
        }),
        Some("record") => Some(MeetingNavigationTarget {
            path: "/meeting",
            query: Some("quick=1"),
        }),
        Some("settings") => Some(MeetingNavigationTarget {
            path: "/meeting/settings",
            query: None,
        }),
        Some("records") | None => Some(MeetingNavigationTarget {
            path: "/meeting",
            query: None,
        }),
        Some(_) => None,
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn navigate_to_root(app: &AppHandle) -> Result<(), String> {
    navigate_to_web_path(app, "/", None)
}

fn navigate_to_web_path(app: &AppHandle, path: &str, query: Option<&str>) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main webview is unavailable".to_string())?;
    require_allowed_origin(&window)?;
    let mut root_url = window.url().map_err(|error| error.to_string())?;
    root_url.set_path(path);
    root_url.set_query(query);
    root_url.set_fragment(None);
    window.navigate(root_url).map_err(|error| error.to_string())
}

fn build_meeting_import(
    prompt: String,
    attachment_name: String,
    attachment_text: String,
    auto_submit: bool,
) -> Result<PendingRecordImport, String> {
    if prompt.trim().is_empty() || prompt.len() > MAX_TRANSCRIPT_BYTES {
        return Err("会议纪要 Prompt 为空或超过 5 MB".to_string());
    }
    if attachment_name.trim().is_empty() || attachment_text.len() > MAX_TRANSCRIPT_BYTES {
        return Err("会议转写文件无效或超过 5 MB".to_string());
    }
    let checksum = format!(
        "{:x}",
        Sha256::digest(format!("{prompt}\0{attachment_name}\0{attachment_text}").as_bytes())
    );
    Ok(PendingRecordImport {
        id: format!("meeting-v2-{checksum}"),
        text: prompt,
        attachment_name: Some(attachment_name),
        attachment_text: Some(attachment_text),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        auto_submit,
        delivery_state: RecordImportDeliveryState::Pending,
    })
}

fn prefill_root_input(window: &WebviewWindow, import: &PendingRecordImport) -> Result<(), String> {
    require_allowed_origin(window)?;
    let text = serde_json::to_string(&import.text).map_err(|error| error.to_string())?;
    let script = format!(
        "window.sessionStorage.setItem('prefill_message', {text});window.sessionStorage.removeItem('prefill_message_options');"
    );
    window.eval(&script).map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn read_clipboard_import() -> Result<PendingRecordImport, String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    let pasteboard = NSPasteboard::generalPasteboard();
    let metadata_type = NSString::from_str(RECORD_METADATA_TYPE);
    let metadata_text = pasteboard
        .stringForType(&metadata_type)
        .map(|value| value.to_string())
        .ok_or_else(|| "missing Snack Record clipboard metadata".to_string())?;
    let metadata: ClipboardMetadata = serde_json::from_str(&metadata_text)
        .map_err(|_| "invalid Snack Record clipboard metadata".to_string())?;

    if metadata.version != 1 || metadata.source != "snack-record" {
        return Err("unsupported Snack Record clipboard metadata".to_string());
    }
    if metadata.created_at.is_empty() || metadata.expires_at.is_empty() {
        return Err("clipboard metadata is missing timestamps".to_string());
    }

    let text = pasteboard
        .stringForType(unsafe { NSPasteboardTypeString })
        .map(|value| value.to_string())
        .ok_or_else(|| "missing clipboard text".to_string())?;
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_TRANSCRIPT_BYTES || bytes.len() != metadata.byte_length
    {
        return Err("clipboard text size does not match metadata".to_string());
    }
    let checksum = format!("{:x}", Sha256::digest(bytes));
    if !checksum.eq_ignore_ascii_case(&metadata.sha256) {
        return Err("clipboard text checksum does not match metadata".to_string());
    }

    Ok(PendingRecordImport {
        id: format!("clipboard-v1-{checksum}"),
        text,
        attachment_name: None,
        attachment_text: None,
        created_at: metadata.created_at,
        auto_submit: false,
        delivery_state: RecordImportDeliveryState::Pending,
    })
}

#[cfg(not(target_os = "macos"))]
#[cfg(target_os = "windows")]
fn read_clipboard_import() -> Result<PendingRecordImport, String> {
    use windows_sys::Win32::System::{DataExchange, Ole};

    unsafe {
        if DataExchange::OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("unable to access the Windows clipboard".to_string());
        }
        let result = (|| {
            let metadata = read_windows_clipboard_string(DataExchange::RegisterClipboardFormatW(
                wide_null(RECORD_METADATA_TYPE).as_ptr(),
            ))?;
            let metadata: ClipboardMetadata = serde_json::from_str(&metadata)
                .map_err(|_| "invalid Snack Record clipboard metadata".to_string())?;
            if metadata.version != 1 || metadata.source != "snack-record" {
                return Err("unsupported Snack Record clipboard metadata".to_string());
            }
            let text = read_windows_clipboard_string(u32::from(Ole::CF_UNICODETEXT))?;
            let bytes = text.as_bytes();
            if bytes.is_empty()
                || bytes.len() > MAX_TRANSCRIPT_BYTES
                || bytes.len() != metadata.byte_length
            {
                return Err("clipboard text size does not match metadata".to_string());
            }
            let checksum = format!("{:x}", Sha256::digest(bytes));
            if !checksum.eq_ignore_ascii_case(&metadata.sha256) {
                return Err("clipboard text checksum does not match metadata".to_string());
            }
            Ok(PendingRecordImport {
                id: format!("clipboard-v1-{checksum}"),
                text,
                attachment_name: None,
                attachment_text: None,
                created_at: metadata.created_at,
                auto_submit: false,
                delivery_state: RecordImportDeliveryState::Pending,
            })
        })();
        DataExchange::CloseClipboard();
        result
    }
}

#[cfg(target_os = "windows")]
unsafe fn read_windows_clipboard_string(format: u32) -> Result<String, String> {
    use windows_sys::Win32::System::{DataExchange, Memory};

    let handle = DataExchange::GetClipboardData(format);
    if handle.is_null() {
        return Err("missing Snack Record clipboard data".to_string());
    }
    let pointer = Memory::GlobalLock(handle) as *const u16;
    if pointer.is_null() {
        return Err("unable to read Snack Record clipboard data".to_string());
    }
    let mut length = 0usize;
    while *pointer.add(length) != 0 {
        length += 1;
    }
    let text = String::from_utf16(std::slice::from_raw_parts(pointer, length))
        .map_err(|_| "clipboard data is not UTF-16".to_string());
    Memory::GlobalUnlock(handle);
    text
}

#[cfg(target_os = "windows")]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_clipboard_import() -> Result<PendingRecordImport, String> {
    Err("clipboard import is not supported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        build_meeting_import, is_clipboard_import_url, meeting_navigation_target,
        MeetingNavigationTarget, PendingRecordImport, RecordImportDeliveryState,
    };

    #[test]
    fn accepts_only_the_v1_clipboard_link() {
        assert!(is_clipboard_import_url(
            &"snack://chat?source=clipboard".parse().unwrap()
        ));
        assert!(!is_clipboard_import_url(
            &"snack://chat?source=http".parse().unwrap()
        ));
        assert!(!is_clipboard_import_url(
            &"snack://other?source=clipboard".parse().unwrap()
        ));
    }

    #[test]
    fn maps_meeting_links_to_the_requested_web_view() {
        assert_eq!(
            meeting_navigation_target(&"snack://meeting?action=record".parse().unwrap()),
            Some(MeetingNavigationTarget {
                path: "/meeting",
                query: Some("quick=1"),
            })
        );
        assert_eq!(
            meeting_navigation_target(&"snack://meeting?action=apps".parse().unwrap()),
            Some(MeetingNavigationTarget {
                path: "/apps",
                query: None,
            })
        );
        assert_eq!(
            meeting_navigation_target(
                &"snack://meeting?action=apps&ensureLatest=1"
                    .parse()
                    .unwrap()
            ),
            Some(MeetingNavigationTarget {
                path: "/apps",
                query: Some("ensureLatest=1"),
            })
        );
        assert_eq!(
            meeting_navigation_target(&"snack://meeting?action=records".parse().unwrap()),
            Some(MeetingNavigationTarget {
                path: "/meeting",
                query: None,
            })
        );
        assert_eq!(
            meeting_navigation_target(&"snack://meeting?action=settings".parse().unwrap()),
            Some(MeetingNavigationTarget {
                path: "/meeting/settings",
                query: None,
            })
        );
        assert_eq!(
            meeting_navigation_target(&"snack://meeting?action=unknown".parse().unwrap()),
            None
        );
    }

    #[test]
    fn legacy_pending_import_defaults_to_pending_delivery() {
        let import: PendingRecordImport = serde_json::from_str(
            r#"{"id":"legacy","text":"transcript","createdAt":"2026-07-16T00:00:00Z"}"#,
        )
        .unwrap();

        assert_eq!(import.delivery_state, RecordImportDeliveryState::Pending);
    }

    #[test]
    fn meeting_handoff_keeps_prompt_and_transcript_attachment_separate() {
        let import = build_meeting_import(
            "请生成会议纪要".to_string(),
            "Snack会议-2026-08-06.txt".to_string(),
            "会议转写正文".to_string(),
            false,
        )
        .unwrap();

        assert_eq!(import.text, "请生成会议纪要");
        assert_eq!(
            import.attachment_name.as_deref(),
            Some("Snack会议-2026-08-06.txt")
        );
        assert_eq!(import.attachment_text.as_deref(), Some("会议转写正文"));
        assert!(!import.auto_submit);
        assert!(import.id.starts_with("meeting-v2-"));
    }

    #[test]
    fn meeting_transcription_completion_handoff_requests_automatic_submission() {
        let import = build_meeting_import(
            "请生成会议纪要".to_string(),
            "Snack会议.txt".to_string(),
            "会议转写正文".to_string(),
            true,
        )
        .unwrap();

        assert!(import.auto_submit);
    }
}
