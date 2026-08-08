//! Local FunASR transcription backed by Snack's fixed ModelScope snapshot.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::meeting::catalog::ModelKey;
use crate::meeting::state::TranscriptSegment;

#[derive(Debug)]
pub(crate) enum TranscriptionOutcome {
    NoAudioDetected,
    Detected {
        segments: Vec<TranscriptSegment>,
        text: String,
        language: String,
    },
}

pub(crate) struct TranscriptionProgress {
    pub(crate) percent: u8,
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

/// The package contains four fixed ModelScope repositories (ASR, VAD,
/// punctuation and speaker verification), so callers cannot select a model.
pub(crate) fn transcribe_file(
    model_key: ModelKey,
    model_dir: &Path,
    wav_path: &Path,
    language: &str,
    mut on_progress: impl FnMut(TranscriptionProgress) -> bool + 'static,
) -> Result<TranscriptionOutcome, String> {
    if model_key != ModelKey::FunAsr2G {
        return Err("不支持的本地模型".to_string());
    }
    if !on_progress(TranscriptionProgress {
        percent: 2,
        current_text: String::new(),
        segment_count: 0,
    }) {
        return Err("本地转写已停止".to_string());
    }

    let wav_samples = wav_path
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| value.eq_ignore_ascii_case("wav"))
        .map(|_| crate::meeting::audio::read_wav_samples(wav_path))
        .transpose()?;
    if wav_samples
        .as_deref()
        .is_some_and(|samples| !crate::meeting::audio::has_audio_signal(samples))
    {
        if !on_progress(TranscriptionProgress {
            percent: 100,
            current_text: String::new(),
            segment_count: 0,
        }) {
            return Err("本地转写已停止".to_string());
        }
        return Ok(TranscriptionOutcome::NoAudioDetected);
    }

    let runtime_dir = model_dir.join("runtime");
    let python = crate::meeting::python_runtime::venv_python(&runtime_dir);
    let script = model_dir.join("runtime/funasr_transcribe.py");
    let cache_dir = model_dir.join("modelscope-cache");
    if !python.exists() || !script.exists() || !cache_dir.exists() {
        return Err("本地会议模型运行环境不完整，请重新下载".to_string());
    }

    let output = Command::new(python)
        .arg(script)
        .arg(wav_path)
        .env("MODELSCOPE_CACHE", cache_dir)
        .output()
        .map_err(|error| format!("无法启动本地 FunASR 转写: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "本地 FunASR 转写失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let decoded = parse_modelscope_transcript(&output.stdout)?;
    if !on_progress(TranscriptionProgress {
        percent: 100,
        current_text: decoded.text.clone(),
        segment_count: decoded.segments.len(),
    }) {
        return Err("本地转写已停止".to_string());
    }
    Ok(transcription_outcome(decoded, language))
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

/// FunASR versions may print their version or progress logs to stdout before
/// the wrapper emits its final one-line JSON result. Prefer a fully clean
/// payload, then fall back to the last independently valid transcript line.
fn parse_modelscope_transcript(output: &[u8]) -> Result<ModelScopeTranscript, String> {
    match serde_json::from_slice(output) {
        Ok(decoded) => return Ok(decoded),
        Err(full_output_error) => {
            let output = String::from_utf8_lossy(output);
            for line in output
                .lines()
                .rev()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                if let Ok(decoded) = serde_json::from_str::<ModelScopeTranscript>(line) {
                    return Ok(decoded);
                }
            }
            Err(format!("本地转写结果无效: {full_output_error}"))
        }
    }
}

/// Installation validation deliberately checks the complete local runtime and
/// cache instead of loading the multi-GB model a second time.
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
            ModelKey::FunAsr2G,
            Path::new("/model-runtime-does-not-exist"),
            &wav_path,
            "zh",
            move |progress| {
                observed_for_callback.lock().unwrap().push(progress.percent);
                true
            },
        )
        .unwrap();

        assert!(matches!(outcome, TranscriptionOutcome::NoAudioDetected));
        assert_eq!(*observed.lock().unwrap(), vec![2, 100]);
        fs::remove_file(&wav_path).ok();
    }

    #[test]
    fn transcript_parser_ignores_funasr_stdout_logs() {
        let output = concat!(
            "funasr version: 1.2.7\n",
            "{\"text\":\"会议内容\",\"segments\":[{\"startMs\":0,",
            "\"endMs\":1200,\"text\":\"会议内容\",\"speaker\":\"说话人 1\"}]}\n"
        );

        let decoded = parse_modelscope_transcript(output.as_bytes()).unwrap();

        assert_eq!(decoded.text, "会议内容");
        assert_eq!(decoded.segments.len(), 1);
        assert_eq!(decoded.segments[0].end_ms, 1200);
    }

    #[test]
    fn transcript_parser_rejects_logs_without_a_json_result() {
        let error =
            parse_modelscope_transcript(b"funasr version: 1.2.7\nloading model\n").unwrap_err();

        assert!(error.starts_with("本地转写结果无效:"));
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
