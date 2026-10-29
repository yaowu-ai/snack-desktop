//! Physical positioning for the compact capsule and its independent passive notice.

pub(super) const WINDOW_WIDTH: f64 = 263.0;
pub(super) const WINDOW_HEIGHT: f64 = 148.0;
pub(super) const CAPSULE_LEFT: f64 = 33.0;
pub(super) const CAPSULE_TOP: f64 = 13.0;
pub(super) const CAPSULE_WIDTH: f64 = 197.0;
pub(super) const CAPSULE_HEIGHT: f64 = 72.0;
pub(super) const NOTICE_WIDTH: f64 = 360.0;
pub(super) const NOTICE_HEIGHT: f64 = 96.0;

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

/// Keep the notice separate from the capsule; changing notice visibility never moves the capsule.
pub(super) fn notice_position(
    area: WorkArea,
    overlay: (i32, i32),
    scale: f64,
    notice_size: (u32, u32),
) -> (i32, i32) {
    let (width, height) = (notice_size.0 as i32, notice_size.1 as i32);
    let center = overlay.0 + scaled(CAPSULE_LEFT + CAPSULE_WIDTH / 2.0, scale);
    let top = overlay.1 + scaled(CAPSULE_TOP, scale);
    let above = top - scaled(8.0, scale) - height;
    let below = top + scaled(CAPSULE_HEIGHT + 8.0, scale);
    let y = if above >= area.y { above } else { below };
    (
        clamp_to_area(center - width / 2, area.x, area.width, width),
        clamp_to_area(y, area.y, area.height, height),
    )
}

fn scaled(value: f64, scale: f64) -> i32 {
    (value * scale).round() as i32
}

fn clamp_to_area(position: i32, start: i32, size: u32, item_size: i32) -> i32 {
    position.clamp(start, (start + size as i32 - item_size).max(start))
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
        assert_eq!(WINDOW_HEIGHT - CAPSULE_TOP - CAPSULE_HEIGHT, 63.0);
    }

    #[test]
    fn notice_is_below_a_capsule_at_the_top_without_moving_it() {
        let overlay = default_position(area(), 1.0);
        assert_eq!(
            notice_position(area(), overlay, 1.0, (360, 96)),
            (1080, 128)
        );
        assert_eq!(overlay, (1186, 35));
    }

    #[test]
    fn notice_prefers_above_when_the_user_dragged_the_capsule_down() {
        assert_eq!(
            notice_position(area(), (600, 300), 1.0, (360, 96)),
            (552, 209)
        );
    }

    #[test]
    fn notice_clamps_at_both_horizontal_edges() {
        assert_eq!(notice_position(area(), (-100, 300), 1.0, (360, 96)).0, 0);
        assert_eq!(notice_position(area(), (1400, 300), 1.0, (360, 96)).0, 1080);
    }

    #[test]
    fn scaled_secondary_monitor_coordinates_stay_in_its_work_area() {
        let area = WorkArea {
            x: -2880,
            y: -400,
            width: 2880,
            height: 1800,
        };
        let overlay = default_position(area, 2.0);
        assert_eq!(overlay, (-508, -378));
        assert_eq!(
            notice_position(area, overlay, 2.0, (720, 192)),
            (-720, -192)
        );
    }

    #[test]
    fn notice_at_bottom_uses_available_space_above() {
        assert_eq!(
            notice_position(area(), (600, 820), 1.0, (360, 96)),
            (552, 729)
        );
    }

    #[test]
    fn unusually_small_work_area_never_panics_or_places_its_origin_outside() {
        let area = WorkArea {
            x: -20,
            y: 10,
            width: 240,
            height: 80,
        };
        assert_eq!(notice_position(area, (0, 12), 1.0, (360, 96)), (-20, 10));
    }
}
