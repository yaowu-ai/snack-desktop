//! One transparent window contains the compact capsule and its temporary card stack.

pub(super) const WINDOW_WIDTH: f64 = 263.0;
pub(super) const WINDOW_HEIGHT: f64 = 192.0;
pub(super) const CAPSULE_LEFT: f64 = 33.0;
pub(super) const CAPSULE_TOP: f64 = 13.0;
pub(super) const CAPSULE_WIDTH: f64 = 197.0;
pub(super) const CAPSULE_HEIGHT: f64 = 72.0;
pub(super) const STACK_OFFSET: f64 = 84.0;
pub(super) const NOTICE_WIDTH: f64 = 188.0;
pub(super) const NOTICE_HEIGHT: f64 = 64.0;

pub(super) fn capsule_top(stack_above: bool) -> f64 {
    CAPSULE_TOP + if stack_above { STACK_OFFSET } else { 0.0 }
}

#[derive(Clone, Copy)]
pub(super) struct WorkArea {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Anchor the visible capsule 24 logical pixels from the top and right work-area edges.
pub(super) fn default_position(area: WorkArea, scale: f64) -> (i32, i32) {
    let x = area.x + area.width as i32 - scaled(CAPSULE_LEFT + CAPSULE_WIDTH + 24.0, scale);
    let y = area.y + scaled(24.0 - CAPSULE_TOP, scale);
    (x.max(area.x), y)
}

/// A hidden macOS window can still report another monitor's backing scale.
/// Resolve logical coordinates using the target monitor before calling its native setter.
pub(super) fn native_position(target: (i32, i32), _target_scale: f64) -> tauri::Position {
    #[cfg(target_os = "macos")]
    {
        tauri::LogicalPosition::new(
            target.0 as f64 / _target_scale,
            target.1 as f64 / _target_scale,
        )
        .into()
    }
    #[cfg(not(target_os = "macos"))]
    {
        tauri::PhysicalPosition::new(target.0, target.1).into()
    }
}

/// Changing the available expansion side moves the transparent window, not the visible capsule.
/// Only the card stack is placed above/below; the user's dragged capsule position is preserved.
pub(super) fn stack_position(
    area: WorkArea,
    overlay: (i32, i32),
    scale: f64,
    was_above: bool,
    stack_active: bool,
) -> ((i32, i32), bool) {
    let top = overlay.1 + scaled(capsule_top(was_above), scale);
    let above = stack_active && top - scaled(STACK_OFFSET, scale) >= area.y;
    ((overlay.0, top - scaled(capsule_top(above), scale)), above)
}

fn scaled(value: f64, scale: f64) -> i32 {
    (value * scale).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> WorkArea {
        WorkArea {
            x: 0,
            y: 24,
            width: 1440,
            height: 876,
        }
    }

    #[test]
    fn default_capsule_has_24_pixel_top_and_right_margins() {
        assert_eq!(default_position(area(), 1.0), (1186, 35));
        assert_eq!(WINDOW_HEIGHT, 192.0);
        assert_eq!(capsule_top(false), 13.0);
        assert_eq!(capsule_top(true), 97.0);
        assert_eq!(NOTICE_WIDTH, 188.0);
        assert_eq!(NOTICE_HEIGHT, 64.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn initial_retina_position_does_not_depend_on_hidden_window_scale() {
        let screen = WorkArea {
            x: 0,
            y: 68,
            width: 2940,
            height: 1736,
        };
        let target = default_position(screen, 2.0);
        assert_eq!(target, (2432, 90));
        let position = native_position(target, 2.0);
        let hidden_window = position.to_logical::<f64>(1.0);
        let visible_window = position.to_logical::<f64>(2.0);
        assert_eq!(hidden_window, tauri::LogicalPosition::new(1216.0, 45.0));
        assert_eq!(hidden_window, visible_window);
    }

    #[test]
    fn native_position_preserves_unscaled_monitor_coordinates() {
        let target = default_position(area(), 1.0);
        assert_eq!(
            native_position(target, 1.0).to_logical::<f64>(1.0),
            tauri::LogicalPosition::new(1186.0, 35.0)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_position_preserves_negative_retina_and_stack_coordinates() {
        let screen = WorkArea {
            x: -2880,
            y: -400,
            width: 2880,
            height: 1800,
        };
        let (target, above) = stack_position(screen, (-508, 0), 2.0, false, true);
        assert!(above);
        let position = native_position(target, 2.0);
        assert_eq!(
            position.to_logical::<f64>(1.0),
            tauri::LogicalPosition::new(-254.0, -84.0)
        );
        assert_eq!(
            position.to_logical::<f64>(1.0),
            position.to_logical::<f64>(2.0)
        );
    }

    #[test]
    fn top_edge_uses_the_space_below_without_moving_the_capsule() {
        let origin = default_position(area(), 1.0);
        assert_eq!(
            stack_position(area(), origin, 1.0, false, true),
            (origin, false)
        );
    }

    #[test]
    fn dragging_down_expands_above_and_keeps_the_visible_capsule_still() {
        let (origin, above) = stack_position(area(), (600, 300), 1.0, false, true);
        assert!(above);
        assert_eq!(origin, (600, 216));
        assert_eq!(origin.1 + capsule_top(above) as i32, 313);
        assert_eq!(
            stack_position(area(), origin, 1.0, true, true),
            (origin, true)
        );
        let (single, above) = stack_position(area(), origin, 1.0, true, false);
        assert_eq!(single, (600, 300));
        assert!(!above);
    }

    #[test]
    fn changing_sides_does_not_shift_the_user_dragged_capsule() {
        let (origin, above) = stack_position(area(), (600, 0), 1.0, true, true);
        assert_eq!(origin, (600, 84));
        assert!(!above);
        assert_eq!(origin.1 + capsule_top(above) as i32, 97);
    }

    #[test]
    fn high_dpi_and_negative_monitor_coordinates_preserve_capsule_position() {
        let screen = WorkArea {
            x: -2880,
            y: -400,
            width: 2880,
            height: 1800,
        };
        let initial = (-508, 0);
        let (origin, above) = stack_position(screen, initial, 2.0, false, true);
        assert_eq!(origin, (-508, -168));
        assert!(above);
        assert_eq!(origin.1 + scaled(capsule_top(above), 2.0), initial.1 + 26);
        assert_eq!(
            stack_position(screen, origin, 2.0, true, false),
            (initial, false)
        );
    }

    #[test]
    fn unusually_small_work_area_has_a_stable_below_fallback() {
        let screen = WorkArea {
            x: -20,
            y: 10,
            width: 240,
            height: 80,
        };
        assert_eq!(
            stack_position(screen, (0, 12), 1.0, false, true),
            ((0, 12), false)
        );
    }
}
