//! Native local transcription with FunASR/ONNX or whisper.cpp.
//!
//! Long recordings are processed in bounded chunks (30 s with 2 s overlap) so
//! memory usage stays flat regardless of recording length. Segment timestamps
//! from each chunk are offset into recording time and overlap duplicates are
//! dropped. Speaker labels are relative (说话人 1/2/…) and derived from
//! whisper.cpp's speaker-turn signal plus a silence-gap heuristic; they never
//! claim real identities.

use std::path::Path;
use std::sync::Arc;
use std::thread;

use crate::meeting::audio::{has_audio_signal, read_wav_samples, TARGET_SAMPLE_RATE};
use crate::meeting::catalog::{find_model, ModelKey};
use crate::meeting::state::TranscriptSegment;

pub(crate) const CHUNK_SECONDS: u64 = 30;
pub(crate) const OVERLAP_SECONDS: u64 = 2;
const WHISPER_TIMESTAMP_UNIT_MS: u64 = 10;
const MAX_SPEAKERS: usize = 4;
const SPEAKER_TURN_SILENCE_MS: u64 = 400;
const NO_AUDIO_SIGNAL_ERROR: &str =
    "录音中没有检测到可转写的声音，录音文件已保留，请检查麦克风或系统音频后重试";

#[derive(Debug, Clone)]
pub(crate) struct RawSegment {
    pub(crate) start_ms: u64,
    pub(crate) end_ms: u64,
    pub(crate) text: String,
    pub(crate) speaker_turn: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptionOutcome {
    pub(crate) segments: Vec<TranscriptSegment>,
    pub(crate) text: String,
    #[allow(dead_code)]
    pub(crate) total_samples: usize,
    pub(crate) language: String,
}

pub(crate) struct TranscriptionProgress {
    pub(crate) percent: u8,
    pub(crate) current_text: String,
    pub(crate) segment_count: usize,
}

/// Transcribe with the selected Snack-owned native engine. `model_dir` always
/// points inside Snack's meeting data directory.
pub(crate) fn transcribe_file(
    model_key: ModelKey,
    model_dir: &Path,
    wav_path: &Path,
    language: &str,
    on_progress: impl FnMut(TranscriptionProgress) -> bool + 'static,
) -> Result<TranscriptionOutcome, String> {
    match model_key {
        ModelKey::FunAsr2G => transcribe_funasr(model_dir, wav_path, language, on_progress),
        ModelKey::LargeV3 | ModelKey::Small => {
            let model = find_model(model_key).ok_or_else(|| "未知的模型".to_string())?;
            let model_bytes = std::fs::read(model_dir.join(model.filename))
                .map_err(|error| format!("模型文件读取失败: {error}"))?;
            transcribe_whisper(&model_bytes, wav_path, language, on_progress)
        }
    }
}

/// Transcribe a WAV file with the given model. `on_progress` is invoked
/// periodically; returning false aborts the transcription.
fn transcribe_whisper(
    model_bytes: &[u8],
    wav_path: &Path,
    language: &str,
    on_progress: impl FnMut(TranscriptionProgress) -> bool + 'static,
) -> Result<TranscriptionOutcome, String> {
    let samples = read_wav_samples(wav_path)?;
    if samples.is_empty() {
        return Err("录音文件为空".to_string());
    }
    ensure_audio_signal(&samples)?;

    whisper_rs::install_logging_hooks();
    let context_params = whisper_rs::WhisperContextParameters::default();
    let context =
        whisper_rs::WhisperContext::new_from_buffer_with_params(model_bytes, context_params)
            .map_err(|error| format!("无法加载本地模型: {error}"))?;

    let mut params =
        whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some(language));
    params.set_translate(false);
    params.set_no_timestamps(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    // Each chunk overlaps the previous one, so carrying decoder context is
    // unnecessary and lets hallucinations from a silent chunk poison all
    // following chunks.
    params.set_no_context(true);
    let threads = thread::available_parallelism()
        .map(|value| value.get() as i32)
        .unwrap_or(4);
    params.set_n_threads(threads.min(8));
    params.set_no_speech_thold(0.6);
    params.set_logprob_thold(-1.0);

    let mut state = context
        .create_state()
        .map_err(|error| format!("无法创建转写引擎: {error}"))?;

    let sample_rate = u64::from(TARGET_SAMPLE_RATE);
    let chunk_samples = (CHUNK_SECONDS * sample_rate) as usize;
    let overlap_samples = (OVERLAP_SECONDS * sample_rate) as usize;
    let total_samples = samples.len();

    let progress_fn: Arc<
        std::sync::Mutex<Box<dyn FnMut(TranscriptionProgress) -> bool + 'static>>,
    > = Arc::new(std::sync::Mutex::new(Box::new(on_progress)));

    let mut merged: Vec<RawSegment> = Vec::new();
    let mut offset = 0usize;
    let mut chunk_index = 0usize;

    while offset < total_samples {
        let end = (offset + chunk_samples).min(total_samples);
        let chunk = &samples[offset..end];
        let chunk_start_ms = offset as u64 * 1000 / sample_rate;
        let chunk_end_ms = end as u64 * 1000 / sample_rate;

        if !has_audio_signal(chunk) {
            let percent = ((end as u64 * 100 / total_samples as u64).min(100)) as u8;
            let mut callback = progress_fn.lock().unwrap();
            if !callback(TranscriptionProgress {
                percent,
                current_text: String::new(),
                segment_count: merged.len(),
            }) {
                return Err("本地转写已停止".to_string());
            }
            if end >= total_samples {
                break;
            }
            offset = end.saturating_sub(overlap_samples);
            chunk_index += 1;
            continue;
        }

        // Progress callback: scale chunk progress into overall progress.
        let chunk_span = (end - offset) as f64 / total_samples as f64;
        let chunk_base = offset as f64 / total_samples as f64;
        let progress_state = Arc::new(std::sync::Mutex::new(ProgressState {
            percent: 0u8,
            current_text: String::new(),
            segment_count: 0usize,
            cancelled: false,
        }));

        {
            let progress_state = Arc::clone(&progress_state);
            let progress_fn = Arc::clone(&progress_fn);
            params.set_progress_callback_safe(move |progress| {
                let mut state = progress_state.lock().unwrap();
                state.percent = ((chunk_base + chunk_span * f64::from(progress) / 100.0).min(100.0)
                    * 100.0)
                    .round() as u8;
                let mut callback = progress_fn.lock().unwrap();
                if !callback(TranscriptionProgress {
                    percent: state.percent,
                    current_text: state.current_text.clone(),
                    segment_count: state.segment_count,
                }) {
                    state.cancelled = true;
                }
            });
        }
        {
            let progress_state = Arc::clone(&progress_state);
            let progress_fn = Arc::clone(&progress_fn);
            params.set_segment_callback_safe(move |segment: whisper_rs::SegmentCallbackData| {
                let mut state = progress_state.lock().unwrap();
                state.segment_count += 1;
                state.current_text = segment.text;
                let mut callback = progress_fn.lock().unwrap();
                if !callback(TranscriptionProgress {
                    percent: state.percent,
                    current_text: state.current_text.clone(),
                    segment_count: state.segment_count,
                }) {
                    state.cancelled = true;
                }
            });
        }
        // NOTE: we deliberately do not set whisper's abort callback —
        // whisper-rs 0.14.4's `set_abort_callback_safe` instantiates its
        // trampoline with the wrong type, corrupting closure captures when
        // the callback fires (crash with Metal and capturing closures).
        // Cancellation is not a product requirement.

        state
            .full(params.clone(), chunk)
            .map_err(|error| format!("本地转写失败: {error}"))?;

        let n_segments = state
            .full_n_segments()
            .map_err(|error| format!("读取转写结果失败: {error}"))?;
        for index in 0..n_segments {
            let segment_start = state
                .full_get_segment_t0(index)
                .map_err(|error| format!("读取转写结果失败: {error}"))?;
            let segment_end = state
                .full_get_segment_t1(index)
                .map_err(|error| format!("读取转写结果失败: {error}"))?;
            let text = state
                // Whisper can occasionally emit a partially invalid UTF-8
                // byte sequence for otherwise usable CJK text. Keep the
                // segment and replace only the invalid bytes instead of
                // failing the entire retained local recording.
                .full_get_segment_text_lossy(index)
                .map_err(|error| format!("读取转写结果失败: {error}"))?
                .trim()
                .to_string();
            if text.is_empty() {
                continue;
            }
            let speaker_turn = state.full_get_segment_speaker_turn_next(index);

            // whisper.cpp pads short chunks internally and can report a tail
            // timestamp beyond the samples we supplied. Never expose a
            // segment outside the retained recording's real duration.
            let start_ms =
                (chunk_start_ms + whisper_timestamp_to_ms(segment_start)).min(chunk_end_ms);
            let end_ms = (chunk_start_ms + whisper_timestamp_to_ms(segment_end)).min(chunk_end_ms);

            // Drop segments fully inside the overlap region (duplicates of the
            // previous chunk's tail).
            if chunk_index > 0 && end_ms <= chunk_start_ms + OVERLAP_SECONDS * 1000 {
                continue;
            }
            if start_ms >= end_ms {
                continue;
            }

            merged.push(RawSegment {
                start_ms,
                end_ms,
                text,
                speaker_turn,
            });
        }

        if end >= total_samples {
            break;
        }
        offset = end.saturating_sub(overlap_samples);
        chunk_index += 1;
    }

    // Assign relative speaker labels.
    let segments = label_speakers(merged, &samples);
    if segments.is_empty() {
        return Err("录音中未识别出有效语音，录音文件已保留".to_string());
    }

    Ok(TranscriptionOutcome {
        text: segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join(""),
        segments,
        total_samples,
        language: language.to_string(),
    })
}

fn transcribe_funasr(
    model_dir: &Path,
    wav_path: &Path,
    language: &str,
    mut on_progress: impl FnMut(TranscriptionProgress) -> bool + 'static,
) -> Result<TranscriptionOutcome, String> {
    use sherpa_onnx::{
        OfflineParaformerModelConfig, OfflinePunctuation, OfflinePunctuationConfig,
        OfflineRecognizer, OfflineRecognizerConfig,
    };

    let samples = read_wav_samples(wav_path)?;
    if samples.is_empty() {
        return Err("录音文件为空".to_string());
    }
    ensure_audio_signal(&samples)?;

    let asr_path = model_dir.join("asr-model.onnx");
    let tokens_path = model_dir.join("tokens.txt");
    let punc_path = model_dir.join("punc-model.onnx");
    for path in [&asr_path, &tokens_path, &punc_path] {
        if !path.exists() {
            return Err(format!("模型组件缺失: {}", path.display()));
        }
    }

    let mut config = OfflineRecognizerConfig::default();
    config.model_config.paraformer = OfflineParaformerModelConfig {
        model: Some(asr_path.to_string_lossy().into_owned()),
    };
    config.model_config.tokens = Some(tokens_path.to_string_lossy().into_owned());
    config.model_config.num_threads = thread::available_parallelism()
        .map(|value| value.get() as i32)
        .unwrap_or(4)
        .min(8);
    config.model_config.provider = Some("cpu".to_string());
    config.decoding_method = Some("greedy_search".to_string());
    let recognizer = OfflineRecognizer::create(&config)
        .ok_or_else(|| "无法加载 FunASR Paraformer 本地模型".to_string())?;

    let mut punc_config = OfflinePunctuationConfig::default();
    punc_config.model.ct_transformer = Some(punc_path.to_string_lossy().into_owned());
    punc_config.model.num_threads = 2;
    let punctuator = OfflinePunctuation::create(&punc_config)
        .ok_or_else(|| "无法加载 FunASR 标点模型".to_string())?;

    // Keep peak memory bounded for long meetings. Paraformer produces token
    // timestamps, while chunk boundaries provide a stable fallback.
    let sample_rate = TARGET_SAMPLE_RATE as usize;
    let chunk_samples = CHUNK_SECONDS as usize * sample_rate;
    let total_samples = samples.len();
    let mut raw_segments = Vec::new();
    let mut offset = 0usize;

    while offset < total_samples {
        let end = (offset + chunk_samples).min(total_samples);
        if !has_audio_signal(&samples[offset..end]) {
            let percent = ((end as u64 * 100 / total_samples as u64).min(100)) as u8;
            if !on_progress(TranscriptionProgress {
                percent,
                current_text: String::new(),
                segment_count: raw_segments.len(),
            }) {
                return Err("本地转写已停止".to_string());
            }
            offset = end;
            continue;
        }
        let stream = recognizer.create_stream();
        stream.accept_waveform(TARGET_SAMPLE_RATE as i32, &samples[offset..end]);
        recognizer.decode(&stream);
        let result = stream
            .get_result()
            .ok_or_else(|| "读取 FunASR 转写结果失败".to_string())?;
        let raw_text = result.text.trim();
        if !raw_text.is_empty() {
            let text = punctuator
                .add_punctuation(raw_text)
                .unwrap_or_else(|| raw_text.to_string())
                .trim()
                .to_string();
            let chunk_start_ms = offset as u64 * 1000 / u64::from(TARGET_SAMPLE_RATE);
            let chunk_end_ms = end as u64 * 1000 / u64::from(TARGET_SAMPLE_RATE);
            raw_segments.push(RawSegment {
                start_ms: chunk_start_ms,
                end_ms: chunk_end_ms,
                text: text.clone(),
                speaker_turn: false,
            });
            let percent = ((end as u64 * 100 / total_samples as u64).min(100)) as u8;
            if !on_progress(TranscriptionProgress {
                percent,
                current_text: text,
                segment_count: raw_segments.len(),
            }) {
                return Err("本地转写已停止".to_string());
            }
        }
        offset = end;
    }

    let segments = label_speakers(raw_segments, &samples);
    if segments.is_empty() {
        return Err("录音中未识别出有效语音，录音文件已保留".to_string());
    }
    Ok(TranscriptionOutcome {
        text: segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join(""),
        segments,
        total_samples,
        language: language.to_string(),
    })
}

/// Shared progress bookkeeping used by whisper callbacks.
struct ProgressState {
    percent: u8,
    current_text: String,
    segment_count: usize,
    cancelled: bool,
}

fn ensure_audio_signal(samples: &[f32]) -> Result<(), String> {
    if has_audio_signal(samples) {
        Ok(())
    } else {
        Err(NO_AUDIO_SIGNAL_ERROR.to_string())
    }
}

fn whisper_timestamp_to_ms(timestamp: i64) -> u64 {
    timestamp.max(0) as u64 * WHISPER_TIMESTAMP_UNIT_MS
}

/// Assign relative speaker labels using whisper's speaker-turn signal plus a
/// silence-gap heuristic: a gap of silence longer than
/// [`SPEAKER_TURN_SILENCE_MS`] before a segment suggests a speaker change.
fn label_speakers(segments: Vec<RawSegment>, samples: &[f32]) -> Vec<TranscriptSegment> {
    let sample_rate = u64::from(TARGET_SAMPLE_RATE);
    let mut current_speaker = 0usize;
    let mut labeled = Vec::with_capacity(segments.len());
    let mut previous_end_ms = 0u64;
    let mut is_first = true;

    for segment in segments {
        let silence_gap = if is_first {
            0
        } else {
            segment.start_ms.saturating_sub(previous_end_ms)
        };
        let gap_is_silent = if silence_gap > 0 && silence_gap < 10_000 {
            let start_sample = (previous_end_ms * sample_rate / 1000) as usize;
            let end_sample = (segment.start_ms * sample_rate / 1000) as usize;
            let slice = &samples[start_sample.min(samples.len())..end_sample.min(samples.len())];
            if slice.is_empty() {
                false
            } else {
                let energy: f32 =
                    slice.iter().map(|sample| sample * sample).sum::<f32>() / slice.len() as f32;
                energy < 1e-4
            }
        } else {
            false
        };

        if !is_first
            && (segment.speaker_turn || (gap_is_silent && silence_gap >= SPEAKER_TURN_SILENCE_MS))
            && current_speaker < MAX_SPEAKERS - 1
        {
            current_speaker += 1;
        }

        labeled.push(TranscriptSegment {
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            text: segment.text,
            speaker: format!("说话人 {}", current_speaker + 1),
        });
        previous_end_ms = segment.end_ms;
        is_first = false;
    }
    labeled
}

/// Run an inference self-check: the model must load and run end-to-end on a
/// short generated tone. Used by the install pipeline's `validating` stage.
pub(crate) fn validate_model(model_key: ModelKey, model_dir: &Path) -> Result<(), String> {
    match model_key {
        ModelKey::FunAsr2G => validate_funasr_model(model_dir),
        ModelKey::LargeV3 | ModelKey::Small => {
            let model = find_model(model_key).ok_or_else(|| "未知的模型".to_string())?;
            let model_bytes = std::fs::read(model_dir.join(model.filename))
                .map_err(|error| format!("模型文件读取失败: {error}"))?;
            validate_whisper_model(&model_bytes)
        }
    }
}

fn validate_whisper_model(model_bytes: &[u8]) -> Result<(), String> {
    whisper_rs::install_logging_hooks();
    let mut samples = Vec::with_capacity(TARGET_SAMPLE_RATE as usize);
    for index in 0..TARGET_SAMPLE_RATE as usize {
        let t = index as f32 / TARGET_SAMPLE_RATE as f32;
        samples.push((t * 440.0 * std::f32::consts::TAU).sin() * 0.05);
    }
    let context_params = whisper_rs::WhisperContextParameters::default();
    let context =
        whisper_rs::WhisperContext::new_from_buffer_with_params(model_bytes, context_params)
            .map_err(|error| format!("模型加载失败: {error}"))?;
    let mut params =
        whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("zh"));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_n_threads(2);
    let mut state = context
        .create_state()
        .map_err(|error| format!("引擎创建失败: {error}"))?;
    state
        .full(params, &samples)
        .map_err(|error| format!("推理自检失败: {error}"))?;
    Ok(())
}

fn validate_funasr_model(model_dir: &Path) -> Result<(), String> {
    use sherpa_onnx::{
        OfflineParaformerModelConfig, OfflinePunctuation, OfflinePunctuationConfig,
        OfflineRecognizer, OfflineRecognizerConfig,
    };

    let mut config = OfflineRecognizerConfig::default();
    config.model_config.paraformer = OfflineParaformerModelConfig {
        model: Some(
            model_dir
                .join("asr-model.onnx")
                .to_string_lossy()
                .into_owned(),
        ),
    };
    config.model_config.tokens = Some(model_dir.join("tokens.txt").to_string_lossy().into_owned());
    config.model_config.num_threads = 2;
    config.model_config.provider = Some("cpu".to_string());
    let recognizer =
        OfflineRecognizer::create(&config).ok_or_else(|| "Paraformer 模型加载失败".to_string())?;
    let stream = recognizer.create_stream();
    let tone = (0..TARGET_SAMPLE_RATE as usize)
        .map(|index| {
            let t = index as f32 / TARGET_SAMPLE_RATE as f32;
            (t * 440.0 * std::f32::consts::TAU).sin() * 0.01
        })
        .collect::<Vec<_>>();
    stream.accept_waveform(TARGET_SAMPLE_RATE as i32, &tone);
    recognizer.decode(&stream);
    stream
        .get_result()
        .ok_or_else(|| "Paraformer 推理自检失败".to_string())?;

    let mut punc_config = OfflinePunctuationConfig::default();
    punc_config.model.ct_transformer = Some(
        model_dir
            .join("punc-model.onnx")
            .to_string_lossy()
            .into_owned(),
    );
    OfflinePunctuation::create(&punc_config).ok_or_else(|| "标点模型加载失败".to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        label_speakers, transcribe_whisper, whisper_timestamp_to_ms, RawSegment, MAX_SPEAKERS,
        NO_AUDIO_SIGNAL_ERROR, SPEAKER_TURN_SILENCE_MS,
    };
    use crate::meeting::audio::{WavWriter, TARGET_SAMPLE_RATE};
    use std::fs;

    #[test]
    fn whisper_timestamps_are_converted_from_ten_millisecond_units() {
        assert_eq!(whisper_timestamp_to_ms(0), 0);
        assert_eq!(whisper_timestamp_to_ms(123), 1230);
        assert_eq!(whisper_timestamp_to_ms(-1), 0);
    }

    #[test]
    fn whisper_rejects_silence_before_loading_the_model() {
        let path =
            std::env::temp_dir().join(format!("snack-meeting-silence-{}.wav", std::process::id()));
        fs::remove_file(&path).ok();
        let mut writer = WavWriter::create(&path).unwrap();
        writer
            .write_samples(&vec![0; TARGET_SAMPLE_RATE as usize])
            .unwrap();
        writer.finalize().unwrap();

        let error = transcribe_whisper(&[], &path, "zh", |_| true).unwrap_err();
        assert_eq!(error, NO_AUDIO_SIGNAL_ERROR);
        fs::remove_file(path).ok();
    }

    #[test]
    fn speakers_label_relative_and_capped() {
        let mut segments = Vec::new();
        let mut start = 0u64;
        for index in 0..12 {
            segments.push(RawSegment {
                start_ms: start,
                end_ms: start + 3000,
                text: format!("segment {index}"),
                // First segment and every third segment signal a turn.
                speaker_turn: index == 0 || index % 3 == 0,
            });
            start += 3000;
        }
        let samples = vec![0.0f32; 200_000];
        let labeled = label_speakers(segments, &samples);
        assert_eq!(labeled.len(), 12);
        // 1 + 4 turns → speaker index 4 (说话人 4), never 5.
        let max_speaker = labeled
            .iter()
            .map(|segment| {
                segment
                    .speaker
                    .trim_start_matches("说话人 ")
                    .parse::<usize>()
                    .unwrap()
            })
            .max()
            .unwrap();
        assert_eq!(max_speaker, MAX_SPEAKERS);
        assert!(labeled[0].speaker.starts_with("说话人 1"));
    }

    #[test]
    fn silence_gap_triggers_speaker_change() {
        let samples = vec![0.0f32; 200_000];
        let segments = vec![
            RawSegment {
                start_ms: 0,
                end_ms: 2000,
                text: "first".to_string(),
                speaker_turn: false,
            },
            RawSegment {
                start_ms: 2000 + SPEAKER_TURN_SILENCE_MS + 100,
                end_ms: 5000,
                text: "second".to_string(),
                speaker_turn: false,
            },
        ];
        let labeled = label_speakers(segments, &samples);
        assert_eq!(labeled[0].speaker, "说话人 1");
        assert_eq!(labeled[1].speaker, "说话人 2");
    }

    #[test]
    fn short_gap_keeps_speaker() {
        let samples = vec![0.0f32; 200_000];
        let segments = vec![
            RawSegment {
                start_ms: 0,
                end_ms: 2000,
                text: "first".to_string(),
                speaker_turn: false,
            },
            RawSegment {
                start_ms: 2100,
                end_ms: 5000,
                text: "second".to_string(),
                speaker_turn: false,
            },
        ];
        let labeled = label_speakers(segments, &samples);
        assert_eq!(labeled[1].speaker, "说话人 1");
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::{
        transcribe_funasr, transcribe_whisper, validate_funasr_model, validate_whisper_model,
    };
    use std::path::PathBuf;

    fn env_path(name: &str, fallback: &str) -> PathBuf {
        std::env::var(name)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(fallback))
    }

    /// Real end-to-end transcription test. Requires:
    ///   SNACK_MEETING_E2E=1 cargo test --release meeting::transcribe::e2e_tests -- --ignored --nocapture
    /// with the small model at /tmp/whisper-models/ggml-small.bin and a
    /// Chinese speech sample at /tmp/meeting-test.wav. The paths can be
    /// overridden with SNACK_WHISPER_MODEL_PATH and SNACK_WHISPER_TEST_WAV.
    #[test]
    #[ignore]
    fn real_transcription_runs_end_to_end() {
        if std::env::var("SNACK_MEETING_E2E").as_deref() != Ok("1") {
            eprintln!("skipped: set SNACK_MEETING_E2E=1 to run");
            return;
        }
        let model_path = env_path(
            "SNACK_WHISPER_MODEL_PATH",
            "/tmp/whisper-models/ggml-small.bin",
        );
        let wav_path = env_path("SNACK_WHISPER_TEST_WAV", "/tmp/meeting-test.wav");
        assert!(model_path.exists(), "model file missing");
        assert!(wav_path.exists(), "wav file missing");
        let model_bytes = std::fs::read(&model_path).unwrap();

        // Inference self-check first.
        validate_whisper_model(&model_bytes).expect("model self-check failed");

        let progress_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let progress_calls_for_cb = std::sync::Arc::clone(&progress_calls);
        let outcome = transcribe_whisper(&model_bytes, &wav_path, "zh", move |progress| {
            let calls = progress_calls_for_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if calls % 5 == 0 {
                eprintln!(
                    "progress: {}% segments={} text={:?}",
                    progress.percent, progress.segment_count, progress.current_text
                );
            }
            true
        })
        .expect("transcription failed");

        eprintln!("segments: {}", outcome.segments.len());
        for segment in &outcome.segments {
            eprintln!(
                "[{:06.2}-{:06.2}] {} {}",
                segment.start_ms as f64 / 1000.0,
                segment.end_ms as f64 / 1000.0,
                segment.speaker,
                segment.text
            );
        }
        eprintln!("full text: {}", outcome.text);

        assert!(!outcome.segments.is_empty(), "no segments produced");
        assert!(outcome.text.chars().count() > 20, "text too short");
        assert!(outcome
            .segments
            .windows(2)
            .all(|pair| pair[0].end_ms <= pair[1].start_ms));
        let recording_duration_ms = outcome.total_samples as u64 * 1000
            / u64::from(crate::meeting::audio::TARGET_SAMPLE_RATE);
        assert!(
            outcome
                .segments
                .iter()
                .all(|segment| segment.end_ms <= recording_duration_ms),
            "segment extends beyond the recording duration"
        );
        if let Ok(min_start_ms) = std::env::var("SNACK_WHISPER_MIN_START_MS") {
            let min_start_ms = min_start_ms.parse::<u64>().unwrap();
            assert!(
                outcome.segments[0].start_ms >= min_start_ms,
                "first segment starts at {} ms, expected at least {min_start_ms} ms",
                outcome.segments[0].start_ms
            );
        }
        assert!(
            progress_calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "progress callback never invoked"
        );
        // A meeting-like sample should surface meeting words.
        let text = outcome.text;
        let expected_terms = std::env::var("SNACK_WHISPER_EXPECTED_TERMS")
            .unwrap_or_else(|_| "发布,会议,测试".to_string());
        assert!(
            expected_terms
                .split(',')
                .any(|term| !term.is_empty() && text.contains(term)),
            "transcript did not contain any expected term: {expected_terms}"
        );
    }

    /// Native FunASR smoke test. The directory must contain
    /// asr-model.onnx, tokens.txt and punc-model.onnx.
    #[test]
    #[ignore]
    fn real_funasr_transcription_runs_end_to_end() {
        if std::env::var("SNACK_FUNASR_E2E").as_deref() != Ok("1") {
            eprintln!("skipped: set SNACK_FUNASR_E2E=1 to run");
            return;
        }
        let model_dir = std::env::var("SNACK_FUNASR_MODEL_DIR")
            .map(std::path::PathBuf::from)
            .expect("SNACK_FUNASR_MODEL_DIR is required");
        let wav_path = std::env::var("SNACK_FUNASR_TEST_WAV")
            .map(std::path::PathBuf::from)
            .expect("SNACK_FUNASR_TEST_WAV is required");

        validate_funasr_model(&model_dir).expect("FunASR model self-check failed");
        let outcome = transcribe_funasr(&model_dir, &wav_path, "zh", |_| true)
            .expect("FunASR transcription failed");
        assert!(!outcome.text.trim().is_empty());
        assert!(!outcome.segments.is_empty());
        eprintln!("FunASR transcript: {}", outcome.text);
    }
}
