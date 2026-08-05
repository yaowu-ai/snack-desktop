//! macOS capture backend.
//!
//! - System audio: ScreenCaptureKit audio stream (16 kHz mono), which requires
//!   the Screen Recording permission on macOS 12.3+.
//! - Microphone: cpal (CoreAudio), which triggers the standard microphone
//!   permission prompt on first use.
//!
//! The whole session (stream creation + writer loop) runs on one dedicated
//! thread because cpal streams and SCStream are not `Send`.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use screencapturekit::cm::CMSampleBuffer;
use screencapturekit::prelude::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutputType,
};

use crate::meeting::audio::WavWriter;
use crate::meeting::capture::{
    downmix_f32, mix_chunks, pcm_bytes_to_f32_mono, resample_to_target, CaptureError,
    CaptureShared, Recorder, CAPTURE_CHUNK_SAMPLES,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const CHANNEL_CAPACITY: usize = 256;
const START_TIMEOUT: Duration = Duration::from_secs(20);

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
    let (started_tx, started_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let audio_for_thread = audio_path.clone();
    let shared_for_thread = Arc::clone(&shared);
    let session = thread::Builder::new()
        .name("snack-capture-session".to_string())
        .spawn(move || {
            let start_result = (|| -> Result<(Option<cpal::Stream>, SCStream), CaptureError> {
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
                Ok((mic_stream, sc_stream))
            })();
            let _ = started_tx.send(
                start_result
                    .as_ref()
                    .map_err(|error| error.message.clone())
                    .map(|_| ()),
            );
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
        Ok(Err(message)) => {
            shared.request_stop();
            let _ = session.join();
            return Err(CaptureError::start_failed(message));
        }
        Err(_) => {
            shared.request_stop();
            // A platform API may be stuck inside CoreAudio/ScreenCaptureKit.
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
    if data.is_empty() {
        return;
    }
    let mono = downmix_f32(data, channels);
    let resampled = resample_to_target(&mono, sample_rate);
    shared.mic_live.store(true, Ordering::SeqCst);
    let _ = tx.try_send(resampled);
}

fn start_mac_system_audio(
    tx: Sender<Vec<f32>>,
    shared: &Arc<CaptureShared>,
) -> Result<SCStream, CaptureError> {
    let content = std::panic::catch_unwind(SCShareableContent::get)
        .map_err(|_| CaptureError::permission("system_audio", "获取系统音频权限状态失败"))?
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("not allowed") || message.contains("NotAllowed") {
                CaptureError::permission("system_audio", "需要屏幕录制权限才能采集系统音频")
            } else {
                CaptureError::start_failed(format!("无法访问系统音频: {message}"))
            }
        })?;

    let display = content
        .displays()
        .first()
        .ok_or_else(|| CaptureError::start_failed("没有可用的显示器"))?
        .clone();
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build();
    let config = SCStreamConfiguration::new()
        .with_captures_audio(true)
        .with_sample_rate(16_000)
        .with_channel_count(1)
        .with_excludes_current_process_audio(false);

    let mut stream = SCStream::new(&filter, &config);
    let tx_for_handler = tx.clone();
    let shared_for_handler = Arc::clone(shared);
    let handler_id = stream.add_output_handler(
        move |sample: CMSampleBuffer, _output_type| {
            if let Some(samples) = sample_buffer_to_f32_mono(&sample) {
                if !samples.is_empty() {
                    shared_for_handler.system_live.store(true, Ordering::SeqCst);
                    let _ = tx_for_handler.try_send(samples);
                }
            }
        },
        SCStreamOutputType::Audio,
    );
    if handler_id.is_none() {
        return Err(CaptureError::start_failed("无法注册系统音频采集回调"));
    }

    stream
        .start_capture()
        .map_err(|error| CaptureError::start_failed(format!("无法启动系统音频采集: {error}")))?;
    Ok(stream)
}

/// Extract f32 mono samples from an SCK audio sample buffer.
fn sample_buffer_to_f32_mono(sample: &CMSampleBuffer) -> Option<Vec<f32>> {
    let num_samples = usize::try_from(sample.num_samples()).ok()?;
    if num_samples == 0 {
        return None;
    }
    let list = sample.audio_buffer_list()?;
    let mut mono = Vec::with_capacity(num_samples);
    for index in 0..list.num_buffers() {
        let buffer = list.get(index)?;
        let data = buffer.data();
        if data.is_empty() {
            continue;
        }
        let bytes_per_sample = data.len() / num_samples;
        if bytes_per_sample == 0 {
            continue;
        }
        mono.extend(pcm_bytes_to_f32_mono(data, bytes_per_sample));
    }
    if mono.is_empty() {
        return None;
    }
    // With channel_count = 1 the OS delivers deinterleaved mono float32; if
    // more buffers arrive, average them frame-wise.
    let channels = list.num_buffers().max(1);
    if channels > 1 {
        let frames = mono.len() / channels;
        let mut mixed = Vec::with_capacity(frames);
        for frame in 0..frames {
            let mut sum = 0.0f32;
            for channel in 0..channels {
                sum += mono[frame * channels + channel];
            }
            mixed.push(sum / channels as f32);
        }
        Some(mixed)
    } else {
        Some(mono)
    }
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

    let drain_and_mix =
        |mic_buffer: &mut Vec<f32>, sys_buffer: &mut Vec<f32>, writer: &mut WavWriter| -> bool {
            while mic_buffer.len() >= CAPTURE_CHUNK_SAMPLES
                || sys_buffer.len() >= CAPTURE_CHUNK_SAMPLES
            {
                let mic_take = mic_buffer.len().min(CAPTURE_CHUNK_SAMPLES);
                let mic: Vec<f32> = mic_buffer.drain(..mic_take).collect();
                let sys_take = sys_buffer.len().min(CAPTURE_CHUNK_SAMPLES);
                let sys: Vec<f32> = sys_buffer.drain(..sys_take).collect();
                let mixed = mix_chunks(&mic, &sys);
                if writer.write_samples(&mixed).is_err() {
                    return false;
                }
            }
            true
        };

    while !shared.should_stop() {
        let mut drained_any = false;
        while let Ok(chunk) = mic_rx.try_recv() {
            mic_buffer.extend_from_slice(&chunk);
            drained_any = true;
        }
        while let Ok(chunk) = sys_rx.try_recv() {
            sys_buffer.extend_from_slice(&chunk);
            drained_any = true;
        }
        if !drained_any {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        if !drain_and_mix(&mut mic_buffer, &mut sys_buffer, &mut writer) {
            return;
        }
    }

    // Drain any remaining samples after stop.
    while let Ok(chunk) = mic_rx.try_recv() {
        mic_buffer.extend_from_slice(&chunk);
    }
    while let Ok(chunk) = sys_rx.try_recv() {
        sys_buffer.extend_from_slice(&chunk);
    }
    while mic_buffer.len() >= 1 || sys_buffer.len() >= 1 {
        let mic_take = mic_buffer.len().min(CAPTURE_CHUNK_SAMPLES);
        let mic: Vec<f32> = mic_buffer.drain(..mic_take).collect();
        let sys_take = sys_buffer.len().min(CAPTURE_CHUNK_SAMPLES);
        let sys: Vec<f32> = sys_buffer.drain(..sys_take).collect();
        let mixed = mix_chunks(&mic, &sys);
        if writer.write_samples(&mixed).is_err() {
            break;
        }
    }
    let _ = writer.finalize();
}
