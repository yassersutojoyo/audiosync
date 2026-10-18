//! Where the tray flyout opens: next to the notification-area icon, on the
//! side away from the taskbar, inside the monitor's work area.

/// A rectangle in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Bounds {
    fn right(&self) -> f64 {
        self.x + self.width
    }

    fn bottom(&self) -> f64 {
        self.y + self.height
    }
}

/// The flyout's top-left corner for a flyout of `width` x `height` next to
/// `icon`, in `work_area` (the monitor minus the taskbar), `gap` away from
/// the taskbar and the screen edges. All in physical pixels.
pub fn flyout_origin(
    icon: Bounds,
    width: f64,
    height: f64,
    work_area: Bounds,
    gap: f64,
) -> (f64, f64) {
    let center_x = icon.x + icon.width / 2.0;
    let center_y = icon.y + icon.height / 2.0;
    let (x, y) = if icon.y >= work_area.bottom() {
        // Taskbar at the bottom (the default): open above the icon.
        (center_x - width / 2.0, work_area.bottom() - gap - height)
    } else if icon.bottom() <= work_area.y {
        // Taskbar at the top.
        (center_x - width / 2.0, work_area.y + gap)
    } else if icon.x >= work_area.right() {
        // Taskbar on the right.
        (work_area.right() - gap - width, center_y - height / 2.0)
    } else if icon.right() <= work_area.x {
        // Taskbar on the left.
        (work_area.x + gap, center_y - height / 2.0)
    } else {
        // An overflow icon or an auto-hidden taskbar: above the icon.
        (center_x - width / 2.0, icon.y - gap - height)
    };
    (
        clamp(x, work_area.x + gap, work_area.right() - gap - width),
        clamp(y, work_area.y + gap, work_area.bottom() - gap - height),
    )
}

/// `value` within `low..=high`; `low` when the range is empty (a flyout larger
/// than the work area keeps its top-left corner on screen).
fn clamp(value: f64, low: f64, high: f64) -> f64 {
    value.min(high).max(low)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Bounds = Bounds {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1032.0,
    };

    fn icon(x: f64, y: f64) -> Bounds {
        Bounds {
            x,
            y,
            width: 24.0,
            height: 24.0,
        }
    }

    #[test]
    fn above_a_bottom_taskbar_centered_on_the_icon() {
        let (x, y) = flyout_origin(icon(1500.0, 1044.0), 320.0, 500.0, SCREEN, 12.0);
        assert_eq!((x, y), (1512.0 - 160.0, 1032.0 - 12.0 - 500.0));
    }

    #[test]
    fn kept_inside_the_work_area_near_the_corner() {
        let (x, _) = flyout_origin(icon(1890.0, 1044.0), 320.0, 500.0, SCREEN, 12.0);
        assert_eq!(x, 1920.0 - 12.0 - 320.0);
    }

    #[test]
    fn below_a_top_taskbar_and_beside_side_taskbars() {
        let work = Bounds {
            x: 0.0,
            y: 48.0,
            width: 1920.0,
            height: 1032.0,
        };
        assert_eq!(
            flyout_origin(icon(800.0, 12.0), 320.0, 500.0, work, 12.0).1,
            60.0
        );
        let work = Bounds {
            x: 0.0,
            y: 0.0,
            width: 1872.0,
            height: 1080.0,
        };
        assert_eq!(
            flyout_origin(icon(1884.0, 900.0), 320.0, 500.0, work, 12.0).0,
            1872.0 - 12.0 - 320.0
        );
        let work = Bounds {
            x: 48.0,
            y: 0.0,
            width: 1872.0,
            height: 1080.0,
        };
        assert_eq!(
            flyout_origin(icon(12.0, 900.0), 320.0, 500.0, work, 12.0).0,
            60.0
        );
    }

    #[test]
    fn a_flyout_taller_than_the_screen_keeps_its_top_on_screen() {
        let (_, y) = flyout_origin(icon(1500.0, 1044.0), 320.0, 2000.0, SCREEN, 12.0);
        assert_eq!(y, 12.0);
    }
}
