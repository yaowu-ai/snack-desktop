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
use std::sync::Arc;
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
    pub(crate) mic_live: AtomicBool,
    pub(crate) system_live: AtomicBool,
    pub(crate) started_millis: AtomicU64,
}

impl CaptureShared {
    pub(crate) fn new(started_millis: u64) -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            mic_live: AtomicBool::new(false),
            system_live: AtomicBool::new(false),
            started_millis: AtomicU64::new(started_millis),
        })
    }

    pub(crate) fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub(crate) fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub(crate) fn elapsed_millis(&self, now: u64) -> u64 {
        now.saturating_sub(self.started_millis.load(Ordering::SeqCst))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LiveCaptureStatus {
    pub(crate) mic_active: bool,
    pub(crate) system_audio_active: bool,
    pub(crate) elapsed_ms: u64,
}

impl LiveCaptureStatus {
    pub(crate) fn from_shared(shared: &CaptureShared, now: u64) -> Self {
        Self {
            mic_active: shared.mic_live.load(Ordering::SeqCst),
            system_audio_active: shared.system_live.load(Ordering::SeqCst),
            elapsed_ms: shared.elapsed_millis(now),
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
    /// Stop capture, drain remaining samples, finalize the WAV file.
    /// Returns the finalized sample count.
    pub(crate) fn stop(mut self) -> Result<u64, String> {
        self.shared.request_stop();
        if let Some(session) = self.session.take() {
            let _ = session.join();
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

#[cfg(test)]
mod tests {
    use super::{downmix_f32, pcm_bytes_to_f32_mono, prepare_audio_output, resample_to_target};

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
}
