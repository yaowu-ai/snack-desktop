mod protocol;
mod registry;
mod updater;

use tauri::{AppHandle, Manager, WebviewWindow};

use protocol::{
    BridgeCallRequest, BridgeCallResponse, BridgeInfo, BridgeProbeRequest, BridgeProbeResponse,
    BridgeProbeStatus, BRIDGE_CONTRACT_REVISION,
};
use registry::BridgeContext;
use updater::{BridgeUpdateCoordinator, StartUpdateResult};

pub(crate) fn initialize(app: &AppHandle) {
    app.manage(BridgeUpdateCoordinator::new());
}

#[tauri::command]
pub(crate) fn bridge_info(app: AppHandle, window: WebviewWindow) -> Result<BridgeInfo, String> {
    require_allowed_main_webview(&window)?;
    Ok(build_bridge_info(&app))
}

#[tauri::command]
pub(crate) fn bridge_probe(
    window: WebviewWindow,
    request: BridgeProbeRequest,
) -> Result<BridgeProbeResponse, String> {
    require_allowed_main_webview(&window)?;
    Ok(resolve_probe(&request))
}

#[tauri::command]
pub(crate) async fn bridge_call(
    app: AppHandle,
    window: WebviewWindow,
    request: BridgeCallRequest,
) -> Result<BridgeCallResponse, String> {
    require_allowed_main_webview(&window)?;
    match resolve_probe(&probe_from_call(&request)).status {
        BridgeProbeStatus::Supported => dispatch_call(app, window, request).await,
        BridgeProbeStatus::UpdateRequired => Ok(start_required_update(&app).await),
        BridgeProbeStatus::Unsupported => Ok(unsupported_response(&request)),
    }
}

#[tauri::command]
pub(crate) async fn bridge_update(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<BridgeCallResponse, String> {
    require_allowed_main_webview(&window)?;
    Ok(start_required_update(&app).await)
}

pub(crate) fn build_bridge_info(app: &AppHandle) -> BridgeInfo {
    BridgeInfo {
        contract_revision: BRIDGE_CONTRACT_REVISION,
        app_version: env!("SNACK_DESKTOP_VERSION"),
        platform: env!("SNACK_DESKTOP_PLATFORM"),
        arch: env!("SNACK_DESKTOP_ARCH"),
        methods: registry::descriptors(),
        update: app.state::<BridgeUpdateCoordinator>().snapshot(),
    }
}

async fn dispatch_call(
    app: AppHandle,
    window: WebviewWindow,
    request: BridgeCallRequest,
) -> Result<BridgeCallResponse, String> {
    let unsupported = unsupported_response(&request);
    let context = BridgeContext { app, window };
    let result = registry::dispatch(context, request.method.trim(), request.payload).await;
    Ok(match result {
        Some(Ok(data)) => BridgeCallResponse::Ok { data },
        Some(Err(error)) => BridgeCallResponse::Unsupported {
            code: error.code,
            message: error.message,
        },
        None => unsupported,
    })
}

async fn start_required_update(app: &AppHandle) -> BridgeCallResponse {
    match updater::start_update(app).await {
        StartUpdateResult::Updating(version) => BridgeCallResponse::Updating { version },
        StartUpdateResult::Unsupported => BridgeCallResponse::Unsupported {
            code: "NO_SUPPORTED_UPDATE",
            message: "当前版本不支持此能力，且暂无可用更新".to_string(),
        },
        StartUpdateResult::Busy(message) => BridgeCallResponse::Busy {
            code: "DESKTOP_BUSY",
            message,
        },
        StartUpdateResult::Failed(message) => BridgeCallResponse::UpdateFailed {
            code: "UPDATE_CHECK_FAILED",
            message,
        },
    }
}

fn resolve_probe(request: &BridgeProbeRequest) -> BridgeProbeResponse {
    if request.required_contract_revision > BRIDGE_CONTRACT_REVISION {
        return probe_response(BridgeProbeStatus::UpdateRequired, Some("CONTRACT_TOO_OLD"));
    }
    if registry::supports(request) {
        return probe_response(BridgeProbeStatus::Supported, None);
    }
    probe_response(
        BridgeProbeStatus::Unsupported,
        Some("METHOD_OR_API_UNSUPPORTED"),
    )
}

fn probe_response(status: BridgeProbeStatus, code: Option<&'static str>) -> BridgeProbeResponse {
    BridgeProbeResponse {
        status,
        current_contract_revision: BRIDGE_CONTRACT_REVISION,
        code,
    }
}

fn probe_from_call(request: &BridgeCallRequest) -> BridgeProbeRequest {
    BridgeProbeRequest {
        method: request.method.clone(),
        required_contract_revision: request.required_contract_revision,
        api_version: request.api_version,
    }
}

fn unsupported_response(request: &BridgeCallRequest) -> BridgeCallResponse {
    BridgeCallResponse::Unsupported {
        code: "METHOD_OR_API_UNSUPPORTED",
        message: format!(
            "当前桌面端不支持桥能力 {}@{}",
            request.method, request.api_version
        ),
    }
}

fn require_allowed_main_webview(window: &WebviewWindow) -> Result<(), String> {
    if window.label() != "main" {
        return Err("bridge kernel is only available to the main webview".to_string());
    }
    let url = window.url().map_err(|error| error.to_string())?;
    if crate::web::is_allowed_web_origin(&url) {
        Ok(())
    } else {
        Err("origin is not allowed to access the bridge kernel".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_contract_requires_an_update_without_app_version_logic() {
        let response = resolve_probe(&probe("future.method", BRIDGE_CONTRACT_REVISION + 1, 1));
        assert_eq!(response.status, BridgeProbeStatus::UpdateRequired);
        assert_eq!(response.code, Some("CONTRACT_TOO_OLD"));
    }

    #[test]
    fn typo_under_current_contract_does_not_trigger_an_update() {
        let response = resolve_probe(&probe("bridge.typo", BRIDGE_CONTRACT_REVISION, 1));
        assert_eq!(response.status, BridgeProbeStatus::Unsupported);
    }

    #[test]
    fn registered_method_and_api_are_supported() {
        let response = resolve_probe(&probe("bridge.info", BRIDGE_CONTRACT_REVISION, 1));
        assert_eq!(response.status, BridgeProbeStatus::Supported);
    }

    fn probe(method: &str, revision: u32, api_version: u32) -> BridgeProbeRequest {
        BridgeProbeRequest {
            method: method.to_string(),
            required_contract_revision: revision,
            api_version,
        }
    }
}
