//! Local FunASR transcription backed by Snack's fixed ModelScope snapshot.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::meeting::catalog::ModelKey;
use crate::meeting::state::TranscriptSegment;

pub(crate) struct TranscriptionOutcome {
    pub(crate) segments: Vec<TranscriptSegment>,
    pub(crate) text: String,
    pub(crate) language: String,
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

    let python = model_dir.join("runtime/venv/bin/python");
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

    let decoded: ModelScopeTranscript = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("本地转写结果无效: {error}"))?;
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
    if !on_progress(TranscriptionProgress {
        percent: 100,
        current_text: decoded.text.clone(),
        segment_count: segments.len(),
    }) {
        return Err("本地转写已停止".to_string());
    }
    Ok(TranscriptionOutcome {
        segments,
        text: decoded.text,
        language: language.to_string(),
    })
}

/// Installation validation deliberately checks the complete local runtime and
/// cache instead of loading the multi-GB model a second time.
pub(crate) fn validate_model(model_key: ModelKey, model_dir: &Path) -> Result<(), String> {
    if model_key == ModelKey::FunAsr2G
        && model_dir.join("runtime/venv/bin/python").exists()
        && model_dir.join("modelscope-cache").exists()
    {
        Ok(())
    } else {
        Err("本地 FunASR 运行环境不完整".to_string())
    }
}
