//! Local-only conversion. A renderer can supply bytes, never a path, executable or arguments.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager, WebviewWindow};
use tempfile::TempDir;
const LEGACY_MAX: u64 = 100 * 1024 * 1024;
const PROCESSING_CEILING: u64 = 2048 * 1024 * 1024;
const CHUNK: usize = 384 * 1024;
struct Job {
    dir: Arc<TempDir>,
    owner: (String, String),
    id: String,
    cancel: Arc<AtomicBool>,
    max_bytes: u64,
    phase: u8,
    touched: Instant,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
static JOB: OnceLock<Mutex<Option<Job>>> = OnceLock::new();
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    op: String,
    id: Option<String>,
    offset: Option<u64>,
    data: Option<String>,
    #[serde(rename = "maxVideoBytes")]
    max_bytes: Option<u64>,
}
fn binary(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}
pub(crate) async fn handle(
    app: AppHandle,
    window: WebviewWindow,
    req: Request,
) -> Result<Value, String> {
    let root = app
        .path()
        .resource_dir()
        .map_err(|_| "VIDEO_RUNTIME")?
        .join("chat-video");
    if req.op == "available" {
        let ffmpeg = binary(&root, "ffmpeg").is_file();
        let ffprobe = binary(&root, "ffprobe").is_file();
        if !ffmpeg || !ffprobe {
            crate::logging::write_app_log(
                &app,
                "warn",
                "chat-video",
                "Bundled video tools missing",
                Some(&json!({ "ffmpegPresent": ffmpeg, "ffprobePresent": ffprobe })),
            );
        }
        return Ok(json!(ffmpeg && ffprobe));
    }
    let url = window.url().map_err(|_| "VIDEO_SESSION")?;
    let owner = (
        url.origin().ascii_serialization(),
        crate::download::read_auth_token_cookie(&window, url)?,
    );
    tauri::async_runtime::spawn_blocking(move || operate(root, owner, req))
        .await
        .map_err(|_| "VIDEO_WORKER".to_string())?
}
fn operate(root: PathBuf, owner: (String, String), req: Request) -> Result<Value, String> {
    let slot = JOB.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().map_err(|_| "VIDEO_BUSY")?;
    if guard
        .as_ref()
        .is_some_and(|j| j.owner != owner || j.touched.elapsed() > Duration::from_secs(600))
    {
        *guard = None;
    }
    if req.op == "begin" {
        if guard.is_some() {
            return Err("VIDEO_BUSY".into());
        }
        let dir = Arc::new(tempfile::tempdir().map_err(|_| "VIDEO_TEMP")?);
        let id = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        File::create(dir.path().join("in")).map_err(|_| "VIDEO_TEMP")?;
        *guard = Some(Job {
            dir,
            owner,
            id: id.clone(),
            cancel: Arc::new(false.into()),
            max_bytes: req
                .max_bytes
                .unwrap_or(LEGACY_MAX)
                .clamp(1, PROCESSING_CEILING),
            phase: 0,
            touched: Instant::now(),
        });
        return Ok(json!(id));
    }
    let j = guard
        .as_mut()
        .filter(|j| Some(&j.id) == req.id.as_ref())
        .ok_or("VIDEO_JOB")?;
    j.touched = Instant::now();
    match req.op.as_str() {
        "release" => {
            *guard = None;
            Ok(Value::Null)
        }
        "append" if j.phase == 0 => {
            let data = req.data.ok_or("VIDEO_CHUNK")?;
            if data.len() > CHUNK * 4 / 3 + 4 {
                return Err("VIDEO_CHUNK".into());
            }
            let bytes = STANDARD.decode(data).map_err(|_| "VIDEO_CHUNK")?;
            let p = j.dir.path().join("in");
            let size = fs::metadata(&p).map_err(|_| "VIDEO_INPUT")?.len();
            if req.offset != Some(size) || size + bytes.len() as u64 > j.max_bytes {
                return Err("VIDEO_LIMIT".into());
            }
            OpenOptions::new()
                .append(true)
                .open(p)
                .and_then(|mut f| f.write_all(&bytes))
                .map_err(|_| "VIDEO_WRITE")?;
            Ok(Value::Null)
        }
        "convert" if j.phase == 0 => {
            j.phase = 1;
            let dir = j.dir.clone();
            let cancel = j.cancel.clone();
            let max_bytes = j.max_bytes;
            drop(guard);
            convert(&root, dir.path(), &cancel, max_bytes)?;
            let size = fs::metadata(dir.path().join("out.mp4"))
                .map_err(|_| "VIDEO_OUTPUT")?
                .len();
            if size == 0 || size > max_bytes {
                return Err("VIDEO_LIMIT".into());
            }
            let mut g = slot.lock().map_err(|_| "VIDEO_JOB")?;
            g.as_mut()
                .filter(|j| Some(&j.id) == req.id.as_ref())
                .ok_or("VIDEO_JOB")?
                .phase = 2;
            Ok(json!(size))
        }
        "read" if j.phase == 2 => {
            let mut f = File::open(j.dir.path().join("out.mp4")).map_err(|_| "VIDEO_OUTPUT")?;
            f.seek(SeekFrom::Start(req.offset.ok_or("VIDEO_OFFSET")?))
                .map_err(|_| "VIDEO_OFFSET")?;
            let mut buf = Vec::new();
            f.take(CHUNK as u64)
                .read_to_end(&mut buf)
                .map_err(|_| "VIDEO_READ")?;
            Ok(json!(STANDARD.encode(buf)))
        }
        _ => Err("VIDEO_REQUEST".into()),
    }
}
const INPUT: [&str; 4] = [
    "-protocol_whitelist",
    "file",
    "-format_whitelist",
    "mov,matroska,webm,avi,flv,mpegts,asf",
];
// Match the H5 chat player's AVC profile allowlist (profile_idc 66, 77, 100).
// Unknown profiles must be transcoded rather than copied into the output MP4.
fn can_copy_video(v: &Value, w: u64, h: u64, hdr: bool) -> bool {
    let fps = v["avg_frame_rate"]
        .as_str()
        .and_then(|r| r.split_once('/'))
        .and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?))
        .unwrap_or(0.);
    v["codec_name"] == "h264"
        && matches!(
            v["profile"].as_str(),
            // The bundled --enable-small ffprobe reports numeric profile strings.
            Some(
                "Baseline" | "Constrained Baseline" | "Main" | "High" | "66" | "578" | "77" | "100"
            )
        )
        && v["pix_fmt"] == "yuv420p"
        && !hdr
        && w.max(h) <= 1920
        && w.min(h) <= 1080
        && fps > 0.
        && fps <= 30.01
        && matches!(v["level"].as_i64(), Some(1..=41))
}
fn convert(root: &Path, dir: &Path, cancel: &AtomicBool, max_bytes: u64) -> Result<(), String> {
    let mut probe = Command::new(binary(root, "ffprobe"));
    probe.current_dir(dir).args(["-v","error"]).args(INPUT).args(["-show_entries","stream=codec_name,codec_type,profile,level,width,height,pix_fmt,color_transfer,avg_frame_rate,channels","-of","json","in"]).stdout(File::create(dir.join("probe")).map_err(|_|"VIDEO_TEMP")?);
    run(probe, cancel, max_bytes)?;
    let p = dir.join("probe");
    if fs::metadata(&p).map_err(|_| "VIDEO_PROBE")?.len() > 65536 {
        return Err("VIDEO_PROBE".into());
    }
    let parsed: Value = serde_json::from_reader(File::open(p).map_err(|_| "VIDEO_PROBE")?)
        .map_err(|_| "VIDEO_PROBE")?;
    let tracks = parsed["streams"].as_array().ok_or("VIDEO_PROBE")?;
    let v = tracks
        .iter()
        .find(|t| t["codec_type"] == "video")
        .ok_or("VIDEO_INPUT")?;
    let (w, h) = (
        v["width"].as_u64().unwrap_or(0),
        v["height"].as_u64().unwrap_or(0),
    );
    if w * h == 0 || w.max(h) > 8192 {
        return Err("VIDEO_DIMENSIONS".into());
    }
    let hdr = matches!(
        v["color_transfer"].as_str(),
        Some("smpte2084" | "arib-std-b67")
    );
    let copy = can_copy_video(v, w, h, hdr);
    let mut cmd = Command::new(binary(root, "ffmpeg"));
    cmd.current_dir(dir)
        .args(["-nostdin", "-v", "error"])
        .args(INPUT)
        .args([
            "-i",
            "in",
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-map_metadata",
            "-1",
            "-sn",
            "-dn",
        ]);
    if copy {
        cmd.args(["-c:v", "copy"]);
    } else {
        // Both native encoders accept NV12. Planar input can corrupt full-range frames.
        let scale = "scale='min(iw,if(gte(iw,ih),1280,720))':'min(ih,if(gte(iw,ih),720,1280))':force_original_aspect_ratio=decrease:force_divisible_by=2,fps=30,format=nv12";
        let filter = if hdr {
            format!("zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable,zscale=t=bt709:m=bt709:r=tv,{scale}")
        } else {
            scale.into()
        };
        cmd.args([
            "-vf",
            &filter,
            "-c:v",
            if cfg!(windows) {
                "h264_mf"
            } else {
                "h264_videotoolbox"
            },
            "-b:v",
            "2200k",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-colorspace",
            "bt709",
        ]);
    }
    if tracks
        .iter()
        .filter(|t| t["codec_type"] == "audio")
        .all(|a| {
            a["codec_name"] == "aac"
                && matches!(a["profile"].as_str(), Some("LC" | "1"))
                && a["channels"].as_u64().unwrap_or(9) <= 2
        })
    {
        cmd.args(["-c:a", "copy"]);
    } else {
        cmd.args(["-c:a", "aac", "-ac", "2", "-ar", "48000", "-b:a", "128k"]);
    }
    cmd.args(["-movflags", "+faststart", "out.mp4"]);
    run(cmd, cancel, max_bytes)
}
fn run(mut cmd: Command, cancel: &AtomicBool, max_bytes: u64) -> Result<(), String> {
    cmd.stderr(Stdio::null()).stdin(Stdio::null());
    let dir = cmd.get_current_dir().ok_or("VIDEO_TEMP")?.to_path_buf();
    let mut child = cmd.spawn().map_err(|_| "VIDEO_RUNTIME")?;
    let start = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed)
            || start.elapsed() > Duration::from_secs(300)
            || fs::metadata(dir.join("out.mp4")).is_ok_and(|m| m.len() > max_bytes)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("VIDEO_CANCELLED_OR_LIMIT".into());
        }
        if let Some(status) = child.try_wait().map_err(|_| "VIDEO_WORKER")? {
            return if status.success() {
                Ok(())
            } else {
                Err("VIDEO_CODEC".into())
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_conversion_takes_the_limit_from_its_begin_request() {
        let owner = ("origin".to_string(), "session".to_string());
        for max_bytes in [2, 8] {
            let call = |payload| {
                operate(
                    PathBuf::new(),
                    owner.clone(),
                    serde_json::from_value(payload).unwrap(),
                )
            };
            let id = call(json!({"op":"begin", "maxVideoBytes":max_bytes})).unwrap();
            let slot = JOB.get().unwrap().lock().unwrap();
            assert_eq!(slot.as_ref().unwrap().max_bytes, max_bytes);
            drop(slot);
            call(json!({"op":"release", "id":id})).unwrap();
        }
    }

    #[test]
    fn only_chat_compatible_h264_profiles_can_be_copied() {
        let stream = json!({
            "codec_name": "h264",
            "pix_fmt": "yuv420p",
            "avg_frame_rate": "30/1",
            "level": 41,
        });
        assert!(!can_copy_video(&stream, 1280, 720, false));
        for profile in [
            "Baseline",
            "Constrained Baseline",
            "Main",
            "High",
            "66",
            "578",
            "77",
            "100",
        ] {
            let mut stream = stream.clone();
            stream["profile"] = json!(profile);
            assert!(can_copy_video(&stream, 1280, 720, false), "{profile}");
        }
        for profile in ["Extended", "High 10", "High 4:2:2", "unknown", "88", "110"] {
            let mut stream = stream.clone();
            stream["profile"] = json!(profile);
            assert!(!can_copy_video(&stream, 1280, 720, false), "{profile}");
        }
    }
}
