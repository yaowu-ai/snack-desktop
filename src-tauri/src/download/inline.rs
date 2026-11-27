//! A blob URL belongs to its WebView. Accept bounded chunks, never arbitrary local paths.
use super::*;
use std::{
    collections::HashMap,
    io::Write,
    sync::{LazyLock, Mutex},
};

const CHUNK: usize = 512 * 1024;
type Key = (String, String, String); // source window, origin, download ID
static PENDING: LazyLock<Mutex<HashMap<Key, Transfer>>> = LazyLock::new(Mutex::default);
struct Transfer {
    file: tempfile::NamedTempFile,
    filename: String,
    total: u64,
    written: u64,
    updated: Instant,
}

pub(crate) fn clear_window(label: &str) {
    if let Ok(mut pending) = PENDING.lock() {
        pending.retain(|(owner, _, _), _| owner != label);
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Chunk {
    offset: u64,
    total: u64,
    data_base64: String,
}

pub(super) async fn receive(
    app: AppHandle,
    window: WebviewWindow,
    origin: Url,
    request: DownloadSnackFileRequest,
) -> Result<DownloadSnackFileResult, String> {
    let id = request.download_id;
    let key = (
        window.label().into(),
        origin.origin().ascii_serialization(),
        id.clone(),
    );
    let filename = sanitize_filename(&request.filename)?;
    let dir = resolve_destination_dir(&app)?;
    let Some(chunk) = request.chunk else {
        PENDING
            .lock()
            .map_err(|_| "inline queue unavailable")?
            .remove(&key);
        return Ok(DownloadSnackFileResult {
            download_id: id,
            path: String::new(),
            downloaded_bytes: 0,
        });
    };
    let total = chunk.total;
    let name = filename.clone();
    let (written, path) = tokio::task::spawn_blocking(move || append(&dir, key, &name, chunk))
        .await
        .map_err(|_| "inline writer unavailable")??;
    let result_path = path
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if written == total {
        emit_generated_image_completed(&window, &id, &filename, &result_path, total);
    } else {
        emit_download_progress(
            &window,
            DownloadProgressPayload {
                download_id: id.clone(),
                status: "progress",
                filename: Some(filename),
                downloaded_bytes: written,
                total_bytes: Some(total),
                percent: calculate_percent(written, Some(total)),
                path: None,
                message: None,
            },
        );
        set_window_progress(&window, Some(total), written);
    }
    Ok(DownloadSnackFileResult {
        download_id: id,
        path: result_path,
        downloaded_bytes: written,
    })
}

fn append(
    dir: &Path,
    key: Key,
    filename: &str,
    chunk: Chunk,
) -> Result<(u64, Option<PathBuf>), String> {
    if chunk.data_base64.len() > CHUNK.div_ceil(3) * 4 {
        return Err("oversized inline chunk".into());
    }
    let bytes = BASE64_STANDARD
        .decode(chunk.data_base64)
        .map_err(|_| "invalid inline bytes")?;
    let end = chunk
        .offset
        .checked_add(bytes.len() as u64)
        .ok_or("invalid inline range")?;
    if bytes.len() > CHUNK || end > chunk.total || (end < chunk.total && bytes.len() != CHUNK) {
        return Err("invalid inline range".into());
    }
    let mut pending = PENDING.lock().map_err(|_| "inline queue unavailable")?;
    pending.retain(|_, v| v.updated.elapsed() < Duration::from_secs(300));
    let mut transfer = match pending.remove(&key) {
        Some(t)
            if t.filename == filename && t.total == chunk.total && t.written == chunk.offset =>
        {
            t
        }
        None if chunk.offset == 0 && pending.len() < 32 => {
            std::fs::create_dir_all(dir).map_err(|_| "failed to create downloads directory")?;
            Transfer {
                file: tempfile::NamedTempFile::new_in(dir)
                    .map_err(|_| "failed to stage download")?,
                filename: filename.into(),
                total: chunk.total,
                written: 0,
                updated: Instant::now(),
            }
        }
        _ => return Err("inline download is no longer active".into()),
    };
    transfer
        .file
        .write_all(&bytes)
        .map_err(|_| "failed to write download")?;
    if end == chunk.total {
        return Ok((
            end,
            Some(finish_download(
                transfer.file.into_temp_path(),
                dir,
                filename,
            )?),
        ));
    }
    transfer.written = end;
    transfer.updated = Instant::now();
    pending.insert(key, transfer);
    Ok((end, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_publish_only_after_completion() {
        let dir = tempfile::tempdir().unwrap();
        let key = ("main".into(), "https://snack.test".into(), "1".into());
        let total = CHUNK as u64 + 4;
        let first = Chunk {
            offset: 0,
            total,
            data_base64: BASE64_STANDARD.encode(vec![0; CHUNK]),
        };
        assert!(append(dir.path(), key.clone(), "demo.bin", first)
            .unwrap()
            .1
            .is_none());
        assert!(!dir.path().join("demo.bin").exists());
        let last = Chunk {
            offset: CHUNK as u64,
            total,
            data_base64: BASE64_STANDARD.encode(b"done"),
        };
        let (_, path) = append(dir.path(), key, "demo.bin", last).unwrap();
        assert_eq!(&std::fs::read(path.unwrap()).unwrap()[CHUNK..], b"done");
    }
}
