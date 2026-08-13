//! Local audio capture: microphone + system audio, mixed into one WAV.
//!
//! Platform backends:
//! - macOS: ScreenCaptureKit for system audio (16 kHz mono), cpal for mic.
//! - Windows: WASAPI loopback for system audio, cpal for mic.
//!
//! Every capture session runs inside a dedicated thread that owns the audio
//! streams (cpal streams and SCStream are not `Send`), reports startup
//! success/failure back to the caller, then continuously mixes both sources
//! into a 16 kHz mono 16-bit WAV file. A source that stops delivering samples
//! is treated as silence so a single failing device cannot stall recording.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

use crate::meeting::audio::mix_samples;

pub(crate) const CAPTURE_CHUNK_SAMPLES: usize = 1600; // 100 ms @ 16 kHz

/// Creates only the immediate audio output directory after native capture has
/// started successfully. `create_dir` deliberately refuses to recreate a
/// missing user-selected storage root.
pub(crate) fn prepare_audio_output(audio_path: &std::path::Path) -> Result<(), CaptureError> {
    let parent = audio_path
        .parent()
        .ok_or_else(|| CaptureError::start_failed("录音保存路径无效"))?;
    if parent.is_dir() {
        return Ok(());
    }
    std::fs::create_dir(parent)
        .map_err(|error| CaptureError::start_failed(format!("无法创建录音文件夹: {error}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureErrorKind {
    PermissionDenied,
    NoInputDevice,
    CaptureStartFailed,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureError {
    pub(crate) kind: CaptureErrorKind,
    pub(crate) message: String,
    /// Which permission is missing when kind == PermissionDenied.
    #[allow(dead_code)]
    pub(crate) missing_permission: Option<&'static str>,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl CaptureError {
    pub(crate) fn permission(permission: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: CaptureErrorKind::PermissionDenied,
            message: message.into(),
            missing_permission: Some(permission),
        }
    }

    pub(crate) fn start_failed(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureErrorKind::CaptureStartFailed,
            message: message.into(),
            missing_permission: None,
        }
    }

    pub(crate) fn no_input(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureErrorKind::NoInputDevice,
            message: message.into(),
            missing_permission: None,
        }
    }
}

/// Live status shared between the capture session and the rest of the app.
#[derive(Debug)]
pub(crate) struct CaptureShared {
    pub(crate) stop: AtomicBool,
    pub(crate) paused: AtomicBool,
    pub(crate) mic_live: AtomicBool,
    pub(crate) system_live: AtomicBool,
    pub(crate) recorded_samples: AtomicU64,
    discard_pending: AtomicBool,
    write_gate: Mutex<()>,
}

impl CaptureShared {
    pub(crate) fn new(_started_millis: u64) -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            mic_live: AtomicBool::new(false),
            system_live: AtomicBool::new(false),
            recorded_samples: AtomicU64::new(0),
            discard_pending: AtomicBool::new(false),
            write_gate: Mutex::new(()),
        })
    }

    pub(crate) fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub(crate) fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        if !paused {
            // Let the writer discard every pre-pause buffered chunk before
            // callbacks are admitted again. This also makes rapid
            // pause/resume clicks deterministic.
            for _ in 0..100 {
                if !self.discard_pending.load(Ordering::SeqCst) || self.should_stop() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        // Serialize the state transition with WAV writes so no in-flight
        // chunk can extend the recording after pause() returns.
        let _write_guard = self.write_gate.lock().expect("capture write gate poisoned");
        self.paused.store(paused, Ordering::SeqCst);
        if paused {
            self.discard_pending.store(true, Ordering::SeqCst);
            self.mic_live.store(false, Ordering::SeqCst);
            self.system_live.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub(crate) fn accepts_audio(&self) -> bool {
        !self.should_stop() && !self.is_paused()
    }

    pub(crate) fn take_discard_pending(&self) -> bool {
        self.discard_pending.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn record_samples(&self, count: usize) {
        self.recorded_samples
            .fetch_add(count as u64, Ordering::SeqCst);
    }

    pub(crate) fn elapsed_millis(&self) -> u64 {
        self.recorded_samples.load(Ordering::SeqCst) * 1_000
            / u64::from(crate::meeting::audio::TARGET_SAMPLE_RATE)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LiveCaptureStatus {
    pub(crate) mic_active: bool,
    pub(crate) system_audio_active: bool,
    pub(crate) elapsed_ms: u64,
    pub(crate) paused: bool,
}

impl LiveCaptureStatus {
    pub(crate) fn from_shared(shared: &CaptureShared) -> Self {
        Self {
            mic_active: shared.mic_live.load(Ordering::SeqCst),
            system_audio_active: shared.system_live.load(Ordering::SeqCst),
            elapsed_ms: shared.elapsed_millis(),
            paused: shared.is_paused(),
        }
    }
}

/// A running recording session. The session thread owns all audio streams;
/// `stop()` requests shutdown, joins the session and finalizes the WAV file.
pub(crate) struct Recorder {
    pub(crate) shared: Arc<CaptureShared>,
    pub(crate) session: Option<JoinHandle<()>>,
    pub(crate) audio_path: PathBuf,
}

impl Recorder {
    pub(crate) fn pause(&self) {
        self.shared.set_paused(true);
    }

    pub(crate) fn resume(&self) {
        self.shared.set_paused(false);
    }

    /// Stop capture, drain remaining samples, finalize the WAV file.
    /// Returns the finalized sample count.
    pub(crate) fn stop(mut self) -> Result<u64, String> {
        self.shared.request_stop();
        if let Some(session) = self.session.take() {
            let _ = session.join();
        }
        if !self.audio_path.is_file() {
            return Err("录音文件在录音期间被删除，无法完成收尾".to_string());
        }
        crate::meeting::audio::repair_wav_header(&self.audio_path)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Safety net: never leave capture running if the recorder is dropped
        // without an explicit stop.
        self.shared.request_stop();
    }
}

/// Start a recording session writing to `audio_path`.
pub(crate) fn start_recording(
    audio_path: PathBuf,
    started_millis: u64,
) -> Result<Recorder, CaptureError> {
    #[cfg(target_os = "macos")]
    {
        crate::meeting::capture_macos::start_macos_capture(audio_path, started_millis)
    }
    #[cfg(target_os = "windows")]
    {
        crate::meeting::capture_windows::start_windows_capture(audio_path, started_millis)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (audio_path, started_millis);
        Err(CaptureError::start_failed(
            "会议录音仅支持 macOS 和 Windows",
        ))
    }
}

/// Check capture permissions. Returns (microphone, system_audio) granted.
pub(crate) fn check_capture_permissions() -> Result<(bool, bool), String> {
    #[cfg(target_os = "macos")]
    {
        crate::meeting::capture_macos::check_mac_capture_permissions()
    }
    #[cfg(target_os = "windows")]
    {
        crate::meeting::capture_windows::check_windows_capture_permissions()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("会议录音仅支持 macOS 和 Windows".to_string())
    }
}

/// Convert raw PCM bytes into f32 mono samples.
/// `bytes_per_sample` is inferred by the caller (4 = f32, 2 = i16, 8 = f64).
pub(crate) fn pcm_bytes_to_f32_mono(bytes: &[u8], bytes_per_sample: usize) -> Vec<f32> {
    match bytes_per_sample {
        4 => bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect(),
        2 => bytes
            .chunks_exact(2)
            .map(|chunk| f32::from(i16::from_le_bytes(chunk.try_into().unwrap())) / 32768.0)
            .collect(),
        8 => bytes
            .chunks_exact(8)
            .map(|chunk| f64::from_le_bytes(chunk.try_into().unwrap()) as f32)
            .collect(),
        _ => Vec::new(),
    }
}

/// Downmix interleaved multi-channel f32 samples to mono.
pub(crate) fn downmix_f32(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 || samples.is_empty() {
        return samples.to_vec();
    }
    let frames = samples.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    for frame in 0..frames {
        let mut sum = 0.0f32;
        for channel in 0..channels {
            sum += samples[frame * channels + channel];
        }
        mono.push(sum / channels as f32);
    }
    mono
}

/// Simple linear resampler to the target 16 kHz rate.
pub(crate) fn resample_to_target(samples: &[f32], source_rate: u32) -> Vec<f32> {
    use crate::meeting::audio::TARGET_SAMPLE_RATE;
    if samples.is_empty() || source_rate == 0 || source_rate == TARGET_SAMPLE_RATE {
        return samples.to_vec();
    }
    let ratio = f64::from(source_rate) / f64::from(TARGET_SAMPLE_RATE);
    let target_len = (samples.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(target_len);
    for index in 0..target_len {
        let pos = index as f64 * ratio;
        let left = pos.floor() as usize;
        let right = (left + 1).min(samples.len() - 1);
        let frac = (pos - left as f64) as f32;
        out.push(samples[left] * (1.0 - frac) + samples[right] * frac);
    }
    out
}

/// Mix two f32 chunk buffers into i16 samples (public for the backends).
pub(crate) fn mix_chunks(mic: &[f32], system: &[f32]) -> Vec<i16> {
    mix_samples(mic, system)
}

pub(crate) fn write_mixed_samples(
    shared: &CaptureShared,
    writer: &mut crate::meeting::audio::WavWriter,
    samples: &[i16],
) -> Result<(), String> {
    let _write_guard = shared
        .write_gate
        .lock()
        .expect("capture write gate poisoned");
    if shared.is_paused() {
        return Ok(());
    }
    writer.write_samples(samples)?;
    shared.record_samples(samples.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        downmix_f32, pcm_bytes_to_f32_mono, prepare_audio_output, resample_to_target,
        CaptureShared, Recorder,
    };

    #[test]
    fn pcm_i16_bytes_decode() {
        let out = pcm_bytes_to_f32_mono(&0i16.to_le_bytes(), 2);
        assert_eq!(out, vec![0.0]);
        let out = pcm_bytes_to_f32_mono(&16384i16.to_le_bytes(), 2);
        assert!((out[0] - 0.5).abs() < 0.001);
    }

    #[test]
    fn downmix_averages_channels() {
        let stereo = vec![1.0f32, 0.0, 0.5, 0.5];
        let mono = downmix_f32(&stereo, 2);
        assert_eq!(mono, vec![0.5, 0.5]);
    }

    #[test]
    fn elapsed_time_counts_only_recorded_samples() {
        let shared = CaptureShared::new(123);
        shared.record_samples(16_000);
        assert_eq!(shared.elapsed_millis(), 1_000);
        shared.set_paused(true);
        assert!(shared.is_paused());
        assert!(!shared.accepts_audio());
        assert!(shared.take_discard_pending());
        assert!(!shared.take_discard_pending());
        assert_eq!(shared.elapsed_millis(), 1_000);
    }

    #[test]
    fn resample_passthrough_at_target_rate() {
        let samples = vec![0.1f32, 0.2, 0.3];
        assert_eq!(resample_to_target(&samples, 16_000), samples);
        assert_eq!(resample_to_target(&samples, 0), samples);
    }

    #[test]
    fn resample_downscales() {
        let samples: Vec<f32> = (0..48000).map(|i| (i % 100) as f32).collect();
        let out = resample_to_target(&samples, 48_000);
        assert_eq!(out.len(), 16_000);
    }

    #[test]
    fn audio_output_directory_is_created_only_when_capture_prepares_it() {
        let root = std::env::temp_dir().join(format!(
            "snack-capture-output-{}",
            crate::meeting::state::unix_millis()
        ));
        let selected_root = root.join("selected");
        let audio_directory = selected_root.join("audio");
        std::fs::create_dir_all(&selected_root).unwrap();
        let audio_path = audio_directory.join("recording.wav");

        assert!(!audio_directory.exists());
        prepare_audio_output(&audio_path).unwrap();
        assert!(audio_directory.is_dir());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audio_output_does_not_recreate_a_missing_selected_root() {
        let root = std::env::temp_dir().join(format!(
            "snack-capture-missing-root-{}",
            crate::meeting::state::unix_millis()
        ));
        let audio_path = root.join("selected").join("audio").join("recording.wav");

        assert!(prepare_audio_output(&audio_path).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn recorder_reports_when_audio_is_deleted_during_capture() {
        let audio_path = std::env::temp_dir().join(format!(
            "snack-deleted-recording-{}.wav",
            crate::meeting::state::unix_millis()
        ));
        let recorder = Recorder {
            shared: CaptureShared::new(0),
            session: None,
            audio_path,
        };

        assert_eq!(
            recorder.stop().unwrap_err(),
            "录音文件在录音期间被删除，无法完成收尾"
        );
    }
}
