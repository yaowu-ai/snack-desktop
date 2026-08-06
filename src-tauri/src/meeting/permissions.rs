//! macOS privacy permission checks via raw Objective-C runtime calls.
//!
//! - Microphone: `AVCaptureDevice authorizationStatusForMediaType:` — exact,
//!   non-prompting status (notDetermined/restricted/denied/authorized).
//! - Screen recording (required for system audio capture): CoreGraphics
//!   preflight/request APIs, which avoid turning an ordinary status check into
//!   a user-visible privacy request during app startup.

#[cfg(target_os = "macos")]
use tauri::AppHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionAccess {
    Granted,
    Denied,
    Unknown,
}

impl PermissionAccess {
    pub(crate) fn from_granted(granted: bool) -> Self {
        if granted {
            Self::Granted
        } else {
            Self::Denied
        }
    }

    pub(crate) fn is_granted(self) -> bool {
        self == Self::Granted
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn check_mac_permission_statuses() -> (PermissionAccess, PermissionAccess) {
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    let microphone = match unsafe { AVMediaTypeAudio } {
        Some(media_type) => {
            let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
            if status == AVAuthorizationStatus::Authorized {
                PermissionAccess::Granted
            } else if status == AVAuthorizationStatus::NotDetermined {
                PermissionAccess::Unknown
            } else {
                PermissionAccess::Denied
            }
        }
        None => PermissionAccess::Denied,
    };

    let system_audio = {
        use core_graphics::access::ScreenCaptureAccess;
        if ScreenCaptureAccess.preflight() {
            PermissionAccess::Granted
        } else {
            // CoreGraphics does not distinguish not-yet-requested from a
            // previous denial until a request is made.
            PermissionAccess::Unknown
        }
    };

    (microphone, system_audio)
}

#[cfg(target_os = "macos")]
pub(crate) fn check_mac_permissions() -> (bool, bool) {
    let (microphone, system_audio) = check_mac_permission_statuses();
    (microphone.is_granted(), system_audio.is_granted())
}

#[cfg(target_os = "macos")]
pub(crate) async fn request_mac_permissions(
    app: &AppHandle,
) -> Result<(PermissionAccess, PermissionAccess), String> {
    let (mut microphone, system_audio) = check_mac_permission_statuses();
    if microphone != PermissionAccess::Granted {
        microphone = if microphone == PermissionAccess::Unknown {
            request_microphone_permission(app).await?
        } else {
            microphone
        };
    }

    // Do not call CGRequestScreenCaptureAccess here. On some macOS releases it
    // displays a prompt even when the privacy switch is already enabled. The
    // ScreenCaptureKit recorder performs the definitive check only after the
    // user explicitly starts a recording.
    Ok((microphone, system_audio))
}

#[cfg(target_os = "macos")]
async fn request_microphone_permission(app: &AppHandle) -> Result<PermissionAccess, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel::<PermissionAccess>();
    app.run_on_main_thread(move || {
        use block2::RcBlock;
        use objc2::runtime::Bool;
        use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
        use std::sync::Mutex;

        let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
            let _ = sender.send(PermissionAccess::Denied);
            return;
        };
        let sender = Mutex::new(Some(sender));
        let completion: RcBlock<dyn Fn(Bool)> = RcBlock::new(move |granted: Bool| {
            if let Some(sender) = sender.lock().ok().and_then(|mut value| value.take()) {
                let _ = sender.send(PermissionAccess::from_granted(granted.as_bool()));
            }
        });
        unsafe {
            AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &completion);
        }
    })
    .map_err(|error| format!("无法请求麦克风权限: {error}"))?;
    match tokio::time::timeout(std::time::Duration::from_secs(15), receiver).await {
        Ok(Ok(granted)) => Ok(granted),
        Ok(Err(_)) => Err("麦克风权限请求未完成".to_string()),
        Err(_) => Ok(PermissionAccess::Denied),
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn check_mac_permissions() -> (bool, bool) {
    (false, false)
}

#[cfg(not(target_os = "macos"))]
pub(crate) async fn request_mac_permissions(
    _app: &tauri::AppHandle,
) -> Result<(PermissionAccess, PermissionAccess), String> {
    Ok((PermissionAccess::Denied, PermissionAccess::Denied))
}

#[cfg(test)]
mod tests {
    use super::PermissionAccess;

    #[test]
    fn permission_access_serializes_each_state_without_losing_denials() {
        assert_eq!(PermissionAccess::from_granted(true).as_str(), "granted");
        assert_eq!(PermissionAccess::from_granted(false).as_str(), "denied");
        assert_eq!(PermissionAccess::Unknown.as_str(), "unknown");
    }
}
