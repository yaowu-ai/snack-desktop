//! Windows capture backend.
//!
//! - System audio: WASAPI loopback capture of the default render endpoint
//!   (no special permission required).
//! - Microphone: cpal (WASAPI), which uses the standard Windows microphone
//!   privacy permission.
//!
//! The whole session (mic stream creation + writer loop) runs on one dedicated
//! thread because cpal streams are not `Send`; the loopback capture runs on
//! its own thread with COM initialized per-thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::meeting::audio::{mix_samples, WavWriter};
use crate::meeting::capture::{
    downmix_f32, prepare_audio_output, resample_to_target, write_mixed_samples, CaptureError,
    CaptureShared, Recorder, CAPTURE_CHUNK_SAMPLES,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const CHANNEL_CAPACITY: usize = 256;
const SESSION_START_TIMEOUT: Duration = Duration::from_secs(30);
const LOOPBACK_START_TIMEOUT: Duration = Duration::from_secs(20);
const MICROPHONE_CALLBACK_TIMEOUT: Duration = Duration::from_secs(5);
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Probe capture permissions. WASAPI loopback needs no permission; the
/// microphone is probed by attempting to open the default input device.
pub(crate) fn check_windows_capture_permissions() -> Result<(bool, bool), String> {
    let microphone = windows_mic_probe();
    Ok((microphone, true))
}

fn windows_mic_probe() -> bool {
    let host = cpal::default_host();
    match host.default_input_device() {
        Some(device) => device.default_input_config().is_ok(),
        None => false,
    }
}

/// Start the Windows capture session.
pub(crate) fn start_windows_capture(
    audio_path: PathBuf,
    started_millis: u64,
) -> Result<Recorder, CaptureError> {
    let shared = CaptureShared::new(started_millis);
    let (mic_tx, mic_rx) = bounded::<Vec<f32>>(CHANNEL_CAPACITY);
    let (sys_tx, sys_rx) = bounded::<Vec<f32>>(CHANNEL_CAPACITY);
    let (started_tx, started_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (mic_ready_tx, mic_ready_rx) = std::sync::mpsc::sync_channel::<()>(1);

    let audio_for_thread = audio_path.clone();
    let shared_for_thread = Arc::clone(&shared);
    let session = thread::Builder::new()
        .name("snack-capture-session".to_string())
        .spawn(move || {
            let start_result = (|| -> Result<WindowsCaptureSession, CaptureError> {
                let mic_stream =
                    start_windows_mic(mic_tx.clone(), mic_ready_tx, &shared_for_thread)?;
                let loopback_thread = start_windows_loopback(sys_tx.clone(), &shared_for_thread)?;
                prepare_audio_output(&audio_for_thread)?;
                let writer = WavWriter::create(&audio_for_thread).map_err(|error| {
                    CaptureError::start_failed(format!("无法创建录音文件: {error}"))
                })?;
                mic_ready_rx
                    .recv_timeout(MICROPHONE_CALLBACK_TIMEOUT)
                    .map_err(|_| {
                        CaptureError::start_failed("麦克风未返回音频数据，请检查设备和隐私权限")
                    })?;
                Ok(WindowsCaptureSession {
                    mic_stream,
                    loopback_thread,
                    writer,
                })
            })();
            let _ = started_tx.send(
                start_result
                    .as_ref()
                    .map_err(|error| error.message.clone())
                    .map(|_| ()),
            );
            match start_result {
                Ok(WindowsCaptureSession {
                    mic_stream: _mic_stream,
                    loopback_thread,
                    mut writer,
                }) => {
                    // Keep the cpal stream alive until the writer has drained
                    // every callback. Dropping it terminates WASAPI capture.
                    run_writer(&mut writer, &shared_for_thread, mic_rx, sys_rx);
                    shared_for_thread.request_stop();
                    let _ = loopback_thread.join();
                }
                Err(error) => {
                    eprintln!("snack meeting capture start failed: {error}");
                }
            }
        })
        .map_err(|error| CaptureError::start_failed(format!("无法创建采集线程: {error}")))?;

    match started_rx.recv_timeout(SESSION_START_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            shared.request_stop();
            let _ = session.join();
            return Err(CaptureError::start_failed(message));
        }
        Err(_) => {
            shared.request_stop();
            let _ = session.join();
            return Err(CaptureError::start_failed("录音启动超时"));
        }
    }

    Ok(Recorder {
        shared,
        session: Some(session),
        audio_path,
    })
}

struct WindowsCaptureSession {
    mic_stream: cpal::Stream,
    loopback_thread: thread::JoinHandle<()>,
    writer: WavWriter,
}

fn start_windows_mic(
    tx: Sender<Vec<f32>>,
    ready: std::sync::mpsc::SyncSender<()>,
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
        cpal::SampleFormat::F32 => {
            let ready = ready.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| {
                    push_mic_chunk(
                        data,
                        channels,
                        sample_rate,
                        &tx_for_callback,
                        &ready,
                        &shared_for_callback,
                    )
                },
                move |error| eprintln!("snack meeting mic error: {error}"),
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let ready = ready.clone();
            device.build_input_stream(
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
                        &ready,
                        &shared_for_callback,
                    )
                },
                move |error| eprintln!("snack meeting mic error: {error}"),
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let ready = ready.clone();
            device.build_input_stream(
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
                        &ready,
                        &shared_for_callback,
                    )
                },
                move |error| eprintln!("snack meeting mic error: {error}"),
                None,
            )
        }
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
    ready: &std::sync::mpsc::SyncSender<()>,
    shared: &CaptureShared,
) {
    if data.is_empty() || !shared.accepts_audio() {
        return;
    }
    let mono = downmix_f32(data, channels);
    let resampled = resample_to_target(&mono, sample_rate);
    shared
        .mic_live
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = ready.try_send(());
    let _ = tx.try_send(resampled);
}

struct WaveFormatInfo {
    channels: usize,
    sample_rate: u32,
    bits_per_sample: usize,
    is_float: bool,
}

impl WaveFormatInfo {
    fn from_ptr(format: *const WAVEFORMATEX) -> Result<Self, CaptureError> {
        if format.is_null() {
            return Err(CaptureError::start_failed("系统音频格式无效"));
        }
        // WAVEFORMATEX is packed(1); copy it to avoid unaligned access.
        // SAFETY: the OS returned a valid WAVEFORMATEX via GetMixFormat.
        let raw = format;
        let format = unsafe { std::ptr::read_unaligned(raw) };
        let (is_float, bits) = if format.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            // SAFETY: extensible formats carry a WAVEFORMATEXTENSIBLE header.
            let extensible = unsafe { &*(raw as *const WAVEFORMATEXTENSIBLE) };
            // SAFETY: SubFormat is the last field of a packed struct; read it
            // unaligned through addr_of!.
            let sub_format = unsafe { std::ptr::addr_of!(extensible.SubFormat).read_unaligned() };
            let is_float = sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
            (is_float, usize::from(format.wBitsPerSample))
        } else {
            (
                format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT,
                usize::from(format.wBitsPerSample),
            )
        };
        Ok(Self {
            channels: usize::from(format.nChannels).max(1),
            sample_rate: format.nSamplesPerSec,
            bits_per_sample: bits.max(16),
            is_float,
        })
    }
}

/// Convert a loopback buffer (device mix format) into f32 samples in
/// 16 kHz mono.
fn convert_loopback_buffer(data: &[u8], info: &WaveFormatInfo) -> Vec<f32> {
    let bytes_per_sample = info.bits_per_sample / 8;
    if bytes_per_sample == 0 || data.is_empty() {
        return Vec::new();
    }
    let sample_count = data.len() / bytes_per_sample;
    let mut interleaved = Vec::with_capacity(sample_count);
    for index in 0..sample_count {
        let start = index * bytes_per_sample;
        let bytes = &data[start..start + bytes_per_sample];
        let sample = match (info.is_float, bytes_per_sample) {
            (true, 4) => f32::from_le_bytes(bytes.try_into().unwrap()),
            (true, 8) => f64::from_le_bytes(bytes.try_into().unwrap()) as f32,
            (false, 2) => f32::from(i16::from_le_bytes(bytes.try_into().unwrap())) / 32768.0,
            (false, 4) => i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2_147_483_648.0,
            _ => 0.0,
        };
        interleaved.push(sample.clamp(-1.0, 1.0));
    }
    let mono = downmix_f32(&interleaved, info.channels);
    resample_to_target(&mono, info.sample_rate)
}

fn convert_loopback_silence(frames: u32, sample_rate: u32) -> Vec<f32> {
    resample_to_target(&vec![0.0; frames as usize], sample_rate)
}

/// WASAPI loopback capture thread for system audio. Returns the thread handle;
/// the thread exits when the session's shared stop flag is set.
fn start_windows_loopback(
    tx: Sender<Vec<f32>>,
    shared: &Arc<CaptureShared>,
) -> Result<thread::JoinHandle<()>, CaptureError> {
    let shared_for_thread = Arc::clone(shared);
    let shared_for_failure = Arc::clone(shared);
    let (started_tx, started_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let loopback_thread = thread::Builder::new()
        .name("snack-loopback".to_string())
        .spawn(move || {
            // SAFETY: single-threaded COM use inside this thread.
            let com_result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
            if let Err(error) = com_result {
                let message = format!("无法初始化系统音频线程: {error}");
                let _ = started_tx.send(Err(message.clone()));
                eprintln!("snack meeting CoInitializeEx failed: {message}");
                return;
            }

            let result = run_loopback_capture(&tx, &shared_for_thread, &started_tx);
            if let Err(error) = result {
                let _ = started_tx.send(Err(error.message.clone()));
                eprintln!("snack meeting loopback failed: {error}");
            }
            unsafe {
                let _ = CoUninitialize();
            }
        })
        .map_err(|error| CaptureError::start_failed(format!("无法创建系统音频线程: {error}")))?;

    match started_rx.recv_timeout(LOOPBACK_START_TIMEOUT) {
        Ok(Ok(())) => Ok(loopback_thread),
        Ok(Err(message)) => {
            shared_for_failure.request_stop();
            let _ = loopback_thread.join();
            Err(CaptureError::start_failed(message))
        }
        Err(_) => {
            shared_for_failure.request_stop();
            drop(loopback_thread);
            Err(CaptureError::start_failed("系统音频启动超时"))
        }
    }
}

fn run_loopback_capture(
    tx: &Sender<Vec<f32>>,
    shared: &CaptureShared,
    started: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), CaptureError> {
    // SAFETY: raw COM usage with checked HRESULTs.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|error| {
                CaptureError::start_failed(format!("无法初始化系统音频: {error}"))
            })?;

        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|error| {
                CaptureError::start_failed(format!("无法获取默认音频设备: {error}"))
            })?;

        let client: IAudioClient = device
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(|error| CaptureError::start_failed(format!("无法激活音频客户端: {error}")))?;

        let mix_format = client
            .GetMixFormat()
            .map_err(|error| CaptureError::start_failed(format!("无法读取音频格式: {error}")))?;
        let format_info = WaveFormatInfo::from_ptr(mix_format)?;

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                mix_format,
                None,
            )
            .map_err(|error| CaptureError::start_failed(format!("无法初始化系统音频: {error}")))?;

        let capture: IAudioCaptureClient =
            client
                .GetService::<IAudioCaptureClient>()
                .map_err(|error| {
                    CaptureError::start_failed(format!("无法获取音频采集接口: {error}"))
                })?;

        let event = CreateEventW(None, false, false, None)
            .map_err(|error| CaptureError::start_failed(format!("无法创建音频事件: {error}")))?;
        let event_handle: HANDLE = event;
        client
            .SetEventHandle(event_handle)
            .map_err(|error| CaptureError::start_failed(format!("无法绑定音频事件: {error}")))?;

        client.Start().map_err(|error| {
            CaptureError::start_failed(format!("无法启动系统音频采集: {error}"))
        })?;
        let _ = started.send(Ok(()));

        let result = (|| -> Result<(), CaptureError> {
            while !shared.should_stop() {
                WaitForSingleObject(event_handle, 200);
                let mut next_packet: u32 = capture.GetNextPacketSize().map_err(|error| {
                    CaptureError::start_failed(format!("系统音频读取失败: {error}"))
                })?;
                while next_packet > 0 {
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .map_err(|error| {
                            CaptureError::start_failed(format!("系统音频读取失败: {error}"))
                        })?;
                    let samples = if frames == 0 {
                        Vec::new()
                    } else if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                        convert_loopback_silence(frames, format_info.sample_rate)
                    } else if !data.is_null() {
                        let bytes = std::slice::from_raw_parts(
                            data,
                            frames as usize * format_info.bits_per_sample / 8
                                * format_info.channels,
                        );
                        convert_loopback_buffer(bytes, &format_info)
                    } else {
                        Vec::new()
                    };
                    if !samples.is_empty() && shared.accepts_audio() {
                        shared
                            .system_live
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = tx.try_send(samples);
                    }
                    capture.ReleaseBuffer(frames).map_err(|error| {
                        CaptureError::start_failed(format!("系统音频释放失败: {error}"))
                    })?;
                    next_packet = capture.GetNextPacketSize().map_err(|error| {
                        CaptureError::start_failed(format!("系统音频读取失败: {error}"))
                    })?;
                }
            }
            Ok(())
        })();

        let _ = client.Stop();
        let _ = CloseHandle(event_handle);
        result
    }
}

/// Writer loop: drain mic + system chunks, mix, append to WAV until stopped.
fn run_writer(
    writer: &mut WavWriter,
    shared: &CaptureShared,
    mic_rx: Receiver<Vec<f32>>,
    sys_rx: Receiver<Vec<f32>>,
) {
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
                let mixed = mix_samples(&mic, &sys);
                if write_mixed_samples(shared, writer, &mixed).is_err() {
                    return false;
                }
            }
            true
        };

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
            thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        if !drain_and_mix(&mut mic_buffer, &mut sys_buffer, writer) {
            return;
        }
    }

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
        let mixed = mix_samples(&mic, &sys);
        if write_mixed_samples(shared, writer, &mixed).is_err() {
            break;
        }
    }
    if let Err(error) = writer.finalize() {
        eprintln!("snack meeting failed to finalize wav: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        convert_loopback_buffer, convert_loopback_silence, run_writer, WaveFormatInfo,
        KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    };
    use crate::meeting::audio::{read_wav_i16, WavWriter};
    use crate::meeting::capture::CaptureShared;
    use crossbeam_channel::bounded;
    use std::fs;

    #[test]
    fn loopback_float_stereo_converts_to_mono() {
        let info = WaveFormatInfo {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            is_float: true,
        };
        // 4 frames stereo: left 1.0, right -1.0
        let mut bytes = Vec::new();
        for _ in 0..4 {
            bytes.extend_from_slice(&1.0f32.to_le_bytes());
            bytes.extend_from_slice(&(-1.0f32).to_le_bytes());
        }
        let out = convert_loopback_buffer(&bytes, &info);
        assert!(out.len() <= 4);
        assert!(out.iter().all(|sample| sample.abs() < 0.01));
    }

    #[test]
    fn loopback_i16_mono_converts() {
        let info = WaveFormatInfo {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            is_float: false,
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&16384i16.to_le_bytes());
        bytes.extend_from_slice(&(-16384i16).to_le_bytes());
        let out = convert_loopback_buffer(&bytes, &info);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < 0.01);
        assert!((out[1] + 0.5).abs() < 0.01);
    }

    #[test]
    fn silent_loopback_packet_preserves_duration() {
        let out = convert_loopback_silence(4_800, 48_000);
        assert_eq!(out.len(), 1_600);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn writer_persists_mic_only_audio_duration() {
        let path = std::env::temp_dir().join(format!(
            "snack-windows-mic-only-{}-{}.wav",
            std::process::id(),
            crate::meeting::state::unix_millis()
        ));
        let mut writer = WavWriter::create(&path).unwrap();
        let shared = CaptureShared::new(0);
        let (mic_tx, mic_rx) = bounded(2);
        let (_sys_tx, sys_rx) = bounded(2);
        mic_tx.send(vec![0.25; 16_000]).unwrap();
        shared.request_stop();

        run_writer(&mut writer, &shared, mic_rx, sys_rx);
        drop(writer);

        let (samples, duration_ms) = read_wav_i16(&path).unwrap();
        assert_eq!(samples.len(), 16_000);
        assert_eq!(duration_ms, 1_000);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn subtype_guids_match_expected() {
        assert_eq!(
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71)
        );
    }
}
