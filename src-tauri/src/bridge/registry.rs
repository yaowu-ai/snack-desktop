use std::future::Future;
use std::pin::Pin;

use tauri::{AppHandle, WebviewWindow};

use super::protocol::{BridgeMethodDescriptor, BridgeProbeRequest};

pub(crate) type BridgeHandlerResult = Result<serde_json::Value, BridgeHandlerError>;
type BridgeHandlerFuture = Pin<Box<dyn Future<Output = BridgeHandlerResult> + Send>>;
type BridgeHandler = fn(BridgeContext, serde_json::Value) -> BridgeHandlerFuture;

#[derive(Clone)]
pub(crate) struct BridgeContext {
    pub(crate) app: AppHandle,
    #[allow(dead_code)]
    pub(crate) window: WebviewWindow,
}

pub(crate) struct BridgeHandlerError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

struct BridgeMethodRegistration {
    descriptor: BridgeMethodDescriptor,
    handler: BridgeHandler,
}

const METHODS: &[BridgeMethodRegistration] = &[BridgeMethodRegistration {
    descriptor: BridgeMethodDescriptor {
        method: "bridge.info",
        introduced_contract_revision: 1,
        api_versions: &[1],
    },
    handler: bridge_info_handler,
}];

pub(crate) fn descriptors() -> Vec<BridgeMethodDescriptor> {
    METHODS
        .iter()
        .map(|registration| registration.descriptor.clone())
        .collect()
}

pub(crate) fn supports(request: &BridgeProbeRequest) -> bool {
    find(request.method.trim()).is_some_and(|registration| {
        registration
            .descriptor
            .api_versions
            .contains(&request.api_version)
    })
}

pub(crate) async fn dispatch(
    context: BridgeContext,
    method: &str,
    payload: serde_json::Value,
) -> Option<BridgeHandlerResult> {
    let registration = find(method)?;
    Some((registration.handler)(context, payload).await)
}

fn find(method: &str) -> Option<&'static BridgeMethodRegistration> {
    METHODS
        .iter()
        .find(|registration| registration.descriptor.method == method)
}

fn bridge_info_handler(context: BridgeContext, payload: serde_json::Value) -> BridgeHandlerFuture {
    Box::pin(async move {
        validate_empty_payload(&payload)?;
        serde_json::to_value(super::build_bridge_info(&context.app)).map_err(|error| {
            BridgeHandlerError {
                code: "SERIALIZATION_FAILED",
                message: error.to_string(),
            }
        })
    })
}

fn validate_empty_payload(payload: &serde_json::Value) -> Result<(), BridgeHandlerError> {
    if payload.is_null() || payload.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(());
    }
    Err(BridgeHandlerError {
        code: "INVALID_PAYLOAD",
        message: "bridge.info does not accept payload fields".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_only_explicit_methods() {
        assert_eq!(descriptors().len(), 1);
        assert_eq!(descriptors()[0].method, "bridge.info");
        assert!(find("meeting_start_recording").is_none());
    }

    #[test]
    fn support_requires_a_registered_api_version() {
        assert!(supports(&request("bridge.info", 1)));
        assert!(!supports(&request("bridge.info", 2)));
        assert!(!supports(&request("bridge.missing", 1)));
    }

    fn request(method: &str, api_version: u32) -> BridgeProbeRequest {
        BridgeProbeRequest {
            method: method.to_string(),
            required_contract_revision: 1,
            api_version,
        }
    }
}
