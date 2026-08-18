//! Local FunASR transcription backed by Snack's fixed ModelScope snapshot.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::meeting::catalog::ModelKey;
use crate::meeting::state::TranscriptSegment;

const FUNASR_TRANSCRIBER: &str = include_str!("funasr_transcribe.py");
const TRANSCRIPTION_STALLED: &str = "本地转写长时间停留在 95%";
const TRANSCRIPTION_STALL_GRACE: Duration = Duration::from_secs(120);
const AUDIO_NORMALIZATION_TIMEOUT: Duration = Duration::from_secs(15 * 60);
static SCRIPT_UPDATE_COUNTER: AtomicU64 = AtomicU64::new(0);
static NORMALIZED_AUDIO_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) enum TranscriptionOutcome {
    NoAudioDetected,
    Detected {
        segments: Vec<TranscriptSegment>,
        text: String,
        language: String,
    },
}

pub(crate) struct TranscriptionRequest<'a> {
    pub(crate) model_key: ModelKey,
    pub(crate) model_dir: &'a Path,
    pub(crate) wav_path: &'a Path,
    pub(crate) language: &'a str,
}

pub(crate) struct TranscriptionProgress {
    pub(crate) percent: u8,
    pub(crate) remaining_seconds: Option<u64>,
    pub(crate) current_text: String,
    pub(crate) segment_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelScopeSegment {
    start_ms: u64,
    end_ms: u64,
    text: String,
    speaker: String,
}

#[derive(Debug, Deserialize)]
struct ModelScopeTranscript {
    text: String,
    #[serde(default)]
    segments: Vec<ModelScopeSegment>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ModelScopeMessage {
    Progress {
        percent: u8,
        #[serde(rename = "remainingSeconds")]
        remaining_seconds: u64,
    },
    Result {
        text: String,
        #[serde(default)]
        segments: Vec<ModelScopeSegment>,
    },
}

/// Transcribe one local audio file and stream task-scoped progress updates.
pub(crate) fn transcribe_file(
    request: TranscriptionRequest<'_>,
    ensure_runtime: impl FnOnce() -> Result<(), String>,
    mut on_progress: impl FnMut(TranscriptionProgress) -> bool + 'static,
) -> Result<TranscriptionOutcome, String> {
    validate_request(&request)?;
    if !on_progress(progress_update(2, None)) {
        return Err("本地转写已停止".to_string());
    }
    if audio_is_silent(&request)? {
        if !on_progress(progress_update(100, Some(0))) {
            return Err("本地转写已停止".to_string());
        }
        return Ok(TranscriptionOutcome::NoAudioDetected);
    }
    ensure_runtime()?;
    let decoded = run_modelscope_with_format_fallback(&request, &mut on_progress)?;
    if !on_progress(completion_update(&decoded)) {
        return Err("本地转写已停止".to_string());
    }
    Ok(transcription_outcome(decoded, request.language))
}

fn run_modelscope_with_format_fallback(
    request: &TranscriptionRequest<'_>,
    on_progress: &mut impl FnMut(TranscriptionProgress) -> bool,
) -> Result<ModelScopeTranscript, String> {
    match run_modelscope(request, on_progress) {
        Err(message) if message == TRANSCRIPTION_STALLED => {
            if !on_progress(format_fallback_update()) {
                return Err("本地转写已停止".to_string());
            }
            let normalized = normalize_audio_for_retry(request, on_progress)?;
            let retry = TranscriptionRequest {
                model_key: request.model_key,
                model_dir: request.model_dir,
                wav_path: normalized.path(),
                language: request.language,
            };
            run_modelscope(&retry, on_progress).map_err(format_retry_error)
        }
        result => result,
    }
}

fn format_retry_error(message: String) -> String {
    if message == TRANSCRIPTION_STALLED {
        "本地转写切换为标准 WAV 后仍未响应，请重新转写".to_string()
    } else {
        format!("本地转写切换为标准 WAV 后失败: {message}")
    }
}

fn validate_request(request: &TranscriptionRequest<'_>) -> Result<(), String> {
    if request.model_key != ModelKey::FunAsr2G {
        return Err("不支持的本地模型".to_string());
    }
    if !request.wav_path.exists() {
        return Err("录音文件缺失，无法转写".to_string());
    }
    Ok(())
}

fn audio_is_silent(request: &TranscriptionRequest<'_>) -> Result<bool, String> {
    if !request
        .wav_path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("wav"))
    {
        return Ok(false);
    }
    let samples = crate::meeting::audio::read_wav_samples(request.wav_path)?;
    Ok(!crate::meeting::audio::has_audio_signal(&samples))
}

fn run_modelscope(
    request: &TranscriptionRequest<'_>,
    on_progress: &mut impl FnMut(TranscriptionProgress) -> bool,
) -> Result<ModelScopeTranscript, String> {
    let (mut child, stderr_thread) = spawn_transcriber(request)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取本地转写进度".to_string())?;
    let protocol_result = consume_protocol(&mut child, BufReader::new(stdout), on_progress);
    let status = child
        .wait()
        .map_err(|error| format!("等待本地 FunASR 转写失败: {error}"))?;
    let stderr = stderr_thread.join().unwrap_or_default();
    let transcript = protocol_result?;
    if !status.success() {
        return Err(transcriber_failure_message(&stderr));
    }
    transcript.ok_or_else(|| "本地转写结果无效：模型未返回结果".to_string())
}

fn spawn_transcriber(
    request: &TranscriptionRequest<'_>,
) -> Result<(Child, JoinHandle<String>), String> {
    let runtime_dir = request.model_dir.join("runtime");
    let python = crate::meeting::python_runtime::venv_python(&runtime_dir);
    let cache_dir = request.model_dir.join("modelscope-cache");
    if !python.exists() || !cache_dir.exists() {
        return Err("本地会议模型运行环境不完整，请重新下载".to_string());
    }
    let script = ensure_transcriber_script(&runtime_dir)?;
    let mut child = Command::new(python)
        .args(["-I", "-X", "utf8"])
        .arg(script)
        .arg(request.wav_path)
        .env("MODELSCOPE_CACHE", cache_dir)
        .env(
            "SNACK_TRANSCRIBER_PARENT_PID",
            std::process::id().to_string(),
        )
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动本地 FunASR 转写: {error}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取本地转写日志".to_string())?;
    Ok((child, drain_stderr(stderr)))
}

fn ensure_transcriber_script(runtime_dir: &Path) -> Result<std::path::PathBuf, String> {
    let digest = format!("{:x}", Sha256::digest(FUNASR_TRANSCRIBER.as_bytes()));
    let script = runtime_dir.join(format!("funasr-transcribe-{}.py", &digest[..12]));
    if std::fs::read(&script).ok().as_deref() == Some(FUNASR_TRANSCRIBER.as_bytes()) {
        return Ok(script);
    }
    let counter = SCRIPT_UPDATE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = runtime_dir.join(format!(
        ".funasr-transcribe-{}-{counter}.tmp",
        std::process::id()
    ));
    std::fs::write(&temporary, FUNASR_TRANSCRIBER)
        .map_err(|error| format!("无法更新本地转写脚本: {error}"))?;
    match std::fs::rename(&temporary, &script) {
        Ok(()) => Ok(script),
        Err(_) if std::fs::read(&script).ok().as_deref() == Some(FUNASR_TRANSCRIBER.as_bytes()) => {
            std::fs::remove_file(&temporary).ok();
            Ok(script)
        }
        Err(error) => {
            std::fs::remove_file(&temporary).ok();
            Err(format!("无法安装本地转写脚本: {error}"))
        }
    }
}

fn consume_protocol(
    child: &mut Child,
    mut reader: impl BufRead,
    on_progress: &mut impl FnMut(TranscriptionProgress) -> bool,
) -> Result<Option<ModelScopeTranscript>, String> {
    let mut transcript = None;
    let mut stall = TranscriptionStall::default();
    while let Some(line) = read_protocol_line(&mut reader)? {
        match decode_message_bytes(&line) {
            Some(ModelScopeMessage::Progress {
                percent,
                remaining_seconds,
            }) => {
                if stall.observe(percent, remaining_seconds, Instant::now()) {
                    child.kill().ok();
                    return Err(TRANSCRIPTION_STALLED.to_string());
                }
                if !on_progress(progress_update(percent.min(99), Some(remaining_seconds))) {
                    child.kill().ok();
                    return Err("本地转写已停止".to_string());
                }
            }
            Some(ModelScopeMessage::Result { text, segments }) => {
                transcript = Some(ModelScopeTranscript { text, segments });
            }
            _ => {}
        }
    }
    Ok(transcript)
}

#[derive(Default)]
struct TranscriptionStall {
    started_at: Option<Instant>,
}

impl TranscriptionStall {
    fn observe(&mut self, percent: u8, remaining_seconds: u64, now: Instant) -> bool {
        if percent < 95 || remaining_seconds > 15 {
            self.started_at = None;
            return false;
        }
        let started_at = self.started_at.get_or_insert(now);
        now.saturating_duration_since(*started_at) >= TRANSCRIPTION_STALL_GRACE
    }
}

fn read_protocol_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut bytes = Vec::new();
    let count = reader
        .read_until(b'\n', &mut bytes)
        .map_err(|error| format!("读取本地转写进度失败: {error}"))?;
    if count == 0 {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn drain_stderr(mut stderr: ChildStderr) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).ok();
        String::from_utf8_lossy(&output).into_owned()
    })
}

fn decode_message_bytes(line: &[u8]) -> Option<ModelScopeMessage> {
    serde_json::from_slice(line).ok()
}

fn diagnostic(stderr: &str) -> String {
    let trimmed = stderr.trim();
    let start = trimmed.len().saturating_sub(4_000);
    trimmed.get(start..).unwrap_or(trimmed).to_string()
}

fn transcriber_failure_message(stderr: &str) -> String {
    let details = diagnostic(stderr);
    if details.is_empty() {
        return "本地 FunASR 转写进程异常退出，未返回诊断信息".to_string();
    }
    format!("本地 FunASR 转写失败: {details}")
}

fn progress_update(percent: u8, remaining_seconds: Option<u64>) -> TranscriptionProgress {
    TranscriptionProgress {
        percent,
        remaining_seconds,
        current_text: String::new(),
        segment_count: 0,
    }
}

fn completion_update(decoded: &ModelScopeTranscript) -> TranscriptionProgress {
    TranscriptionProgress {
        percent: 100,
        remaining_seconds: Some(0),
        current_text: decoded.text.clone(),
        segment_count: decoded.segments.len(),
    }
}

fn format_fallback_update() -> TranscriptionProgress {
    TranscriptionProgress {
        percent: 95,
        remaining_seconds: None,
        current_text: "转写耗时过长，正在切换为标准 WAV 格式重试".to_string(),
        segment_count: 0,
    }
}

struct NormalizedAudio {
    path: PathBuf,
}

impl NormalizedAudio {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for NormalizedAudio {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

fn normalize_audio_for_retry(
    request: &TranscriptionRequest<'_>,
    on_progress: &mut impl FnMut(TranscriptionProgress) -> bool,
) -> Result<NormalizedAudio, String> {
    let path = normalized_audio_path(request.model_dir);
    let result =
        normalize_audio(request, &path, on_progress).and_then(|_| validate_normalized_audio(&path));
    if let Err(message) = result {
        std::fs::remove_file(&path).ok();
        return Err(message);
    }
    Ok(NormalizedAudio { path })
}

fn normalized_audio_path(model_dir: &Path) -> PathBuf {
    let counter = NORMALIZED_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    model_dir.join("runtime").join(format!(
        ".normalized-audio-{}-{counter}.wav",
        std::process::id()
    ))
}

fn normalize_audio(
    request: &TranscriptionRequest<'_>,
    output: &Path,
    on_progress: &mut impl FnMut(TranscriptionProgress) -> bool,
) -> Result<(), String> {
    match crate::meeting::audio::read_wav_i16(request.wav_path) {
        Ok((samples, _)) => write_normalized_wav(output, &samples),
        Err(_) => normalize_audio_with_python(request, output, on_progress),
    }
}

fn write_normalized_wav(output: &Path, samples: &[i16]) -> Result<(), String> {
    let mut writer = crate::meeting::audio::WavWriter::create(output)?;
    writer.write_samples(samples)?;
    writer.finalize().map(drop)
}

fn normalize_audio_with_python(
    request: &TranscriptionRequest<'_>,
    output: &Path,
    on_progress: &mut impl FnMut(TranscriptionProgress) -> bool,
) -> Result<(), String> {
    let runtime_dir = request.model_dir.join("runtime");
    let python = crate::meeting::python_runtime::venv_python(&runtime_dir);
    let script = ensure_transcriber_script(&runtime_dir)?;
    let mut child = Command::new(python)
        .args(["-I", "-X", "utf8"])
        .arg(script)
        .arg("--normalize")
        .arg(request.wav_path)
        .arg(output)
        .env(
            "SNACK_TRANSCRIBER_PARENT_PID",
            std::process::id().to_string(),
        )
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动音频格式转换: {error}"))?;
    let stderr = child
        .stderr
        .take()
        .map(drain_stderr)
        .ok_or_else(|| "无法读取音频格式转换日志".to_string())?;
    wait_for_normalizer(&mut child, stderr, on_progress)
}

fn wait_for_normalizer(
    child: &mut Child,
    stderr: JoinHandle<String>,
    on_progress: &mut impl FnMut(TranscriptionProgress) -> bool,
) -> Result<(), String> {
    let started_at = Instant::now();
    let mut last_progress_at = started_at;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("等待音频格式转换失败: {error}"))?
        {
            let stderr_output = stderr.join().unwrap_or_default();
            return if status.success() {
                Ok(())
            } else {
                Err(format!("音频格式转换失败: {}", diagnostic(&stderr_output)))
            };
        }
        if started_at.elapsed() >= AUDIO_NORMALIZATION_TIMEOUT {
            child.kill().ok();
            child.wait().ok();
            let stderr_output = stderr.join().unwrap_or_default();
            return Err(format!("音频格式转换超时: {}", diagnostic(&stderr_output)));
        }
        if last_progress_at.elapsed() >= Duration::from_secs(1) {
            if !on_progress(format_fallback_update()) {
                child.kill().ok();
                child.wait().ok();
                stderr.join().ok();
                return Err("本地转写已停止".to_string());
            }
            last_progress_at = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn validate_normalized_audio(path: &Path) -> Result<(), String> {
    crate::meeting::audio::read_wav_i16(path)
        .map(drop)
        .map_err(|message| format!("标准 WAV 校验失败: {message}"))
}

fn transcription_outcome(decoded: ModelScopeTranscript, language: &str) -> TranscriptionOutcome {
    let segments = decoded
        .segments
        .into_iter()
        .map(|segment| TranscriptSegment {
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            text: segment.text,
            speaker: segment.speaker,
        })
        .collect::<Vec<_>>();
    if decoded.text.trim().is_empty()
        && segments
            .iter()
            .all(|segment| segment.text.trim().is_empty())
    {
        return TranscriptionOutcome::NoAudioDetected;
    }
    TranscriptionOutcome::Detected {
        segments,
        text: decoded.text,
        language: language.to_string(),
    }
}

/// Validate the complete local runtime and cache without loading the model.
pub(crate) fn validate_model(model_key: ModelKey, model_dir: &Path) -> Result<(), String> {
    if model_key == ModelKey::FunAsr2G
        && crate::meeting::python_runtime::venv_python(&model_dir.join("runtime")).exists()
        && model_dir.join("modelscope-cache").exists()
    {
        Ok(())
    } else {
        Err("本地 FunASR 运行环境不完整".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::audio::WavWriter;
    use std::fs;
    use std::sync::{Arc, Mutex};

    #[test]
    fn silent_recording_finishes_without_loading_the_model_runtime() {
        let wav_path = std::env::temp_dir().join(format!(
            "snack-silent-transcription-{}.wav",
            std::process::id()
        ));
        let _ = fs::remove_file(&wav_path);
        let mut writer = WavWriter::create(&wav_path).unwrap();
        writer.write_samples(&vec![0; 16_000]).unwrap();
        writer.finalize().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_callback = Arc::clone(&observed);
        let outcome = transcribe_file(
            TranscriptionRequest {
                model_key: ModelKey::FunAsr2G,
                model_dir: Path::new("/model-runtime-does-not-exist"),
                wav_path: &wav_path,
                language: "zh",
            },
            || panic!("silent audio must not initialize the Python runtime"),
            move |progress| {
                observed_for_callback
                    .lock()
                    .unwrap()
                    .push((progress.percent, progress.remaining_seconds));
                true
            },
        )
        .unwrap();

        assert!(matches!(outcome, TranscriptionOutcome::NoAudioDetected));
        assert_eq!(*observed.lock().unwrap(), vec![(2, None), (100, Some(0))]);
        fs::remove_file(&wav_path).ok();
    }

    #[test]
    fn progress_protocol_decodes_remaining_seconds() {
        let message = decode_message_bytes(
            r#"{"type":"progress","percent":36,"remainingSeconds":125}"#.as_bytes(),
        )
        .unwrap();

        assert!(matches!(
            message,
            ModelScopeMessage::Progress {
                percent: 36,
                remaining_seconds: 125
            }
        ));
    }

    #[test]
    fn progress_stall_requires_two_minutes_at_ninety_five_percent() {
        let started_at = Instant::now();
        let mut stall = TranscriptionStall::default();

        assert!(!stall.observe(95, 15, started_at));
        assert!(!stall.observe(95, 15, started_at + Duration::from_secs(119)));
        assert!(stall.observe(95, 15, started_at + Duration::from_secs(120)));
        assert!(!stall.observe(94, 16, started_at + Duration::from_secs(121)));
    }

    #[test]
    fn empty_stderr_returns_an_actionable_transcriber_failure() {
        assert_eq!(
            transcriber_failure_message(""),
            "本地 FunASR 转写进程异常退出，未返回诊断信息"
        );
    }

    #[test]
    fn result_protocol_ignores_unrelated_log_lines() {
        assert!(decode_message_bytes(b"funasr version: 1.3.14").is_none());
        let payload = concat!(
            r#"{"type":"result","text":"会议内容","segments":[{"startMs":0,"#,
            r#""endMs":1200,"text":"会议内容","speaker":"说话人 1"}]}"#
        );
        let message = decode_message_bytes(payload.as_bytes()).unwrap();

        assert!(matches!(message, ModelScopeMessage::Result { text, .. } if text == "会议内容"));
    }

    #[test]
    fn protocol_reader_tolerates_non_utf8_windows_logs() {
        let mut output = b"loading model: \xC4\xE3\xBA\xC3\n".to_vec();
        output.extend_from_slice(r#"{"type":"result","text":"会议内容","segments":[]}"#.as_bytes());
        output.extend_from_slice(b"\r\n");
        let mut reader = BufReader::new(output.as_slice());

        let log = read_protocol_line(&mut reader).unwrap().unwrap();
        assert!(decode_message_bytes(&log).is_none());
        let result = read_protocol_line(&mut reader).unwrap().unwrap();
        assert!(matches!(
            decode_message_bytes(&result),
            Some(ModelScopeMessage::Result { text, .. }) if text == "会议内容"
        ));
    }

    #[test]
    fn empty_model_output_is_a_no_audio_result() {
        let outcome = transcription_outcome(
            ModelScopeTranscript {
                text: String::new(),
                segments: Vec::new(),
            },
            "zh",
        );

        assert!(matches!(outcome, TranscriptionOutcome::NoAudioDetected));
    }
}
