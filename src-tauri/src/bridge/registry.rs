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

const METHODS: &[BridgeMethodRegistration] = &[
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "locale.get",
            introduced_contract_revision: 1,
            api_versions: &[1],
        },
        handler: locale_get_handler,
    },
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "locale.set",
            introduced_contract_revision: 1,
            api_versions: &[1],
        },
        handler: locale_set_handler,
    },
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "bridge.info",
            introduced_contract_revision: 1,
            api_versions: &[1],
        },
        handler: bridge_info_handler,
    },
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "notifications.permission",
            introduced_contract_revision: 1,
            api_versions: &[1],
        },
        handler: notification_permission_handler,
    },
];

fn locale_get_handler(context: BridgeContext, payload: serde_json::Value) -> BridgeHandlerFuture {
    Box::pin(async move {
        validate_empty_payload(&payload)?;
        Ok(serde_json::json!(crate::i18n::snapshot(&context.app)))
    })
}

fn locale_set_handler(context: BridgeContext, payload: serde_json::Value) -> BridgeHandlerFuture {
    Box::pin(async move {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Preference {
            preference: Option<crate::i18n::Locale>,
        }
        let request: Preference =
            serde_json::from_value(payload).map_err(|_| BridgeHandlerError {
                code: "INVALID_LOCALE",
                message: crate::i18n::t(&context.app, "desktop.localeFailed"),
            })?;
        let snapshot =
            crate::i18n::set_preference(&context.app, request.preference).map_err(|_| {
                BridgeHandlerError {
                    code: "LOCALE_SAVE_FAILED",
                    message: crate::i18n::t(&context.app, "desktop.localeFailed"),
                }
            })?;
        Ok(serde_json::json!(snapshot))
    })
}

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
        message: "this method does not accept payload fields".to_string(),
    })
}

fn notification_permission_handler(
    context: BridgeContext,
    payload: serde_json::Value,
) -> BridgeHandlerFuture {
    Box::pin(async move {
        validate_empty_payload(&payload)?;
        let permission = crate::notification_permission::current(&context.app)
            .await
            .map_err(|message| BridgeHandlerError {
                code: "NOTIFICATION_PERMISSION_UNAVAILABLE",
                message,
            })?;
        Ok(serde_json::json!({ "permission": permission }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_only_explicit_methods() {
        assert_eq!(descriptors().len(), 4);
        assert!(find("bridge.info").is_some());
        assert!(find("locale.get").is_some());
        assert!(find("locale.set").is_some());
        assert!(find("meeting_start_recording").is_none());
    }

    #[test]
    fn support_requires_a_registered_api_version() {
        assert!(supports(&request("bridge.info", 1)));
        assert!(supports(&request("notifications.permission", 1)));
        assert!(!supports(&request("notifications.permission", 2)));
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
