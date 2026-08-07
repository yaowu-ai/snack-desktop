//! Recording reminder lifecycle shared by meeting orchestration.

use tauri::AppHandle;

#[cfg(target_os = "macos")]
use super::reminder_macos::ReminderControl;

pub(crate) struct RecordingReminderMonitor {
    #[cfg(target_os = "macos")]
    control: ReminderControl,
}

impl RecordingReminderMonitor {
    pub(crate) fn new(app: AppHandle) -> Self {
        #[cfg(target_os = "macos")]
        {
            Self {
                control: ReminderControl::start(app),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = app;
            Self {}
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        #[cfg(target_os = "macos")]
        self.control.set_enabled(enabled);
        #[cfg(not(target_os = "macos"))]
        let _ = enabled;
    }

    pub(crate) fn set_recording_active(&self, active: bool) {
        #[cfg(target_os = "macos")]
        self.control.set_recording_active(active);
        #[cfg(not(target_os = "macos"))]
        let _ = active;
    }
}

pub(crate) const fn supported() -> bool {
    cfg!(target_os = "macos")
}
