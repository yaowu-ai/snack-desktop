//! Catalog of locally installable transcription models.
//!
//! The catalog is baked into the desktop binary. Size and SHA-256 values come
//! from the publishers' Hugging Face LFS metadata (k2-fsa/sherpa-onnx model
//! exports for FunASR, and `ggerganov/whisper.cpp` for Whisper).
//!
//! Bumping `catalog_version` for a model marks already-installed copies as
//! `update_required` so the user can explicitly confirm a new download.
//! Downloads never happen silently.

pub(crate) const CATALOG_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelKey {
    /// Native Paraformer + CT-Transformer bundle. This mirrors the model
    /// family used by Snack Record while inference remains inside Snack.
    FunAsr2G,
    /// 3.1 GB multilingual large-v3 compatibility option.
    LargeV3,
    /// 466 MB multilingual small, offered as a lightweight alternative.
    Small,
}

impl ModelKey {
    pub(crate) const DEFAULT: ModelKey = ModelKey::FunAsr2G;

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "funasr-2g" => Some(Self::FunAsr2G),
            "large-v3" => Some(Self::LargeV3),
            "small" => Some(Self::Small),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::FunAsr2G => "funasr-2g",
            Self::LargeV3 => "large-v3",
            Self::Small => "small",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ModelArtifact {
    pub(crate) filename: &'static str,
    pub(crate) url: &'static str,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct CatalogModel {
    pub(crate) key: ModelKey,
    pub(crate) display_name: &'static str,
    pub(crate) filename: &'static str,
    /// Files downloaded into the model's private Snack directory. Keeping the
    /// bundle explicit lets the native engine load ASR, vocabulary and
    /// punctuation without consulting Snack Record's app data or processes.
    pub(crate) artifacts: Vec<ModelArtifact>,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: &'static str,
    pub(crate) catalog_version: u32,
    pub(crate) languages: &'static str,
    pub(crate) capabilities: &'static str,
}

impl CatalogModel {
    /// Extra disk space (besides the model file itself) needed during install.
    pub(crate) fn install_overhead_bytes(&self) -> u64 {
        // Download staging file + final file coexist briefly; keep 15% headroom.
        self.size_bytes / 100 * 15
    }

    pub(crate) fn install_requirement_bytes(&self) -> u64 {
        self.size_bytes + self.install_overhead_bytes()
    }

    /// Expected on-disk footprint after installation (model + manifest).
    pub(crate) fn installed_size_bytes(&self) -> u64 {
        self.size_bytes + 1024
    }
}

pub(crate) fn catalog() -> Vec<CatalogModel> {
    vec![
        CatalogModel {
            key: ModelKey::FunAsr2G,
            display_name: "FunASR 中文增强（2G 模型族·原生优化）",
            filename: "asr-model.onnx",
            artifacts: vec![
                ModelArtifact {
                    filename: "asr-model.onnx",
                    url: "https://huggingface.co/csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09/resolve/main/model.onnx?download=true",
                    size_bytes: 822_641_426,
                    sha256: "ed302fb061dcb65655b5240f5f8cd18d6d6c2f5c2b5cb63184d413d728bc1ec4",
                },
                ModelArtifact {
                    filename: "tokens.txt",
                    url: "https://huggingface.co/csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09/resolve/main/tokens.txt?download=true",
                    size_bytes: 75_354,
                    sha256: "6c0e3b35cece259829e6cb5b8d90d13db88f61ea3a2953d11898e4b2bfd7a2e2",
                },
                ModelArtifact {
                    filename: "punc-model.onnx",
                    url: "https://huggingface.co/csukuangfj/sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12/resolve/main/model.onnx?download=true",
                    size_bytes: 294_372_519,
                    sha256: "e93593a6dbd69a07f8734ef269dbe861a379755f8d1c8354719432116f2c44bd",
                },
            ],
            size_bytes: 1_117_089_299,
            sha256: "bundle",
            catalog_version: CATALOG_VERSION,
            languages: "中文普通话、英语及常见中文方言",
            capabilities: "Paraformer 中文识别 · CT-Transformer 标点恢复 · 分段时间戳 · 相对说话人区分",
        },
        CatalogModel {
            key: ModelKey::LargeV3,
            display_name: "Whisper large-v3（中文优先）",
            filename: "ggml-large-v3.bin",
            artifacts: vec![ModelArtifact {
                filename: "ggml-large-v3.bin",
                url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",
                size_bytes: 3_095_033_483,
                sha256: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
            }],
            size_bytes: 3_095_033_483,
            sha256: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
            catalog_version: CATALOG_VERSION,
            languages: "中文普通话、英语等 99 种语言",
            capabilities: "标点恢复 · 长音频分段 · 分段时间戳 · 相对说话人区分",
        },
        CatalogModel {
            key: ModelKey::Small,
            display_name: "Whisper small（轻量）",
            filename: "ggml-small.bin",
            artifacts: vec![ModelArtifact {
                filename: "ggml-small.bin",
                url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
                size_bytes: 487_601_967,
                sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
            }],
            size_bytes: 487_601_967,
            sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
            catalog_version: CATALOG_VERSION,
            languages: "中文普通话、英语等 99 种语言",
            capabilities: "标点恢复 · 长音频分段 · 分段时间戳 · 相对说话人区分",
        },
    ]
}

pub(crate) fn find_model(key: ModelKey) -> Option<CatalogModel> {
    catalog().into_iter().find(|model| model.key == key)
}

/// Machine-readable label for the supported platform/arch tuple.
pub(crate) fn platform_label() -> String {
    let os = if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "Apple Silicon (arm64)"
    } else if cfg!(target_arch = "x86_64") {
        "Intel/AMD 64 位 (x86_64)"
    } else {
        "未知架构"
    };
    format!("{os} · {arch}")
}

#[cfg(test)]
mod tests {
    use super::{catalog, find_model, ModelKey, CATALOG_VERSION};

    #[test]
    fn catalog_contains_both_models_with_expected_sizes() {
        let models = catalog();
        assert_eq!(models.len(), 3);
        let funasr = find_model(ModelKey::FunAsr2G).expect("funasr-2g in catalog");
        let large = find_model(ModelKey::LargeV3).expect("large-v3 in catalog");
        let small = find_model(ModelKey::Small).expect("small in catalog");
        assert_eq!(large.size_bytes, 3_095_033_483);
        assert_eq!(small.size_bytes, 487_601_967);
        assert_eq!(funasr.artifacts.len(), 3);
        assert_eq!(funasr.size_bytes, 1_117_089_299);
        assert_eq!(large.sha256.len(), 64);
        assert_eq!(small.sha256.len(), 64);
        assert_eq!(large.catalog_version, CATALOG_VERSION);
    }

    #[test]
    fn install_requirement_accounts_for_overhead() {
        let small = find_model(ModelKey::Small).unwrap();
        assert!(small.install_requirement_bytes() > small.size_bytes);
        assert!(small.installed_size_bytes() > small.size_bytes);
    }

    #[test]
    fn model_key_roundtrip() {
        assert_eq!(ModelKey::parse("large-v3"), Some(ModelKey::LargeV3));
        assert_eq!(ModelKey::parse("funasr-2g"), Some(ModelKey::FunAsr2G));
        assert_eq!(ModelKey::parse("small"), Some(ModelKey::Small));
        assert_eq!(ModelKey::parse("nope"), None);
        assert_eq!(ModelKey::LargeV3.as_str(), "large-v3");
        assert_eq!(
            find_model(ModelKey::DEFAULT).unwrap().key,
            ModelKey::FunAsr2G
        );
    }
}
