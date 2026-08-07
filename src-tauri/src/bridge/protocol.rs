use serde::{Deserialize, Serialize};

pub(crate) const BRIDGE_CONTRACT_REVISION: u32 = 1;
pub(crate) const BRIDGE_UPDATE_PROGRESS_EVENT: &str = "bridge-update-progress";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeProbeRequest {
    pub(crate) method: String,
    pub(crate) required_contract_revision: u32,
    pub(crate) api_version: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeCallRequest {
    pub(crate) method: String,
    pub(crate) required_contract_revision: u32,
    pub(crate) api_version: u32,
    #[serde(default)]
    pub(crate) payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeMethodDescriptor {
    pub(crate) method: &'static str,
    pub(crate) introduced_contract_revision: u32,
    pub(crate) api_versions: &'static [u32],
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeInfo {
    pub(crate) contract_revision: u32,
    pub(crate) app_version: &'static str,
    pub(crate) platform: &'static str,
    pub(crate) arch: &'static str,
    pub(crate) methods: Vec<BridgeMethodDescriptor>,
    pub(crate) update: BridgeUpdateSnapshot,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BridgeProbeStatus {
    Supported,
    UpdateRequired,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeProbeResponse {
    pub(crate) status: BridgeProbeStatus,
    pub(crate) current_contract_revision: u32,
    pub(crate) code: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum BridgeCallResponse {
    Ok { data: serde_json::Value },
    Updating { version: Option<String> },
    Unsupported { code: &'static str, message: String },
    Busy { code: &'static str, message: String },
    UpdateFailed { code: &'static str, message: String },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeUpdateSnapshot {
    pub(crate) stage: BridgeUpdateStage,
    pub(crate) version: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BridgeUpdateStage {
    Idle,
    Checking,
    Downloading,
    Verifying,
    Installing,
    Relaunching,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeUpdateProgress {
    pub(crate) stage: BridgeUpdateStage,
    pub(crate) version: String,
    pub(crate) downloaded_bytes: Option<u64>,
    pub(crate) total_bytes: Option<u64>,
    pub(crate) percent: Option<u8>,
    pub(crate) error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_response_uses_a_stable_tagged_shape() {
        let response = BridgeCallResponse::Updating {
            version: Some("1.4.0".to_string()),
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::json!({ "status": "updating", "version": "1.4.0" })
        );
    }

    #[test]
    fn update_progress_serializes_for_the_web_listener() {
        let progress = BridgeUpdateProgress {
            stage: BridgeUpdateStage::Downloading,
            version: "1.4.0".to_string(),
            downloaded_bytes: Some(50),
            total_bytes: Some(100),
            percent: Some(50),
            error: None,
        };
        let value = serde_json::to_value(progress).unwrap();
        assert_eq!(value["downloadedBytes"], 50);
        assert_eq!(value["stage"], "downloading");
    }
}
