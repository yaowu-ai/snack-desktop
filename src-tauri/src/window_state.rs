use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewWindow, WindowEvent,
};

use crate::logging;

const STATE_FILE_NAME: &str = "window-state.json";
const STATE_VERSION: u8 = 1;
const WRITE_DEBOUNCE: Duration = Duration::from_millis(300);
const MONITOR_DETECTION_TIMEOUT: Duration = Duration::from_millis(150);
const DEFAULT_WINDOW_MARGIN: u32 = 48;
const MIN_WIDTH: u32 = 960;
const MIN_HEIGHT: u32 = 640;
const MAX_DIMENSION: u32 = 32_768;
const MAX_COORDINATE: i32 = 1_000_000;
const MIN_REACHABLE_TITLE_WIDTH: i64 = 120;
const MIN_REACHABLE_TITLE_HEIGHT: i64 = 24;
const TITLE_BAR_HEIGHT: u32 = 48;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct PersistedWindowState {
    version: u8,
    bounds: WindowBounds,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct WorkArea {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug)]
struct MonitorSnapshot {
    work_areas: Vec<WorkArea>,
    primary: Option<WorkArea>,
    scale_factor: f64,
}

impl From<&Monitor> for WorkArea {
    fn from(monitor: &Monitor) -> Self {
        let area = monitor.work_area();
        Self {
            x: area.position.x,
            y: area.position.y,
            width: area.size.width,
            height: area.size.height,
        }
    }
}

enum PersistSignal {
    Changed,
    Shutdown,
}

pub(crate) fn restore_track_and_show(app: &AppHandle, window: &WebviewWindow) {
    let path = state_path(app);

    let saved_bounds = match load(&path) {
        Ok(Some(saved)) if saved.version == STATE_VERSION => Some(saved.bounds),
        Ok(Some(_)) => {
            logging::write_app_log(
                app,
                "warn",
                "window-state",
                "Ignoring unsupported window state version",
                None,
            );
            None
        }
        Ok(None) => None,
        Err(error) => {
            logging::write_app_log(
                app,
                "warn",
                "window-state",
                "Ignoring invalid saved window state",
                Some(&serde_json::json!({ "error": error })),
            );
            None
        }
    };

    let worker_app = app.clone();
    let worker_window = window.clone();
    let detection_window = window.clone();
    let (detection_sender, detection_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = detection_sender.send(detect_monitors(&detection_window));
    });
    thread::spawn(move || {
        match detection_receiver.recv_timeout(MONITOR_DETECTION_TIMEOUT) {
            Ok(Ok(snapshot)) => {
                apply_initial_bounds(&worker_app, &worker_window, saved_bounds, snapshot);
            }
            Err(RecvTimeoutError::Timeout) => logging::write_app_log(
                &worker_app,
                "warn",
                "window-state",
                "Display detection timed out; using configured window bounds",
                Some(&serde_json::json!({
                    "timeoutMs": MONITOR_DETECTION_TIMEOUT.as_millis()
                })),
            ),
            Ok(Err(error)) => logging::write_app_log(
                &worker_app,
                "warn",
                "window-state",
                "Failed to detect displays; using configured window bounds",
                Some(&serde_json::json!({ "error": error.to_string() })),
            ),
            Err(RecvTimeoutError::Disconnected) => {}
        }

        if let Err(error) = worker_window.show() {
            logging::write_app_log(
                &worker_app,
                "error",
                "window-state",
                "Failed to show the main window",
                Some(&serde_json::json!({ "error": error.to_string() })),
            );
        }
        track(&worker_app, &worker_window, path);
    });
}

pub(crate) fn recover_if_unreachable(window: &WebviewWindow) {
    let (Ok(position), Ok(size), Ok(monitors)) = (
        window.outer_position(),
        window.outer_size(),
        window.available_monitors(),
    ) else {
        return;
    };

    let current = WindowBounds {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    };
    let work_areas = monitors.iter().map(WorkArea::from).collect::<Vec<_>>();
    if has_reachable_title_bar(current, &work_areas) {
        return;
    }

    let Ok(Some(primary)) = window.primary_monitor() else {
        return;
    };
    let recovered = centered_bounds(current, WorkArea::from(&primary));
    let _ = window.set_position(PhysicalPosition::new(recovered.x, recovered.y));
}

fn apply_initial_bounds(
    app: &AppHandle,
    window: &WebviewWindow,
    saved: Option<WindowBounds>,
    snapshot: MonitorSnapshot,
) {
    let bounds = match saved {
        Some(saved) => {
            let Some(bounds) = safe_restored_bounds(saved, &snapshot.work_areas, snapshot.primary)
            else {
                logging::write_app_log(
                    app,
                    "warn",
                    "window-state",
                    "Ignoring implausible saved window bounds",
                    Some(&serde_json::json!({ "bounds": saved })),
                );
                return apply_default_bounds(window, snapshot);
            };
            bounds
        }
        None => return apply_default_bounds(window, snapshot),
    };

    set_bounds(app, window, bounds, "restore");
}

fn detect_monitors(window: &WebviewWindow) -> tauri::Result<MonitorSnapshot> {
    let monitors = window.available_monitors()?;
    let primary = window.primary_monitor()?;
    let scale_factor = window.scale_factor()?;
    Ok(MonitorSnapshot {
        work_areas: monitors.iter().map(WorkArea::from).collect(),
        primary: primary.as_ref().map(WorkArea::from),
        scale_factor,
    })
}

fn apply_default_bounds(window: &WebviewWindow, snapshot: MonitorSnapshot) {
    let (Ok(size), Some(primary)) = (window.inner_size(), snapshot.primary) else {
        return;
    };
    let margin = (f64::from(DEFAULT_WINDOW_MARGIN) * snapshot.scale_factor).round() as u32;
    let bounds = default_bounds(size, primary, margin);
    let _ = window.set_size(PhysicalSize::new(bounds.width, bounds.height));
    let _ = window.set_position(PhysicalPosition::new(bounds.x, bounds.y));
}

fn set_bounds(app: &AppHandle, window: &WebviewWindow, bounds: WindowBounds, action: &str) {
    if let Err(error) = window.set_size(PhysicalSize::new(bounds.width, bounds.height)) {
        logging::write_app_log(
            app,
            "warn",
            "window-state",
            "Failed to apply window size",
            Some(&serde_json::json!({ "action": action, "error": error.to_string() })),
        );
    }
    if let Err(error) = window.set_position(PhysicalPosition::new(bounds.x, bounds.y)) {
        logging::write_app_log(
            app,
            "warn",
            "window-state",
            "Failed to apply window position",
            Some(&serde_json::json!({ "action": action, "error": error.to_string() })),
        );
    }
}

fn track(app: &AppHandle, window: &WebviewWindow, path: PathBuf) {
    let (Ok(position), Ok(size)) = (window.outer_position(), window.inner_size()) else {
        return;
    };
    let bounds = Arc::new(Mutex::new(WindowBounds {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    }));
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker_bounds = Arc::clone(&bounds);
    let worker_app = app.clone();

    thread::spawn(move || loop {
        match receiver.recv() {
            Ok(PersistSignal::Changed) => loop {
                match receiver.recv_timeout(WRITE_DEBOUNCE) {
                    Ok(PersistSignal::Changed) => {}
                    Ok(PersistSignal::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                        persist_latest(&worker_app, &path, &worker_bounds);
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        persist_latest(&worker_app, &path, &worker_bounds);
                        break;
                    }
                }
            },
            Ok(PersistSignal::Shutdown) | Err(_) => {
                persist_latest(&worker_app, &path, &worker_bounds);
                return;
            }
        }
    });

    let event_window = window.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::Resized(size) if is_normal_window(&event_window) => {
            update_size(&bounds, *size);
            signal_change(&sender);
        }
        WindowEvent::Moved(position) if is_normal_window(&event_window) => {
            update_position(&bounds, *position);
            signal_change(&sender);
        }
        WindowEvent::ScaleFactorChanged { new_inner_size, .. }
            if is_normal_window(&event_window) =>
        {
            update_size(&bounds, *new_inner_size);
            signal_change(&sender);
        }
        WindowEvent::Destroyed => {
            let _ = sender.send(PersistSignal::Shutdown);
        }
        _ => {}
    });
}

fn is_normal_window(window: &WebviewWindow) -> bool {
    matches!(window.is_minimized(), Ok(false))
        && matches!(window.is_maximized(), Ok(false))
        && matches!(window.is_fullscreen(), Ok(false))
}

fn update_position(bounds: &Mutex<WindowBounds>, position: PhysicalPosition<i32>) {
    let mut bounds = bounds.lock().unwrap_or_else(|error| error.into_inner());
    bounds.x = position.x;
    bounds.y = position.y;
}

fn update_size(bounds: &Mutex<WindowBounds>, size: PhysicalSize<u32>) {
    let mut bounds = bounds.lock().unwrap_or_else(|error| error.into_inner());
    bounds.width = size.width;
    bounds.height = size.height;
}

fn signal_change(sender: &SyncSender<PersistSignal>) {
    match sender.try_send(PersistSignal::Changed) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

fn persist_latest(app: &AppHandle, path: &Path, bounds: &Mutex<WindowBounds>) {
    let bounds = *bounds.lock().unwrap_or_else(|error| error.into_inner());
    if !is_plausible(bounds) {
        return;
    }
    let state = PersistedWindowState {
        version: STATE_VERSION,
        bounds,
    };
    if let Err(error) = persist(path, &state) {
        logging::write_app_log(
            app,
            "warn",
            "window-state",
            "Failed to persist window state",
            Some(&serde_json::json!({ "error": error })),
        );
    }
}

fn state_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("snack"))
        .join(STATE_FILE_NAME)
}

fn load(path: &Path) -> Result<Option<PersistedWindowState>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn persist(path: &Path, state: &PersistedWindowState) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| "window state path has no parent".to_string())?;
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(state).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| error.to_string())
}

fn safe_restored_bounds(
    saved: WindowBounds,
    work_areas: &[WorkArea],
    primary: Option<WorkArea>,
) -> Option<WindowBounds> {
    if !is_plausible(saved) {
        return None;
    }

    if let Some(area) = work_areas.iter().copied().find(|area| {
        title_bar_intersection(saved, *area).0 >= MIN_REACHABLE_TITLE_WIDTH
            && title_bar_intersection(saved, *area).1 >= MIN_REACHABLE_TITLE_HEIGHT
    }) {
        return Some(fit_oversized_window(saved, area));
    }

    primary.map(|area| centered_bounds(saved, area))
}

fn is_plausible(bounds: WindowBounds) -> bool {
    bounds.width >= MIN_WIDTH
        && bounds.height >= MIN_HEIGHT
        && bounds.width <= MAX_DIMENSION
        && bounds.height <= MAX_DIMENSION
        && bounds.x.unsigned_abs() <= MAX_COORDINATE as u32
        && bounds.y.unsigned_abs() <= MAX_COORDINATE as u32
}

fn has_reachable_title_bar(bounds: WindowBounds, work_areas: &[WorkArea]) -> bool {
    work_areas.iter().any(|area| {
        let (width, height) = title_bar_intersection(bounds, *area);
        width >= MIN_REACHABLE_TITLE_WIDTH && height >= MIN_REACHABLE_TITLE_HEIGHT
    })
}

fn title_bar_intersection(bounds: WindowBounds, area: WorkArea) -> (i64, i64) {
    let title_right = i64::from(bounds.x) + i64::from(bounds.width);
    let title_bottom = i64::from(bounds.y) + i64::from(TITLE_BAR_HEIGHT.min(bounds.height));
    let area_right = i64::from(area.x) + i64::from(area.width);
    let area_bottom = i64::from(area.y) + i64::from(area.height);
    let width = (title_right.min(area_right) - i64::from(bounds.x).max(i64::from(area.x))).max(0);
    let height =
        (title_bottom.min(area_bottom) - i64::from(bounds.y).max(i64::from(area.y))).max(0);
    (width, height)
}

fn fit_oversized_window(mut bounds: WindowBounds, area: WorkArea) -> WindowBounds {
    if bounds.width > area.width {
        bounds.width = area.width;
        bounds.x = area.x;
    }
    if bounds.height > area.height {
        bounds.height = area.height;
        bounds.y = area.y;
    }
    bounds
}

fn centered_bounds(mut bounds: WindowBounds, area: WorkArea) -> WindowBounds {
    bounds.width = bounds.width.min(area.width);
    bounds.height = bounds.height.min(area.height);
    bounds.x =
        i64_to_i32(i64::from(area.x) + (i64::from(area.width) - i64::from(bounds.width)) / 2);
    bounds.y =
        i64_to_i32(i64::from(area.y) + (i64::from(area.height) - i64::from(bounds.height)) / 2);
    bounds
}

fn default_bounds(size: PhysicalSize<u32>, area: WorkArea, margin: u32) -> WindowBounds {
    let available_width = area.width.saturating_sub(margin.saturating_mul(2));
    let available_height = area.height.saturating_sub(margin.saturating_mul(2));
    centered_bounds(
        WindowBounds {
            x: area.x,
            y: area.y,
            width: size.width.min(available_width.max(MIN_WIDTH)),
            height: size.height.min(available_height.max(MIN_HEIGHT)),
        },
        area,
    )
}

fn i64_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tauri::PhysicalSize;

    use super::{
        default_bounds, has_reachable_title_bar, load, persist, safe_restored_bounds,
        PersistedWindowState, WindowBounds, WorkArea, MIN_HEIGHT, MIN_WIDTH, STATE_VERSION,
    };

    static TEMP_FILE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    const PRIMARY: WorkArea = WorkArea {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn preserves_reachable_bounds() {
        let saved = WindowBounds {
            x: 100,
            y: 80,
            width: 1280,
            height: 800,
        };

        assert_eq!(
            safe_restored_bounds(saved, &[PRIMARY], Some(PRIMARY)),
            Some(saved)
        );
    }

    #[test]
    fn recenters_a_window_with_only_a_corner_visible() {
        let saved = WindowBounds {
            x: 1880,
            y: 1040,
            width: 1280,
            height: 800,
        };

        assert_eq!(
            safe_restored_bounds(saved, &[PRIMARY], Some(PRIMARY)),
            Some(WindowBounds {
                x: 320,
                y: 140,
                width: 1280,
                height: 800,
            })
        );
    }

    #[test]
    fn accepts_negative_coordinates_on_a_secondary_monitor() {
        let secondary = WorkArea {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let saved = WindowBounds {
            x: -1700,
            y: 100,
            width: 1280,
            height: 800,
        };

        assert_eq!(
            safe_restored_bounds(saved, &[PRIMARY, secondary], Some(PRIMARY)),
            Some(saved)
        );
    }

    #[test]
    fn default_size_leaves_a_margin_on_a_small_display() {
        let area = WorkArea {
            x: -1366,
            y: 20,
            width: 1366,
            height: 748,
        };

        assert_eq!(
            default_bounds(PhysicalSize::new(1280, 860), area, 48),
            WindowBounds {
                x: -1318,
                y: 68,
                width: 1270,
                height: 652,
            }
        );
    }

    #[test]
    fn default_size_does_not_grow_on_a_large_display() {
        assert_eq!(
            default_bounds(PhysicalSize::new(1280, 860), PRIMARY, 48),
            WindowBounds {
                x: 320,
                y: 110,
                width: 1280,
                height: 860,
            }
        );
    }

    #[test]
    fn rejects_corrupt_or_too_small_dimensions() {
        let saved = WindowBounds {
            x: 0,
            y: 0,
            width: MIN_WIDTH - 1,
            height: MIN_HEIGHT - 1,
        };

        assert_eq!(safe_restored_bounds(saved, &[PRIMARY], Some(PRIMARY)), None);
    }

    #[test]
    fn title_bar_must_be_reachable_not_just_the_window_body() {
        let bounds = WindowBounds {
            x: 200,
            y: -500,
            width: 1280,
            height: 800,
        };

        assert!(!has_reachable_title_bar(bounds, &[PRIMARY]));
    }

    #[test]
    fn persists_and_loads_window_state() {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "snack-window-state-{}-{sequence}.json",
            std::process::id()
        ));
        let expected = WindowBounds {
            x: -1200,
            y: 50,
            width: 1280,
            height: 800,
        };

        persist(
            &path,
            &PersistedWindowState {
                version: STATE_VERSION,
                bounds: expected,
            },
        )
        .unwrap();
        let loaded = load(&path).unwrap().unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.bounds, expected);
    }

    #[test]
    fn rejects_the_minimum_signed_coordinate_without_overflowing() {
        let bounds = WindowBounds {
            x: i32::MIN,
            y: i32::MIN,
            width: 1280,
            height: 800,
        };

        assert_eq!(
            safe_restored_bounds(bounds, &[PRIMARY], Some(PRIMARY)),
            None
        );
    }
}
