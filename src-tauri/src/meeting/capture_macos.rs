//! macOS capture backend.
//!
//! - System audio: Core Audio process tap (macOS 14.2+), resampled to 16 kHz mono.
//!   Uses system-audio-only authorization, without Screen Recording permission.
//! - Microphone: cpal (CoreAudio), which triggers the standard microphone
//!   permission prompt on first use.
//!
//! The whole session (stream creation + writer loop) runs on one dedicated
//! thread because cpal streams and the native tap are not `Send`.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::meeting::audio::WavWriter;
use crate::meeting::capture::{
    downmix_f32, mix_timeline_chunk, prepare_audio_output, resample_to_target, write_mixed_samples,
    CaptureError, CaptureShared, Recorder, CAPTURE_CHUNK_SAMPLES,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const CHANNEL_CAPACITY: usize = 256;
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// Probe whether macOS capture permissions are granted.
/// Returns (microphone, system_audio).
pub(crate) fn check_mac_capture_permissions() -> Result<(bool, bool), String> {
    Ok(crate::meeting::permissions::check_mac_permissions())
}

/// Start the macOS capture session on a dedicated thread.
pub(crate) fn start_macos_capture(
    audio_path: PathBuf,
    started_millis: u64,
) -> Result<Recorder, CaptureError> {
    let shared = CaptureShared::new(started_millis);
    let (mic_tx, mic_rx) = bounded::<Vec<f32>>(CHANNEL_CAPACITY);
    let (sys_tx, sys_rx) = bounded::<Vec<f32>>(CHANNEL_CAPACITY);
    let (started_tx, started_rx) = std::sync::mpsc::channel::<Result<(), CaptureError>>();

    let audio_for_thread = audio_path.clone();
    let shared_for_thread = Arc::clone(&shared);
    let session = thread::Builder::new()
        .name("snack-capture-session".to_string())
        .spawn(move || {
            let start_result = (|| -> Result<(Option<cpal::Stream>, SystemAudioTap), CaptureError> {
                let mic_stream = if crate::meeting::permissions::check_mac_permissions().0 {
                    match start_mac_mic(mic_tx.clone(), &shared_for_thread) {
                        Ok(stream) => Some(stream),
                        Err(error) => {
                            eprintln!("snack meeting microphone unavailable, continuing with system audio: {error}");
                            None
                        }
                    }
                } else {
                    eprintln!("snack meeting microphone permission unavailable, continuing with system audio");
                    None
                };
                let sc_stream = start_mac_system_audio(sys_tx.clone(), &shared_for_thread)?;
                prepare_audio_output(&audio_for_thread)?;
                Ok((mic_stream, sc_stream))
            })();
            if start_result.is_ok() {
                shared_for_thread.reset_elapsed_timer();
            }
            let _ = started_tx.send(start_result.as_ref().map(|_| ()).map_err(|error| error.clone()));
            match start_result {
                Ok((_mic_stream, _sc_stream)) => {
                    // Keep both native streams alive for the entire writer
                    // loop. Dropping either one stops that capture source.
                    run_writer(audio_for_thread, &shared_for_thread, mic_rx, sys_rx)
                }
                Err(error) => {
                    eprintln!("snack meeting capture start failed: {error}");
                }
            }
        })
        .map_err(|error| CaptureError::start_failed(format!("无法创建采集线程: {error}")))?;

    match started_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            shared.request_stop();
            let _ = session.join();
            return Err(error);
        }
        Err(_) => {
            shared.request_stop();
            // A platform API may be stuck inside CoreAudio.
            // Dropping the handle detaches that thread so the command can
            // return the timeout instead of blocking the UI indefinitely.
            drop(session);
            return Err(CaptureError::start_failed("录音启动超时"));
        }
    }
    Ok(Recorder {
        shared,
        session: Some(session),
        audio_path,
    })
}

fn start_mac_mic(
    tx: Sender<Vec<f32>>,
    shared: &Arc<CaptureShared>,
) -> Result<cpal::Stream, CaptureError> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| CaptureError::no_input("没有找到可用的麦克风设备"))?;
    let config = device
        .default_input_config()
        .map_err(|error| CaptureError::start_failed(format!("无法读取麦克风配置: {error}")))?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.clone().into();

    let tx_for_callback = tx.clone();
    let shared_for_callback = Arc::clone(shared);
    let build_result = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _| {
                push_mic_chunk(
                    data,
                    channels,
                    sample_rate,
                    &tx_for_callback,
                    &shared_for_callback,
                )
            },
            move |error| eprintln!("snack meeting mic error: {error}"),
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _| {
                let converted: Vec<f32> = data
                    .iter()
                    .map(|sample| f32::from(*sample) / 32768.0)
                    .collect();
                push_mic_chunk(
                    &converted,
                    channels,
                    sample_rate,
                    &tx_for_callback,
                    &shared_for_callback,
                )
            },
            move |error| eprintln!("snack meeting mic error: {error}"),
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _| {
                let converted: Vec<f32> = data
                    .iter()
                    .map(|sample| (f32::from(*sample) / 32767.0) * 2.0 - 1.0)
                    .collect();
                push_mic_chunk(
                    &converted,
                    channels,
                    sample_rate,
                    &tx_for_callback,
                    &shared_for_callback,
                )
            },
            move |error| eprintln!("snack meeting mic error: {error}"),
            None,
        ),
        other => {
            return Err(CaptureError::start_failed(format!(
                "不支持的麦克风采样格式: {other:?}"
            )))
        }
    };

    let stream = build_result
        .map_err(|error| CaptureError::start_failed(format!("无法启动麦克风采集: {error}")))?;
    stream
        .play()
        .map_err(|error| CaptureError::start_failed(format!("无法启动麦克风采集: {error}")))?;
    Ok(stream)
}

fn push_mic_chunk(
    data: &[f32],
    channels: usize,
    sample_rate: u32,
    tx: &Sender<Vec<f32>>,
    shared: &CaptureShared,
) {
    if data.is_empty() || !shared.accepts_audio() {
        return;
    }
    let mono = downmix_f32(data, channels);
    let resampled = resample_to_target(&mono, sample_rate);
    shared.mic_live.store(true, Ordering::SeqCst);
    let _ = tx.try_send(resampled);
}

struct TapContext {
    tx: Sender<Vec<f32>>,
    shared: Arc<CaptureShared>,
}

struct SystemAudioTap {
    handle: std::ptr::NonNull<std::ffi::c_void>,
    // Must outlive all callbacks. Drop stops the IOProc before releasing this.
    _context: Box<TapContext>,
}

extern "C" {
    fn snack_audio_tap_start(
        callback: extern "C" fn(*const f32, usize, f64, *mut std::ffi::c_void),
        context: *mut std::ffi::c_void,
        error: *mut i32,
        stage: *mut u32,
    ) -> *mut std::ffi::c_void;
    fn snack_audio_tap_stop(handle: *mut std::ffi::c_void);
}

impl Drop for SystemAudioTap {
    fn drop(&mut self) {
        // CoreAudio stops and unregisters the callback before context is freed.
        unsafe { snack_audio_tap_stop(self.handle.as_ptr()) };
    }
}

extern "C" fn receive_system_audio(
    samples: *const f32,
    count: usize,
    sample_rate: f64,
    context: *mut std::ffi::c_void,
) {
    // Never unwind across the native audio callback boundary.
    let _ = std::panic::catch_unwind(|| {
        if samples.is_null() || context.is_null() || count == 0 {
            return;
        }
        let context = unsafe { &*(context as *const TapContext) };
        if !context.shared.accepts_audio() {
            return;
        }
        let data = unsafe { std::slice::from_raw_parts(samples, count) };
        let mono = resample_to_target(data, sample_rate.round() as u32);
        context.shared.system_live.store(true, Ordering::SeqCst);
        let _ = context.tx.try_send(mono);
    });
}

fn start_mac_system_audio(
    tx: Sender<Vec<f32>>,
    shared: &Arc<CaptureShared>,
) -> Result<SystemAudioTap, CaptureError> {
    let mut context = Box::new(TapContext {
        tx,
        shared: Arc::clone(shared),
    });
    let mut error = 0;
    let mut stage = 0;
    let handle = unsafe {
        snack_audio_tap_start(
            receive_system_audio,
            (&mut *context as *mut TapContext).cast(),
            &mut error,
            &mut stage,
        )
    };
    let handle = std::ptr::NonNull::new(handle).ok_or_else(|| {
        eprintln!("snack Core Audio tap failed: stage={stage} status={error}");
        if stage == 0 {
            CaptureError::start_failed("系统音频录制需要 macOS 14.2 或更新版本")
        } else {
            CaptureError::start_failed("无法启动系统音频录制。如已拒绝授权，请在系统设置中允许 Snack 的“仅系统录音”后重试。")
        }
    })?;
    Ok(SystemAudioTap {
        handle,
        _context: context,
    })
}

/// Writer loop: drain mic + system chunks, mix, append to WAV until stopped.
fn run_writer(
    audio_path: PathBuf,
    shared: &CaptureShared,
    mic_rx: Receiver<Vec<f32>>,
    sys_rx: Receiver<Vec<f32>>,
) {
    let mut writer = match WavWriter::create(&audio_path) {
        Ok(writer) => writer,
        Err(error) => {
            eprintln!("snack meeting failed to create wav: {error}");
            return;
        }
    };
    let mut mic_buffer: Vec<f32> = Vec::new();
    let mut sys_buffer: Vec<f32> = Vec::new();

    while !shared.should_stop() {
        if shared.take_discard_pending() {
            mic_buffer.clear();
            sys_buffer.clear();
            while mic_rx.try_recv().is_ok() {}
            while sys_rx.try_recv().is_ok() {}
        }
        if shared.is_paused() {
            mic_buffer.clear();
            sys_buffer.clear();
        }
        while let Ok(chunk) = mic_rx.try_recv() {
            mic_buffer.extend_from_slice(&chunk);
        }
        while let Ok(chunk) = sys_rx.try_recv() {
            sys_buffer.extend_from_slice(&chunk);
        }
        // Keep one chunk of latency so asynchronous mic/system callbacks for
        // the same time window can arrive before it is mixed.
        while !shared.is_paused() && shared.samples_due() >= (CAPTURE_CHUNK_SAMPLES * 2) as u64 {
            let mixed = mix_timeline_chunk(&mut mic_buffer, &mut sys_buffer, CAPTURE_CHUNK_SAMPLES);
            if write_mixed_samples(shared, &mut writer, &mixed).is_err() {
                return;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }

    // Drain any remaining samples after stop.
    while let Ok(chunk) = mic_rx.try_recv() {
        mic_buffer.extend_from_slice(&chunk);
    }
    while let Ok(chunk) = sys_rx.try_recv() {
        sys_buffer.extend_from_slice(&chunk);
    }
    while !shared.is_paused() && shared.samples_due() > 0 {
        let sample_count = shared.samples_due().min(CAPTURE_CHUNK_SAMPLES as u64) as usize;
        let mixed = mix_timeline_chunk(&mut mic_buffer, &mut sys_buffer, sample_count);
        if write_mixed_samples(shared, &mut writer, &mixed).is_err() {
            break;
        }
    }
    let _ = writer.finalize();
}

#[cfg(test)]
mod tap_tests {
    use super::*;

    #[test]
    fn tap_callback_resamples_into_existing_mix_queue() {
        let (tx, rx) = bounded(2);
        let shared = CaptureShared::new(0);
        let mut context = TapContext {
            tx,
            shared: shared.clone(),
        };
        let samples = vec![0.25f32; 480];
        receive_system_audio(
            samples.as_ptr(),
            samples.len(),
            48_000.0,
            (&mut context as *mut TapContext).cast(),
        );
        let chunk = rx.try_recv().unwrap();
        assert_eq!(chunk.len(), 160);
        assert!(chunk.iter().all(|sample| (*sample - 0.25).abs() < 0.001));
        assert!(shared.system_live.load(Ordering::SeqCst));
    }

    #[test]
    fn stopped_capture_discards_late_tap_callback() {
        let (tx, rx) = bounded(2);
        let shared = CaptureShared::new(0);
        shared.request_stop();
        let mut context = TapContext {
            tx,
            shared: shared.clone(),
        };
        receive_system_audio(
            [0.5f32; 160].as_ptr(),
            160,
            16_000.0,
            (&mut context as *mut TapContext).cast(),
        );
        assert!(rx.try_recv().is_err());
        assert!(!shared.system_live.load(Ordering::SeqCst));
    }

    #[test]
    fn full_tap_queue_drops_chunk_without_blocking_audio_thread() {
        let (tx, rx) = bounded(1);
        tx.try_send(vec![0.1]).unwrap();
        let mut context = TapContext {
            tx,
            shared: CaptureShared::new(0),
        };
        receive_system_audio(
            [0.5f32; 160].as_ptr(),
            160,
            16_000.0,
            (&mut context as *mut TapContext).cast(),
        );
        assert_eq!(rx.try_recv().unwrap(), vec![0.1]);
        assert!(rx.try_recv().is_err());
    }
}
