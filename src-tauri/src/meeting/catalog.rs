//! Catalog of locally installable transcription models.
//!
//! The catalog is baked into the desktop binary. Snack ships one fixed FunASR
//! bundle, fetched from ModelScope with `modelscope.snapshot_download`.
//!
//! Bumping `catalog_version` for a model marks already-installed copies as
//! `update_required` so the user can explicitly confirm a new download.
//! Downloads never happen silently.

pub(crate) const CATALOG_VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelKey {
    /// The only local model package offered by Snack.
    FunAsr2G,
}

impl ModelKey {
    pub(crate) const DEFAULT: ModelKey = ModelKey::FunAsr2G;

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "funasr-modelscope" => Some(Self::FunAsr2G),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::FunAsr2G => "funasr-modelscope",
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
    /// Kept empty for the ModelScope snapshot bundle. The ModelScope SDK owns
    /// per-file integrity and resume handling inside its cache.
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
    vec![CatalogModel {
        key: ModelKey::FunAsr2G,
        display_name: "Snack 中文会议转写模型",
        filename: ".modelscope-complete.json",
        artifacts: vec![],
        // ModelScope reports individual repository sizes at runtime. This
        // conservative estimate is used only for the pre-download disk check
        // and to set the first-download expectation in the UI.
        size_bytes: 2_200_000_000,
        sha256: "modelscope-snapshot",
        catalog_version: CATALOG_VERSION,
        languages: "中文普通话、英语及常见中文方言",
        capabilities: "Paraformer 识别 · VAD 端点检测 · 标点恢复 · 相对说话人区分",
    }]
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
    fn catalog_contains_one_fixed_modelscope_bundle() {
        let models = catalog();
        assert_eq!(models.len(), 1);
        let funasr = find_model(ModelKey::FunAsr2G).expect("funasr-2g in catalog");
        assert_eq!(funasr.artifacts.len(), 0);
        assert_eq!(funasr.size_bytes, 2_200_000_000);
        assert_eq!(funasr.catalog_version, CATALOG_VERSION);
    }

    #[test]
    fn install_requirement_accounts_for_overhead() {
        let model = find_model(ModelKey::DEFAULT).unwrap();
        assert!(model.install_requirement_bytes() > model.size_bytes);
        assert!(model.installed_size_bytes() > model.size_bytes);
    }

    #[test]
    fn model_key_roundtrip() {
        assert_eq!(
            ModelKey::parse("funasr-modelscope"),
            Some(ModelKey::FunAsr2G)
        );
        assert_eq!(ModelKey::parse("nope"), None);
        assert_eq!(ModelKey::FunAsr2G.as_str(), "funasr-modelscope");
        assert_eq!(
            find_model(ModelKey::DEFAULT).unwrap().key,
            ModelKey::FunAsr2G
        );
    }
}
