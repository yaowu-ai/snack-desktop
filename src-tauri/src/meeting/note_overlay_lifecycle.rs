//! Pure reminder lifetime. Callers keep one state per owner, recording and processing attempt.
//! Window hiding never clears this state, and an old attempt cannot affect a new one.

pub(crate) const SUCCESS_DISPLAY_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activity {
    Recording,
    Processing,
    Success,
    Failure,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Lifecycle {
    pub(crate) processing_dismissed: bool,
    pub(crate) terminal_closed: bool,
    pub(crate) success_presented_at_ms: Option<u64>,
}

impl Lifecycle {
    /// `result_viewed` must mean the result page is visible in the focused main window.
    /// `note_visible` also permits temporarily hiding an in-progress reminder.
    pub(crate) fn should_show(
        &mut self,
        activity: Activity,
        note_visible: bool,
        result_viewed: bool,
        now_ms: u64,
    ) -> bool {
        if activity != Activity::Success && !self.terminal_closed {
            self.success_presented_at_ms = None;
        }
        if activity == Activity::Recording {
            return !note_visible;
        }
        if self.terminal_closed {
            return false;
        }
        if matches!(activity, Activity::Success | Activity::Failure) && result_viewed {
            self.terminal_closed = true;
            return false;
        }
        if activity == Activity::Success
            && self
                .success_presented_at_ms
                .is_some_and(|started| now_ms.saturating_sub(started) >= SUCCESS_DISPLAY_MS)
        {
            self.terminal_closed = true;
            return false;
        }
        if note_visible {
            return false;
        }
        activity != Activity::Processing || !self.processing_dismissed
    }

    /// Start the five seconds only after the native window was successfully shown.
    pub(crate) fn presented(&mut self, activity: Activity, now_ms: u64) {
        if activity == Activity::Success && !self.terminal_closed {
            self.success_presented_at_ms.get_or_insert(now_ms);
        }
    }

    pub(crate) fn dismiss(&mut self, activity: Activity) -> Result<(), &'static str> {
        match activity {
            Activity::Recording => return Err("an active recording cannot be dismissed"),
            Activity::Processing => self.processing_dismissed = true,
            Activity::Success | Activity::Failure => self.terminal_closed = true,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_expires_five_seconds_after_presentation_and_never_revives() {
        let mut state = Lifecycle::default();
        assert!(state.should_show(Activity::Success, false, false, 500));
        assert_eq!(state.success_presented_at_ms, None);
        state.presented(Activity::Success, 1_000);
        state.presented(Activity::Success, 3_000); // duplicate sync cannot restart the clock
        assert!(state.should_show(Activity::Success, false, false, 5_999));
        assert!(!state.should_show(Activity::Success, false, false, 6_000));
        let mut rebound = state; // persisted/reloaded snapshot
        assert!(!rebound.should_show(Activity::Success, false, false, 50_000));
        assert!(!rebound.should_show(Activity::Processing, false, false, 50_001));
    }

    #[test]
    fn incomplete_success_never_starts_a_timer() {
        let mut state = Lifecycle::default();
        state.presented(Activity::Processing, 1_000);
        assert!(state.should_show(Activity::Processing, false, false, 80_000));
        assert_eq!(state.success_presented_at_ms, None);
    }

    #[test]
    fn follow_up_processing_restarts_the_final_success_display_time() {
        let mut state = Lifecycle::default();
        state.presented(Activity::Success, 1_000);
        assert!(state.should_show(Activity::Processing, false, false, 3_000));
        assert_eq!(state.success_presented_at_ms, None);
        assert!(state.should_show(Activity::Success, false, false, 40_000));
        state.presented(Activity::Success, 40_000);
        assert!(state.should_show(Activity::Success, false, false, 44_999));
        assert!(!state.should_show(Activity::Success, false, false, 45_000));
    }

    #[test]
    fn processing_can_be_muted_but_its_final_result_still_appears_once() {
        for result in [Activity::Success, Activity::Failure] {
            let mut state = Lifecycle::default();
            state.dismiss(Activity::Processing).unwrap();
            assert!(!state.should_show(Activity::Processing, false, false, 10));
            assert!(state.should_show(result, false, false, 20));
            state.dismiss(result).unwrap();
            assert!(!state.should_show(result, false, false, 30));
        }
    }

    #[test]
    fn recording_cannot_be_dismissed_even_if_a_previous_stage_was_muted() {
        let mut state = Lifecycle::default();
        assert!(state.dismiss(Activity::Recording).is_err());
        assert!(state.should_show(Activity::Recording, false, false, 10));
        assert!(!state.should_show(Activity::Recording, true, false, 20));
    }

    #[test]
    fn viewing_processing_only_hides_it_temporarily() {
        let mut state = Lifecycle::default();
        assert!(!state.should_show(Activity::Processing, true, true, 10));
        assert!(state.should_show(Activity::Processing, false, false, 20));
        assert!(!state.terminal_closed);
    }

    #[test]
    fn failure_has_no_timer_and_requires_actual_viewing_or_manual_close() {
        let mut state = Lifecycle::default();
        state.presented(Activity::Failure, 10);
        assert!(state.should_show(Activity::Failure, false, false, 10_000_000));
        // A matching route in a background window is not an acknowledgement.
        assert!(!state.should_show(Activity::Failure, true, false, 10_000_001));
        assert!(!state.terminal_closed);
        assert!(state.should_show(Activity::Failure, false, false, 10_000_002));
        assert!(!state.should_show(Activity::Failure, true, true, 10_000_003));
        assert!(!state.should_show(Activity::Failure, false, false, 10_000_004));
    }

    #[test]
    fn an_old_attempts_timer_or_close_does_not_mutate_a_new_attempt() {
        let mut old = Lifecycle::default();
        let mut next = Lifecycle::default();
        old.presented(Activity::Success, 0);
        assert!(!old.should_show(Activity::Success, false, false, 5_000));
        old.dismiss(Activity::Failure).unwrap();
        assert!(next.should_show(Activity::Processing, false, false, 5_001));
        assert_eq!(next, Lifecycle::default());
    }
}
