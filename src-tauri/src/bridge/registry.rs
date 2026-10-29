use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;
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
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "diagnostics.record",
            introduced_contract_revision: 2,
            api_versions: &[1],
        },
        handler: diagnostics_record_handler,
    },
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "diagnostics.snapshot",
            introduced_contract_revision: 2,
            api_versions: &[1],
        },
        handler: diagnostics_snapshot_handler,
    },
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "diagnostics.clear",
            introduced_contract_revision: 2,
            api_versions: &[1],
        },
        handler: diagnostics_clear_handler,
    },
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "diagnostics.info",
            introduced_contract_revision: 2,
            api_versions: &[1],
        },
        handler: diagnostics_info_handler,
    },
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "recording.audio.read",
            introduced_contract_revision: 3,
            api_versions: &[1],
        },
        handler: recording_audio_read_handler,
    },
    BridgeMethodRegistration {
        descriptor: BridgeMethodDescriptor {
            method: "recording.overlay.sync",
            introduced_contract_revision: 4,
            api_versions: &[1, 2, 3, 4],
        },
        handler: recording_overlay_sync_handler,
    },
];

fn recording_overlay_sync_handler(
    context: BridgeContext,
    payload: serde_json::Value,
) -> BridgeHandlerFuture {
    Box::pin(async move {
        let request = parse_payload(payload)?;
        let result = crate::meeting::note_overlay::sync(&context.app, &context.window, request)
            .map_err(|message| BridgeHandlerError {
                code: "RECORDING_OVERLAY_UNAVAILABLE",
                message,
            })?;
        serde_json::to_value(result).map_err(|error| BridgeHandlerError {
            code: "SERIALIZATION_FAILED",
            message: error.to_string(),
        })
    })
}

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

fn recording_audio_read_handler(
    context: BridgeContext,
    payload: serde_json::Value,
) -> BridgeHandlerFuture {
    Box::pin(async move {
        let request = parse_payload(payload)?;
        let response = crate::meeting::audio_read::read(&context.app, request)
            .await
            .map_err(|error| BridgeHandlerError {
                code: error.code,
                message: error.message,
            })?;
        serde_json::to_value(response).map_err(|error| BridgeHandlerError {
            code: "SERIALIZATION_FAILED",
            message: error.to_string(),
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

fn diagnostics_record_handler(
    context: BridgeContext,
    payload: serde_json::Value,
) -> BridgeHandlerFuture {
    Box::pin(async move {
        let request: DiagnosticsRecordRequest = parse_payload(payload)?;
        crate::logging::record_web_event(
            &context.app,
            crate::logging::DiagnosticRecord {
                scope: request.scope.scope,
                user_id: request.scope.user_id,
                tenant_key: request.scope.tenant_key,
                owner_type: request.scope.owner_type,
                session_id: request.scope.session_id,
                event: request.event,
            },
        )
        .map_err(|message| BridgeHandlerError {
            code: "INVALID_DIAGNOSTIC_EVENT",
            message,
        })?;
        Ok(serde_json::json!({ "accepted": true }))
    })
}

fn diagnostics_snapshot_handler(
    context: BridgeContext,
    payload: serde_json::Value,
) -> BridgeHandlerFuture {
    Box::pin(async move {
        let request: DiagnosticsSnapshotRequest = parse_payload(payload)?;
        if request.limit == 0 || request.limit > 10000 {
            return Err(BridgeHandlerError {
                code: "INVALID_PAYLOAD",
                message: "诊断日志分页参数无效".to_string(),
            });
        }
        activate_diagnostics_scope(&context.app, &request.scope)?;
        let rows = crate::logging::snapshot(
            &context.app,
            &request.scope.scope,
            request.since,
            request.limit,
        )
        .map_err(diagnostics_error)?;
        serde_json::to_value(serde_json::json!({ "rows": rows })).map_err(|error| {
            BridgeHandlerError {
                code: "SERIALIZATION_FAILED",
                message: error.to_string(),
            }
        })
    })
}

fn diagnostics_clear_handler(
    context: BridgeContext,
    payload: serde_json::Value,
) -> BridgeHandlerFuture {
    Box::pin(async move {
        let request: DiagnosticsScopeRequest = parse_payload(payload)?;
        activate_diagnostics_scope(&context.app, &request.scope)?;
        crate::logging::clear_scope(&context.app, &request.scope.scope)
            .map_err(diagnostics_error)?;
        Ok(serde_json::json!({ "cleared": true }))
    })
}

fn diagnostics_info_handler(
    context: BridgeContext,
    payload: serde_json::Value,
) -> BridgeHandlerFuture {
    Box::pin(async move {
        let request: DiagnosticsScopeRequest = parse_payload(payload)?;
        activate_diagnostics_scope(&context.app, &request.scope)?;
        serde_json::to_value(
            crate::logging::info(&context.app, &request.scope.scope).map_err(diagnostics_error)?,
        )
        .map_err(|error| BridgeHandlerError {
            code: "SERIALIZATION_FAILED",
            message: error.to_string(),
        })
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticsScope {
    scope: String,
    user_id: String,
    tenant_key: String,
    owner_type: String,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticsRecordRequest {
    #[serde(flatten)]
    scope: DiagnosticsScope,
    event: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticsScopeRequest {
    #[serde(flatten)]
    scope: DiagnosticsScope,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticsSnapshotRequest {
    #[serde(flatten)]
    scope: DiagnosticsScope,
    since: i64,
    #[serde(default = "default_snapshot_limit")]
    limit: usize,
}

fn default_snapshot_limit() -> usize {
    10000
}

fn parse_payload<T: for<'de> Deserialize<'de>>(
    payload: serde_json::Value,
) -> Result<T, BridgeHandlerError> {
    serde_json::from_value(payload).map_err(|error| BridgeHandlerError {
        code: "INVALID_PAYLOAD",
        message: error.to_string(),
    })
}

fn activate_diagnostics_scope(
    app: &tauri::AppHandle,
    scope: &DiagnosticsScope,
) -> Result<(), BridgeHandlerError> {
    if scope.scope.trim().is_empty() || scope.scope.len() > 256 {
        return Err(BridgeHandlerError {
            code: "INVALID_PAYLOAD",
            message: "诊断日志 scope 无效".to_string(),
        });
    }
    crate::logging::activate_scope(
        app,
        &crate::logging::DiagnosticScopeInput {
            scope: scope.scope.clone(),
            user_id: scope.user_id.clone(),
            tenant_key: scope.tenant_key.clone(),
            owner_type: scope.owner_type.clone(),
            session_id: scope.session_id.clone(),
        },
    )
    .map_err(diagnostics_error)
}

fn diagnostics_error(message: String) -> BridgeHandlerError {
    BridgeHandlerError {
        code: "DIAGNOSTICS_UNAVAILABLE",
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_only_explicit_methods() {
        assert_eq!(descriptors().len(), 10);
        assert!(find("bridge.info").is_some());
        assert!(find("locale.get").is_some());
        assert!(find("locale.set").is_some());
        assert!(find("notifications.permission").is_some());
        assert!(find("diagnostics.record").is_some());
        assert!(find("diagnostics.snapshot").is_some());
        assert!(find("diagnostics.clear").is_some());
        assert!(find("diagnostics.info").is_some());
        assert!(find("recording.audio.read").is_some());
        assert!(find("recording.overlay.sync").is_some());
        assert!(find("meeting_start_recording").is_none());
    }

    #[test]
    fn support_requires_a_registered_api_version() {
        assert!(supports(&request("bridge.info", 1)));
        assert!(supports(&request("notifications.permission", 1)));
        assert!(supports(&request("diagnostics.info", 1)));
        assert!(supports(&request("recording.audio.read", 1)));
        assert!(supports(&request("recording.overlay.sync", 1)));
        assert!(supports(&request("recording.overlay.sync", 2)));
        assert!(supports(&request("recording.overlay.sync", 3)));
        assert!(supports(&request("recording.overlay.sync", 4)));
        assert!(!supports(&request("recording.overlay.sync", 5)));
        assert!(!supports(&request("recording.audio.read", 2)));
        assert!(!supports(&request("notifications.permission", 2)));
        assert!(!supports(&request("bridge.info", 2)));
        assert!(!supports(&request("bridge.missing", 1)));
    }

    #[test]
    fn recording_audio_read_declares_its_contract_revision() {
        let descriptor = &find("recording.audio.read").unwrap().descriptor;
        assert_eq!(descriptor.introduced_contract_revision, 3);
        assert_eq!(descriptor.api_versions, &[1]);
    }

    fn request(method: &str, api_version: u32) -> BridgeProbeRequest {
        BridgeProbeRequest {
            method: method.to_string(),
            required_contract_revision: 1,
            api_version,
        }
    }
}
