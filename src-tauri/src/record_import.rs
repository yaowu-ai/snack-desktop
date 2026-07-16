use std::{fs, path::PathBuf, sync::Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::web::is_allowed_web_origin;

const RECORD_METADATA_TYPE: &str = "cn.yaowutech.snack.record-handoff+json";
const MAX_TRANSCRIPT_BYTES: usize = 5 * 1024 * 1024;

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
    pub created_at: String,
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

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    app.manage(RecordImportStore::load(app)?);
    Ok(())
}

pub(crate) fn handle_open_url(app: &AppHandle, url: &tauri::Url) {
    if !is_clipboard_import_url(url) {
        return;
    }

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
                crate::logging::write_app_log(
                    app,
                    "info",
                    "record-import",
                    "Clipboard import is ready",
                    None,
                );
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

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
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
        created_at: metadata.created_at,
    })
}

#[cfg(not(target_os = "macos"))]
#[cfg(target_os = "windows")]
fn read_clipboard_import() -> Result<PendingRecordImport, String> {
    use windows_sys::Win32::System::{DataExchange, Memory};

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
            let text = read_windows_clipboard_string(DataExchange::CF_UNICODETEXT)?;
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
                created_at: metadata.created_at,
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
    use super::is_clipboard_import_url;

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
}
