//! WAV I/O, PCM conversion and mixing helpers for local recording.
//!
//! Recordings are stored as 16 kHz mono 16-bit PCM WAV files — the native
//! format for whisper.cpp inference. Capture sources (microphone, system
//! audio) are resampled and mixed into this single track.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

pub(crate) const TARGET_SAMPLE_RATE: u32 = 16_000;
pub(crate) const TARGET_CHANNELS: u16 = 1;
const WAV_HEADER_BYTES: usize = 44;
const MIN_SIGNAL_AMPLITUDE: f32 = 0.001;
const MIN_SIGNAL_RMS: f32 = 0.0001;
const MIN_ACTIVE_SAMPLE_RATIO: f32 = 0.0025;

// ---------------------------------------------------------------------------
// WAV writing
// ---------------------------------------------------------------------------

/// Streaming 16-bit mono WAV writer. The header is written with a placeholder
/// size and patched on `finalize`; if the process dies mid-recording the file
/// can still be repaired with [`repair_wav_header`].
pub(crate) struct WavWriter {
    file: File,
    #[allow(dead_code)]
    path: std::path::PathBuf,
    sample_count: u64,
    finalized: bool,
}

impl WavWriter {
    pub(crate) fn create(path: &Path) -> Result<Self, String> {
        let mut file = File::create(path).map_err(|error| error.to_string())?;
        write_wav_header(&mut file, 0).map_err(|error| error.to_string())?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            sample_count: 0,
            finalized: false,
        })
    }

    /// Append interleaved i16 samples.
    pub(crate) fn write_samples(&mut self, samples: &[i16]) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.file
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
        self.sample_count += samples.len() as u64;
        Ok(())
    }

    /// Patch the header with real sizes and flush. Idempotent.
    pub(crate) fn finalize(&mut self) -> Result<u64, String> {
        if self.finalized {
            return Ok(self.sample_count);
        }
        let data_bytes = self.sample_count * 2;
        self.file.flush().map_err(|error| error.to_string())?;
        write_wav_header(&mut self.file, data_bytes).map_err(|error| error.to_string())?;
        self.file.flush().map_err(|error| error.to_string())?;
        self.finalized = true;
        Ok(self.sample_count)
    }
}

fn write_wav_header(file: &mut File, data_bytes: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    let byte_rate = TARGET_SAMPLE_RATE * u32::from(TARGET_CHANNELS) * 2;
    let block_align = TARGET_CHANNELS * 2;
    let mut header = Vec::with_capacity(WAV_HEADER_BYTES);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&((36u64 + data_bytes).min(u32::MAX as u64) as u32).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes()); // PCM
    header.extend_from_slice(&TARGET_CHANNELS.to_le_bytes());
    header.extend_from_slice(&TARGET_SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(data_bytes.min(u32::MAX as u64) as u32).to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)
}

/// Repair a WAV file whose header is stale (e.g. app crashed mid-recording).
/// Rewrites RIFF/data sizes based on the actual file length, then validates
/// that the file decodes. Returns the sample count.
pub(crate) fn repair_wav_header(path: &Path) -> Result<u64, String> {
    let len = fs::metadata(path).map_err(|error| error.to_string())?.len();
    if len < WAV_HEADER_BYTES as u64 {
        return Err("audio file is too small to be a recording".to_string());
    }
    let data_bytes = len - WAV_HEADER_BYTES as u64;
    if data_bytes % 2 != 0 {
        return Err("audio file has a truncated sample".to_string());
    }
    let mut file = File::options()
        .write(true)
        .read(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    write_wav_header(&mut file, data_bytes).map_err(|error| error.to_string())?;
    validate_wav(path)?;
    Ok(data_bytes / 2)
}

// ---------------------------------------------------------------------------
// WAV reading
// ---------------------------------------------------------------------------

/// Read a 16 kHz mono 16-bit WAV into f32 samples in [-1, 1].
pub(crate) fn read_wav_samples(path: &Path) -> Result<Vec<f32>, String> {
    let (samples_i16, _) = read_wav_i16(path)?;
    Ok(samples_i16
        .into_iter()
        .map(|sample| f32::from(sample) / 32768.0)
        .collect())
}

/// Return whether a sample window contains enough non-trivial energy to be
/// useful for speech recognition. This prevents local ASR engines from
/// hallucinating text for silent recordings while keeping the WAV untouched.
pub(crate) fn has_audio_signal(samples: &[f32]) -> bool {
    if samples.is_empty() {
        return false;
    }
    let active_samples = samples
        .iter()
        .filter(|sample| sample.abs() >= MIN_SIGNAL_AMPLITUDE)
        .count();
    let minimum_active = ((samples.len() as f32 * MIN_ACTIVE_SAMPLE_RATIO).ceil() as usize).max(1);
    let rms =
        (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt();
    active_samples >= minimum_active && rms >= MIN_SIGNAL_RMS
}

pub(crate) fn read_wav_i16(path: &Path) -> Result<(Vec<i16>, u64), String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    decode_wav_i16(&bytes)
}

/// Decode a WAV file (supports PCM 16/24/32-bit, float 32/64, mono/stereo,
/// any sample rate) into 16 kHz mono i16 samples.
pub(crate) fn decode_wav_i16(bytes: &[u8]) -> Result<(Vec<i16>, u64), String> {
    if bytes.len() < WAV_HEADER_BYTES || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".to_string());
    }

    let mut offset = 12usize;
    let mut audio_format = 1u16;
    let mut channels = 1u16;
    let mut sample_rate = 16_000u32;
    let mut bits_per_sample = 16u16;
    let mut data: &[u8] = &[];

    while offset + 8 <= bytes.len() {
        let chunk_id = &bytes[offset..offset + 4];
        let chunk_size = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| "invalid chunk size".to_string())?,
        ) as usize;
        let body_start = offset + 8;
        let body_end = (body_start + chunk_size).min(bytes.len());
        match chunk_id {
            b"fmt " if body_end >= body_start + 16 => {
                audio_format = u16::from_le_bytes(
                    bytes[body_start..body_start + 2]
                        .try_into()
                        .map_err(|_| "invalid format".to_string())?,
                );
                channels = u16::from_le_bytes(
                    bytes[body_start + 2..body_start + 4]
                        .try_into()
                        .map_err(|_| "invalid channels".to_string())?,
                );
                sample_rate = u32::from_le_bytes(
                    bytes[body_start + 4..body_start + 8]
                        .try_into()
                        .map_err(|_| "invalid sample rate".to_string())?,
                );
                bits_per_sample = u16::from_le_bytes(
                    bytes[body_start + 14..body_start + 16]
                        .try_into()
                        .map_err(|_| "invalid bit depth".to_string())?,
                );
            }
            b"data" => {
                data = &bytes[body_start..body_end];
            }
            _ => {}
        }
        offset = body_start + chunk_size + (chunk_size % 2);
    }

    if data.is_empty() {
        return Err("wav file has no data chunk".to_string());
    }
    if channels == 0 || sample_rate == 0 {
        return Err("wav file has invalid format".to_string());
    }

    let samples = match audio_format {
        1 => decode_pcm(data, channels, bits_per_sample)?,
        3 => decode_float(data, channels, bits_per_sample)?,
        other => return Err(format!("unsupported wav audio format {other}")),
    };
    let duration_ms = samples.len() as u64 * 1000 / u64::from(sample_rate);
    Ok((
        resample_to_16k_mono(&samples, sample_rate, channels),
        duration_ms,
    ))
}

fn decode_pcm(data: &[u8], channels: u16, bits: u16) -> Result<Vec<f32>, String> {
    let bytes_per_sample = usize::from(bits / 8);
    if bytes_per_sample == 0 {
        return Err("invalid pcm bit depth".to_string());
    }
    let frame_count = data.len() / (bytes_per_sample * usize::from(channels));
    let mut out = Vec::with_capacity(frame_count * usize::from(channels));
    for frame in 0..frame_count {
        for channel in 0..usize::from(channels) {
            let start = (frame * usize::from(channels) + channel) * bytes_per_sample;
            let sample_bytes = &data[start..start + bytes_per_sample];
            let sample = match bits {
                16 => i16::from_le_bytes(sample_bytes.try_into().unwrap()) as f32 / 32768.0,
                24 => {
                    let value = i32::from_le_bytes([
                        sample_bytes[0],
                        sample_bytes[1],
                        sample_bytes[2],
                        if sample_bytes[2] & 0x80 != 0 { 0xFF } else { 0 },
                    ]);
                    value as f32 / 8_388_608.0
                }
                32 => i32::from_le_bytes(sample_bytes.try_into().unwrap()) as f32 / 2_147_483_648.0,
                _ => return Err("unsupported pcm bit depth".to_string()),
            };
            out.push(sample);
        }
    }
    Ok(out)
}

fn decode_float(data: &[u8], channels: u16, bits: u16) -> Result<Vec<f32>, String> {
    let bytes_per_sample = usize::from(bits / 8);
    let frame_count = data.len() / (bytes_per_sample * usize::from(channels));
    let mut out = Vec::with_capacity(frame_count * usize::from(channels));
    for frame in 0..frame_count {
        for channel in 0..usize::from(channels) {
            let start = (frame * usize::from(channels) + channel) * bytes_per_sample;
            let sample_bytes = &data[start..start + bytes_per_sample];
            let sample = match bits {
                32 => f32::from_le_bytes(sample_bytes.try_into().unwrap()),
                64 => f64::from_le_bytes(sample_bytes.try_into().unwrap()) as f32,
                _ => return Err("unsupported float bit depth".to_string()),
            };
            out.push(sample.clamp(-1.0, 1.0));
        }
    }
    Ok(out)
}

/// Downmix interleaved channels to mono and resample to 16 kHz using linear
/// interpolation.
fn resample_to_16k_mono(samples: &[f32], source_rate: u32, channels: u16) -> Vec<i16> {
    if samples.is_empty() {
        return Vec::new();
    }
    let channels = usize::from(channels.max(1));
    let frame_count = samples.len() / channels;
    // Downmix to mono first (average channels per frame).
    let mut mono = Vec::with_capacity(frame_count);
    for frame in 0..frame_count {
        let mut sum = 0.0f32;
        for channel in 0..channels {
            sum += samples[frame * channels + channel];
        }
        mono.push(sum / channels as f32);
    }
    let ratio = source_rate as f64 / f64::from(TARGET_SAMPLE_RATE);
    let target_len = (mono.len() as f64 / ratio.max(0.001)) as usize;
    let mut out = Vec::with_capacity(target_len);
    for index in 0..target_len {
        let source_pos = index as f64 * ratio;
        let left = source_pos.floor() as usize;
        let right = (left + 1).min(mono.len() - 1);
        let frac = source_pos - left as f64;
        let sample = mono[left] * (1.0 - frac) as f32 + mono[right] * frac as f32;
        out.push((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16);
    }
    out
}

fn validate_wav(path: &Path) -> Result<(), String> {
    read_wav_i16(path).map(|_| ())
}

/// Compute the SHA-256 hex digest of a file (streaming).
pub(crate) fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Check whether there is at least `required` free bytes at `path`.
pub(crate) fn available_bytes(path: &Path) -> Result<u64, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let stat = fs::metadata(path).map_err(|error| error.to_string())?;
        // st_blocks * 512 approximates allocated size; use it to skip the syscall.
        let _ = stat.blocks();
    }
    let available = fs4::available_space(path).map_err(|error| error.to_string())?;
    Ok(available)
}

pub(crate) fn disk_has_room(path: &Path, required_bytes: u64) -> Result<bool, String> {
    Ok(available_bytes(path)? >= required_bytes)
}

// ---------------------------------------------------------------------------
// Mixing
// ---------------------------------------------------------------------------

/// Mix two mono f32 streams sample-by-sample with soft clipping.
pub(crate) fn mix_samples(mic: &[f32], system: &[f32]) -> Vec<i16> {
    let len = mic.len().max(system.len());
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        let a = mic.get(index).copied().unwrap_or(0.0);
        let b = system.get(index).copied().unwrap_or(0.0);
        out.push((soft_clip(a + b) * 32767.0) as i16);
    }
    out
}

fn soft_clip(sample: f32) -> f32 {
    sample.tanh()
}

#[cfg(test)]
mod tests {
    use super::{
        decode_wav_i16, disk_has_room, has_audio_signal, mix_samples, repair_wav_header,
        sha256_file, WavWriter, TARGET_SAMPLE_RATE,
    };
    use std::fs;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("snack-meeting-test-{name}"))
    }

    #[test]
    fn audio_signal_rejects_silence_and_short_spikes() {
        let silence = vec![0.0f32; TARGET_SAMPLE_RATE as usize * 30];
        let mut spike = silence.clone();
        spike[100] = 0.8;
        assert!(!has_audio_signal(&silence));
        assert!(!has_audio_signal(&spike));
    }

    #[test]
    fn audio_signal_accepts_quiet_speech_like_energy() {
        let samples = (0..TARGET_SAMPLE_RATE as usize)
            .map(|index| {
                let time = index as f32 / TARGET_SAMPLE_RATE as f32;
                (time * 220.0 * std::f32::consts::TAU).sin() * 0.005
            })
            .collect::<Vec<_>>();
        assert!(has_audio_signal(&samples));
    }

    #[test]
    fn wav_writer_roundtrip_and_repair() {
        let path = temp_path("roundtrip.wav");
        let _ = fs::remove_file(&path);
        let mut writer = WavWriter::create(&path).unwrap();
        let samples: Vec<i16> = (0..16000).map(|i| (i as i16 % 1000) - 500).collect();
        writer.write_samples(&samples).unwrap();
        // Simulate a crash: never call finalize; the header still says 0 bytes.
        drop(writer);

        // Repair must recover the full sample count.
        let count = repair_wav_header(&path).unwrap();
        assert_eq!(count, 16000);

        let (decoded, duration_ms) = decode_wav_i16(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(decoded.len(), 16000);
        assert_eq!(duration_ms, 1000);
        assert_eq!(decoded[0], samples[0]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn wav_writer_finalize_is_idempotent() {
        let path = temp_path("finalize.wav");
        let _ = fs::remove_file(&path);
        let mut writer = WavWriter::create(&path).unwrap();
        writer.write_samples(&[1, 2, 3]).unwrap();
        assert_eq!(writer.finalize().unwrap(), 3);
        assert_eq!(writer.finalize().unwrap(), 3);
        let (decoded, _) = decode_wav_i16(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(decoded, vec![1, 2, 3]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn decoder_handles_stereo_and_float() {
        // 8 samples stereo i16 → mono 4 samples
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&36u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&TARGET_SAMPLE_RATE.to_le_bytes());
        bytes.extend_from_slice(&(TARGET_SAMPLE_RATE * 4).to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        let data: Vec<u8> = vec![
            0x00, 0x40, 0x00, 0x40, // 16384, 16384
            0x00, 0xC0, 0x00, 0xC0, // -16384, -16384
            0x00, 0x40, 0x00, 0x40, //
            0x00, 0xC0, 0x00, 0xC0, //
        ];
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);
        let (decoded, _) = decode_wav_i16(&bytes).unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[0], 16384);
        assert_eq!(decoded[1], -16384);
    }

    #[test]
    fn mix_pads_shorter_stream() {
        let mic = vec![0.5f32; 4];
        let system = vec![-0.5f32; 2];
        let mixed = mix_samples(&mic, &system);
        assert_eq!(mixed.len(), 4);
        assert!(mixed[0] < 100); // 0.5 + (-0.5) ≈ 0
        assert!(mixed[2].abs() > 1000); // 0.5 + 0
    }

    #[test]
    fn sha256_file_detects_changes() {
        let path = temp_path("hash.bin");
        fs::write(&path, b"hello").unwrap();
        let first = sha256_file(&path).unwrap();
        fs::write(&path, b"hello!").unwrap();
        let second = sha256_file(&path).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn disk_room_check_runs() {
        let ok = disk_has_room(std::path::Path::new("/tmp"), 1024).unwrap();
        assert!(ok);
    }

    /// Long-recording write-path stability: 30 minutes of 16 kHz audio must
    /// stream through the WAV writer without issue (equivalent to the PRD's
    /// "连续录音至少30分钟无崩溃" acceptance for the storage path).
    #[test]
    fn wav_writer_handles_thirty_minutes() {
        let path = temp_path("thirty-minutes.wav");
        let _ = fs::remove_file(&path);
        let mut writer = WavWriter::create(&path).unwrap();
        let chunk = vec![0i16; 16_000 * 10]; // 10 s per write
        let total_writes = 30 * 60 / 10; // 30 minutes
        for _ in 0..total_writes {
            writer.write_samples(&chunk).unwrap();
        }
        let sample_count = writer.finalize().unwrap();
        assert_eq!(sample_count, 16_000 * 30 * 60);
        let meta = fs::metadata(&path).unwrap();
        // 16-bit mono: 2 bytes per sample + 44-byte header
        assert_eq!(meta.len(), sample_count * 2 + 44);
        fs::remove_file(&path).ok();
    }
}
