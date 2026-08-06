//! Opt-in macOS meeting-activity monitor.
//!
//! ScreenCaptureKit is filtered to one known meeting application. Audio
//! samples are reduced to RMS energy in memory and are never persisted.

use std::time::{Duration, Instant};

use crossbeam_channel::{after, bounded, unbounded, Receiver, Sender};
use objc2_app_kit::NSWorkspace;
use screencapturekit::cm::{CMSampleBuffer, CMTime};
use screencapturekit::prelude::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamOutputType, SCWindow,
};
use tauri::AppHandle;

const POLL_INTERVAL: Duration = Duration::from_secs(8);
const IDLE_WAIT: Duration = Duration::from_secs(60);
const COOLDOWN: Duration = Duration::from_secs(10 * 60);
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5 * 60);
const AUDIO_SAMPLE_RATE: f64 = 16_000.0;
const AUDIO_RMS_THRESHOLD: f64 = 0.002;
const ACTIVE_AUDIO_TRIGGER_SECONDS: f64 = 3.0;

pub(crate) struct ReminderControl {
    sender: Sender<ReminderCommand>,
}

impl ReminderControl {
    pub(crate) fn start(app: AppHandle) -> Self {
        let (sender, receiver) = unbounded();
        std::thread::Builder::new()
            .name("snack-meeting-reminder".to_string())
            .spawn(move || run_monitor(app, receiver))
            .expect("failed to start recording reminder monitor");
        Self { sender }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        let _ = self.sender.send(ReminderCommand::SetEnabled(enabled));
    }

    pub(crate) fn set_recording_active(&self, active: bool) {
        let _ = self
            .sender
            .send(ReminderCommand::SetRecordingActive(active));
    }
}

enum ReminderCommand {
    SetEnabled(bool),
    SetRecordingActive(bool),
}

struct ReminderRuntime {
    app: AppHandle,
    enabled: bool,
    recording_active: bool,
    probe: Option<AudioProbe>,
    active_audio_seconds: f64,
    cooldown_until: Option<Instant>,
    next_poll: Instant,
    last_error_log: Option<Instant>,
}

struct AudioProbe {
    stream: SCStream,
    bundle_identifier: String,
    application_name: String,
}

struct MeetingTarget {
    application: SCRunningApplication,
    display: SCDisplay,
    bundle_identifier: String,
    application_name: String,
}

#[derive(Debug, Clone, Copy)]
struct AudioSignal {
    rms: f64,
    duration_seconds: f64,
}

fn run_monitor(app: AppHandle, command_rx: Receiver<ReminderCommand>) {
    let (audio_tx, audio_rx) = bounded(64);
    let mut runtime = ReminderRuntime::new(app);
    loop {
        let wait = runtime.wait_duration();
        let poll_timer = after(wait);
        crossbeam_channel::select! {
            recv(command_rx) -> command => {
                let Ok(command) = command else { break };
                runtime.handle_command(command);
            }
            recv(audio_rx) -> signal => {
                if let Ok(signal) = signal { runtime.handle_audio(signal); }
            }
            recv(poll_timer) -> _ => runtime.poll(&audio_tx),
        }
    }
    runtime.stop_probe();
}

impl ReminderRuntime {
    fn new(app: AppHandle) -> Self {
        Self {
            app,
            enabled: false,
            recording_active: false,
            probe: None,
            active_audio_seconds: 0.0,
            cooldown_until: None,
            next_poll: Instant::now(),
            last_error_log: None,
        }
    }

    fn wait_duration(&self) -> Duration {
        if !self.enabled || self.recording_active {
            return IDLE_WAIT;
        }
        self.next_poll.saturating_duration_since(Instant::now())
    }

    fn handle_command(&mut self, command: ReminderCommand) {
        match command {
            ReminderCommand::SetEnabled(enabled) => self.set_enabled(enabled),
            ReminderCommand::SetRecordingActive(active) => self.set_recording_active(active),
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.next_poll = Instant::now();
        if enabled {
            return;
        }
        self.stop_probe();
        super::overlay::hide_recording_reminder(&self.app);
    }

    fn set_recording_active(&mut self, active: bool) {
        self.recording_active = active;
        self.next_poll = Instant::now();
        if active {
            self.stop_probe();
            super::overlay::hide_recording_reminder(&self.app);
        }
    }

    fn poll(&mut self, audio_tx: &Sender<AudioSignal>) {
        self.next_poll = Instant::now() + POLL_INTERVAL;
        if !self.should_probe() {
            return;
        }
        let content = match shareable_content() {
            Ok(content) => content,
            Err(error) => {
                self.log_probe_error(&format!("无法检查会议应用: {error}"));
                self.stop_probe();
                return;
            }
        };
        let current_probe = self
            .probe
            .as_ref()
            .map(|probe| probe.bundle_identifier.as_str());
        let frontmost_bundle = frontmost_bundle_identifier();
        let preferred = choose_preferred_bundle(frontmost_bundle.as_deref(), current_probe);
        let Some(target) = select_meeting_target(&content, preferred) else {
            self.stop_probe();
            return;
        };
        if !probe_target_changed(current_probe, &target.bundle_identifier) {
            return;
        }
        self.stop_probe();
        if let Err(error) = self.start_probe(target, audio_tx.clone()) {
            self.log_probe_error(&error);
        }
    }

    fn should_probe(&mut self) -> bool {
        if !self.enabled || self.recording_active {
            return false;
        }
        if self
            .cooldown_until
            .is_some_and(|deadline| deadline > Instant::now())
        {
            return false;
        }
        self.cooldown_until = None;
        true
    }

    fn start_probe(
        &mut self,
        target: MeetingTarget,
        audio_tx: Sender<AudioSignal>,
    ) -> Result<(), String> {
        let filter = SCContentFilter::create()
            .with_display(&target.display)
            .with_including_applications(&[&target.application], &[])
            .build();
        let frame_interval = CMTime::new(1, 1);
        let config = SCStreamConfiguration::new()
            .with_captures_audio(true)
            .with_excludes_current_process_audio(true)
            .with_sample_rate(AUDIO_SAMPLE_RATE as i32)
            .with_channel_count(1)
            .with_width(2)
            .with_height(2)
            .with_minimum_frame_interval(&frame_interval)
            .with_queue_depth(1);
        let mut stream = SCStream::new(&filter, &config);
        let handler = stream.add_output_handler(
            move |sample: CMSampleBuffer, output_type| {
                if output_type != SCStreamOutputType::Audio {
                    return;
                }
                if let Some(signal) = audio_signal(&sample) {
                    let _ = audio_tx.try_send(signal);
                }
            },
            SCStreamOutputType::Audio,
        );
        if handler.is_none() {
            return Err("无法监听会议应用音频".to_string());
        }
        stream
            .start_capture()
            .map_err(|error| format!("会议应用音频探测启动失败: {error}"))?;
        self.active_audio_seconds = 0.0;
        self.probe = Some(AudioProbe {
            stream,
            bundle_identifier: target.bundle_identifier,
            application_name: target.application_name,
        });
        Ok(())
    }

    fn handle_audio(&mut self, signal: AudioSignal) {
        if !self.enabled || self.recording_active || self.probe.is_none() {
            return;
        }
        self.active_audio_seconds = next_active_audio_seconds(
            self.active_audio_seconds,
            signal.rms,
            signal.duration_seconds,
        );
        if self.active_audio_seconds < ACTIVE_AUDIO_TRIGGER_SECONDS {
            return;
        }
        let application_name = self
            .probe
            .as_ref()
            .map(|probe| probe.application_name.clone())
            .unwrap_or_else(|| "会议应用".to_string());
        self.stop_probe();
        self.cooldown_until = Some(Instant::now() + COOLDOWN);
        if let Err(error) = super::overlay::show_recording_reminder(&self.app, &application_name) {
            self.log_probe_error(&error);
        }
    }

    fn stop_probe(&mut self) {
        if let Some(probe) = self.probe.take() {
            let _ = probe.stream.stop_capture();
        }
        self.active_audio_seconds = 0.0;
    }

    fn log_probe_error(&mut self, error: &str) {
        let now = Instant::now();
        if self
            .last_error_log
            .is_some_and(|last| now.duration_since(last) < ERROR_LOG_INTERVAL)
        {
            return;
        }
        self.last_error_log = Some(now);
        crate::logging::write_app_log(
            &self.app,
            "warn",
            "meeting-reminder",
            "meeting reminder probe failed",
            Some(&serde_json::json!({ "error": error })),
        );
    }
}

fn shareable_content() -> Result<SCShareableContent, String> {
    SCShareableContent::create()
        .with_exclude_desktop_windows(true)
        .with_on_screen_windows_only(false)
        .get()
        .map_err(|error| error.to_string())
}

fn select_meeting_target(
    content: &SCShareableContent,
    preferred_bundle: Option<&str>,
) -> Option<MeetingTarget> {
    let applications = content.applications();
    let windows = content.windows();
    let selected_window = select_meeting_window(&windows, preferred_bundle);
    let selected_bundle = selected_window
        .as_ref()
        .and_then(window_bundle_identifier)
        .or_else(|| select_dedicated_application(&applications, preferred_bundle))?;
    let application = applications
        .iter()
        .find(|application| application.bundle_identifier() == selected_bundle)?
        .clone();
    let displays = content.displays();
    let display = select_display(&displays, selected_window.as_ref())?;
    Some(MeetingTarget {
        application,
        display,
        application_name: meeting_application_name(&selected_bundle)?.to_string(),
        bundle_identifier: selected_bundle,
    })
}

fn select_meeting_window(windows: &[SCWindow], preferred: Option<&str>) -> Option<SCWindow> {
    windows
        .iter()
        .find(|window| {
            window_bundle_identifier(window).as_deref() == preferred
                && window_suggests_meeting(window)
        })
        .or_else(|| {
            windows
                .iter()
                .find(|window| window_title_explicitly_suggests_meeting(window))
        })
        .or_else(|| {
            windows
                .iter()
                .find(|window| window_suggests_meeting(window))
        })
        .cloned()
}

fn window_title_explicitly_suggests_meeting(window: &SCWindow) -> bool {
    window_bundle_identifier(window)
        .is_some_and(|bundle| meeting_application_name(&bundle).is_some())
        && title_suggests_meeting(window.title().as_deref())
}

fn frontmost_bundle_identifier() -> Option<String> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|application| application.bundleIdentifier())
        .map(|bundle| bundle.to_string())
}

fn choose_preferred_bundle<'a>(
    frontmost_bundle: Option<&'a str>,
    current_probe: Option<&'a str>,
) -> Option<&'a str> {
    frontmost_bundle
        .filter(|bundle| meeting_application_name(bundle).is_some())
        .or(current_probe)
}

fn probe_target_changed(current_probe: Option<&str>, selected_bundle: &str) -> bool {
    current_probe != Some(selected_bundle)
}

fn select_dedicated_application(
    applications: &[SCRunningApplication],
    preferred: Option<&str>,
) -> Option<String> {
    applications
        .iter()
        .find(|application| {
            let bundle = application.bundle_identifier();
            Some(bundle.as_str()) == preferred && is_dedicated_meeting_bundle(&bundle)
        })
        .or_else(|| {
            applications
                .iter()
                .find(|application| is_dedicated_meeting_bundle(&application.bundle_identifier()))
        })
        .map(SCRunningApplication::bundle_identifier)
}

fn window_suggests_meeting(window: &SCWindow) -> bool {
    let Some(bundle) = window_bundle_identifier(window) else {
        return false;
    };
    window_values_suggest_meeting(
        &bundle,
        window.title().as_deref(),
        window.frame().width,
        window.frame().height,
    )
}

fn window_values_suggest_meeting(
    bundle: &str,
    title: Option<&str>,
    width: f64,
    height: f64,
) -> bool {
    if meeting_application_name(bundle).is_none() {
        return false;
    }
    if title_suggests_meeting(title) {
        return true;
    }
    is_dedicated_meeting_bundle(bundle) && width >= 160.0 && height >= 100.0
}

fn title_suggests_meeting(title: Option<&str>) -> bool {
    let title = title.unwrap_or_default().to_lowercase();
    ["会议", "通话", "meeting", "call", "conference", "zoom"]
        .iter()
        .any(|keyword| title.contains(keyword))
}

fn window_bundle_identifier(window: &SCWindow) -> Option<String> {
    window
        .owning_application()
        .map(|application| application.bundle_identifier())
}

fn select_display(displays: &[SCDisplay], window: Option<&SCWindow>) -> Option<SCDisplay> {
    let preferred = window.and_then(|window| {
        let center = window.frame().center();
        displays.iter().find(|display| {
            let frame = display.frame();
            center.x >= frame.min_x()
                && center.x <= frame.max_x()
                && center.y >= frame.min_y()
                && center.y <= frame.max_y()
        })
    });
    preferred.or_else(|| displays.first()).cloned()
}

fn meeting_application_name(bundle: &str) -> Option<&'static str> {
    match bundle {
        "com.tencent.wwmapp" | "com.tencent.WeWorkMac" => Some("企业微信"),
        "com.electron.lark" | "com.bytedance.ee.lark" => Some("飞书"),
        "com.tencent.meeting" | "com.tencent.wemeet" => Some("腾讯会议"),
        "us.zoom.xos" => Some("Zoom"),
        _ => None,
    }
}

fn is_dedicated_meeting_bundle(bundle: &str) -> bool {
    matches!(
        bundle,
        "com.tencent.wwmapp"
            | "com.tencent.WeWorkMac"
            | "com.tencent.meeting"
            | "com.tencent.wemeet"
            | "us.zoom.xos"
    )
}

fn audio_signal(sample: &CMSampleBuffer) -> Option<AudioSignal> {
    let samples = super::capture_macos::sample_buffer_to_f32_mono(sample)?;
    if samples.is_empty() {
        return None;
    }
    let squared_sum = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>();
    Some(AudioSignal {
        rms: (squared_sum / samples.len() as f64).sqrt(),
        duration_seconds: samples.len() as f64 / AUDIO_SAMPLE_RATE,
    })
}

fn next_active_audio_seconds(current: f64, rms: f64, duration: f64) -> f64 {
    if rms >= AUDIO_RMS_THRESHOLD {
        current + duration.max(0.0)
    } else {
        (current - duration.max(0.0) * 0.5).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_meeting_apps_match_the_reference_set() {
        assert_eq!(meeting_application_name("com.electron.lark"), Some("飞书"));
        assert_eq!(
            meeting_application_name("com.tencent.wwmapp"),
            Some("企业微信")
        );
        assert_eq!(
            meeting_application_name("com.tencent.WeWorkMac"),
            Some("企业微信")
        );
        assert_eq!(meeting_application_name("us.zoom.xos"), Some("Zoom"));
        assert_eq!(meeting_application_name("com.example.other"), None);
    }

    #[test]
    fn feishu_requires_a_meeting_like_window_title() {
        assert!(window_values_suggest_meeting(
            "com.electron.lark",
            Some("产品会议"),
            900.0,
            700.0,
        ));
        assert!(!window_values_suggest_meeting(
            "com.electron.lark",
            Some("飞书消息"),
            900.0,
            700.0,
        ));
    }

    #[test]
    fn dedicated_meeting_apps_accept_a_meaningful_window() {
        assert!(window_values_suggest_meeting(
            "com.tencent.meeting",
            None,
            640.0,
            480.0,
        ));
        assert!(!window_values_suggest_meeting(
            "com.tencent.meeting",
            None,
            120.0,
            80.0,
        ));
    }

    #[test]
    fn explicit_meeting_titles_are_recognized_across_clients() {
        assert!(title_suggests_meeting(Some("腾讯会议")));
        assert!(title_suggests_meeting(Some("Weekly Product Call")));
        assert!(!title_suggests_meeting(Some("企业微信")));
    }

    #[test]
    fn frontmost_meeting_app_outranks_the_current_background_probe() {
        assert_eq!(
            choose_preferred_bundle(Some("com.tencent.meeting"), Some("com.tencent.wwmapp")),
            Some("com.tencent.meeting")
        );
        assert_eq!(
            choose_preferred_bundle(Some("cn.yaowutech.snack"), Some("com.tencent.wwmapp")),
            Some("com.tencent.wwmapp")
        );
        assert!(probe_target_changed(
            Some("com.tencent.wwmapp"),
            "com.tencent.meeting"
        ));
        assert!(!probe_target_changed(
            Some("com.tencent.meeting"),
            "com.tencent.meeting"
        ));
    }

    #[test]
    fn audio_activity_accumulates_and_quiet_periods_decay() {
        let active = next_active_audio_seconds(2.5, AUDIO_RMS_THRESHOLD, 0.75);
        assert!(active >= ACTIVE_AUDIO_TRIGGER_SECONDS);
        assert_eq!(next_active_audio_seconds(1.0, 0.0, 1.0), 0.5);
        assert_eq!(next_active_audio_seconds(0.2, 0.0, 1.0), 0.0);
    }

    #[test]
    fn timing_and_thresholds_match_the_reference_behavior() {
        assert_eq!(POLL_INTERVAL, Duration::from_secs(8));
        assert_eq!(COOLDOWN, Duration::from_secs(10 * 60));
        assert_eq!(AUDIO_RMS_THRESHOLD, 0.002);
        assert_eq!(ACTIVE_AUDIO_TRIGGER_SECONDS, 3.0);
    }
}
