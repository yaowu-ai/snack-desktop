//! Read current OS authorization, not the notification plugin's persisted WebView choice.

#[cfg(target_os = "macos")]
#[link(name = "UserNotifications", kind = "framework")]
extern "C" {}

#[cfg(target_os = "macos")]
pub(crate) async fn current(app: &tauri::AppHandle) -> Result<&'static str, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || read_macos_settings(sender))
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receiver)
        .await
        .map_err(|_| crate::i18n::t(app, "desktop.permissionTimeout"))?
        .map_err(|_| crate::i18n::t(app, "desktop.permissionCancelled"))
}

#[cfg(target_os = "macos")]
fn read_macos_settings(sender: tokio::sync::oneshot::Sender<&'static str>) {
    use block2::RcBlock;
    use objc2::{class, msg_send, runtime::AnyObject};
    use objc2_foundation::NSBundle;
    use std::sync::Mutex;

    // UNUserNotificationCenter raises an exception for an unbundled command-line process.
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        let _ = sender.send("unsupported");
        return;
    }
    let sender = Mutex::new(Some(sender));
    let completion = RcBlock::new(move |settings: *mut AnyObject| {
        // Apple's completion supplies a non-null UNNotificationSettings for this application.
        let status: isize = unsafe { msg_send![settings, authorizationStatus] };
        if let Some(sender) = sender.lock().ok().and_then(|mut value| value.take()) {
            let _ = sender.send(permission_from_status(status));
        }
    });
    unsafe {
        let center: *mut AnyObject =
            msg_send![class!(UNUserNotificationCenter), currentNotificationCenter];
        let _: () = msg_send![center, getNotificationSettingsWithCompletionHandler: &*completion];
    }
}

#[cfg(any(target_os = "macos", test))]
fn permission_from_status(status: isize) -> &'static str {
    match status {
        0 => "default",
        1 => "denied",
        2..=4 => "granted",
        _ => "unsupported",
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) async fn current(_app: &tauri::AppHandle) -> Result<&'static str, String> {
    Ok("unsupported")
}

#[cfg(test)]
mod tests {
    use super::permission_from_status;

    #[test]
    fn maps_os_authorization_without_treating_denial_as_granted() {
        assert_eq!(permission_from_status(0), "default");
        assert_eq!(permission_from_status(1), "denied");
        for status in 2..=4 {
            assert_eq!(permission_from_status(status), "granted");
        }
        assert_eq!(permission_from_status(99), "unsupported");
    }
}
