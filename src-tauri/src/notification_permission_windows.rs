#[cfg(target_os = "windows")]
pub(super) fn current(identifier: &str) -> Result<&'static str, String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
    use windows::UI::Notifications::ToastNotificationManager;

    struct RuntimeInitialization(bool);

    impl Drop for RuntimeInitialization {
        fn drop(&mut self) {
            if self.0 {
                // Balance successful initialization, including an already initialized MTA.
                unsafe { RoUninitialize() };
            }
        }
    }

    // A blocking-pool thread may already use another COM apartment. It can still
    // read the notifier; only uninitialize when this call acquired a reference.
    let initialized = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let should_uninitialize = initialized.is_ok();
    if let Err(error) = initialized {
        if error.code() != RPC_E_CHANGED_MODE {
            return Err(error.to_string());
        }
    }
    let _runtime = RuntimeInitialization(should_uninitialize);

    // Desktop apps must use the same AppUserModelID as their notification sender.
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(identifier))
        .map_err(|error| error.to_string())?;
    let setting = notifier.Setting().map_err(|error| error.to_string())?;
    Ok(permission_from_setting(setting.0))
}

fn permission_from_setting(setting: i32) -> &'static str {
    match setting {
        0 => "granted",
        // Disabled for this application, user, group policy, or manifest.
        1..=4 => "denied",
        _ => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::permission_from_setting;

    #[test]
    fn only_enabled_windows_notifications_are_granted() {
        assert_eq!(permission_from_setting(0), "granted");
        for disabled_setting in 1..=4 {
            assert_eq!(permission_from_setting(disabled_setting), "denied");
        }
    }

    #[test]
    fn unknown_windows_notification_settings_are_not_granted() {
        assert_eq!(permission_from_setting(-1), "unsupported");
        assert_eq!(permission_from_setting(5), "unsupported");
    }
}
