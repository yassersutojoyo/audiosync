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
#[cfg(any(windows, test))]
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

/// The flyout's top-left corner on Linux, where the StatusNotifier host
/// reports the pointer but not the icon. It opens away from the `monitor`
/// edge nearest the pointer (the panel's), past a panel twice as thick as
/// the pointer is from that edge. At (0, 0), which hosts send when they
/// know no position, or off the monitor, it opens in the top-right corner.
#[cfg(any(target_os = "linux", test))]
pub fn pointer_flyout_origin(
    pointer: (f64, f64),
    width: f64,
    height: f64,
    monitor: Bounds,
    gap: f64,
) -> (f64, f64) {
    let _ = (pointer, width, height, monitor, gap);
    (0.0, 0.0)
}

/// The monitor holding the point, else the first one.
#[cfg(any(target_os = "linux", test))]
pub fn monitor_at(monitors: &[Bounds], x: f64, y: f64) -> Option<Bounds> {
    let _ = (x, y);
    monitors.last().copied()
}

/// A window's outer frame `frame` (physical pixels) as it opens in
/// `work_area`: no larger than the work area, and moved as little as needed
/// to lie wholly inside it; centered in it unless `keep_place` (a saved
/// place on a monitor that is gone means nothing here).
pub fn fit_window(frame: Bounds, work_area: Bounds, keep_place: bool) -> Bounds {
    let width = frame.width.min(work_area.width);
    let height = frame.height.min(work_area.height);
    let (x, y) = if keep_place {
        (frame.x, frame.y)
    } else {
        (
            work_area.x + (work_area.width - width) / 2.0,
            work_area.y + (work_area.height - height) / 2.0,
        )
    };
    Bounds {
        x: clamp(x, work_area.x, work_area.right() - width),
        y: clamp(y, work_area.y, work_area.bottom() - height),
        width,
        height,
    }
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

    const LAPTOP: Bounds = Bounds {
        x: 0.0,
        y: 0.0,
        width: 1366.0,
        height: 728.0,
    };

    fn frame(x: f64, y: f64, width: f64, height: f64) -> Bounds {
        Bounds {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_window_bigger_than_the_work_area_shrinks_to_it() {
        // Saved on a 4K monitor, opened on a laptop.
        let fitted = fit_window(frame(300.0, 200.0, 2500.0, 1400.0), LAPTOP, true);
        assert_eq!(fitted, LAPTOP);
    }

    #[test]
    fn a_window_partly_off_the_right_and_bottom_edges_moves_in() {
        let fitted = fit_window(frame(1000.0, 500.0, 900.0, 600.0), LAPTOP, true);
        assert_eq!(fitted, frame(1366.0 - 900.0, 728.0 - 600.0, 900.0, 600.0));
        // On a second monitor to the left, partly off its top-left corner.
        let left = frame(-1920.0, 0.0, 1920.0, 1040.0);
        let fitted = fit_window(frame(-1950.0, -20.0, 900.0, 600.0), left, true);
        assert_eq!(fitted, frame(-1920.0, 0.0, 900.0, 600.0));
    }

    #[test]
    fn a_window_that_fits_stays_where_it_was() {
        let saved = frame(100.0, 50.0, 900.0, 600.0);
        assert_eq!(fit_window(saved, LAPTOP, true), saved);
    }

    #[test]
    fn a_window_from_a_monitor_that_is_gone_opens_centered() {
        let fitted = fit_window(frame(5000.0, 3000.0, 2500.0, 600.0), LAPTOP, false);
        assert_eq!(fitted, frame(0.0, 64.0, 1366.0, 600.0));
    }

    const MONITOR: Bounds = Bounds {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    };

    #[test]
    fn pointer_flyout_opens_below_a_top_panel_and_above_a_bottom_one() {
        assert_eq!(
            pointer_flyout_origin((1800.0, 16.0), 320.0, 500.0, MONITOR, 12.0),
            (1920.0 - 12.0 - 320.0, 32.0 + 12.0)
        );
        assert_eq!(
            pointer_flyout_origin((900.0, 1060.0), 320.0, 500.0, MONITOR, 12.0),
            (900.0 - 160.0, 1040.0 - 12.0 - 500.0)
        );
    }

    #[test]
    fn pointer_flyout_opens_beside_a_side_panel() {
        assert_eq!(
            pointer_flyout_origin((20.0, 600.0), 320.0, 500.0, MONITOR, 12.0),
            (40.0 + 12.0, 350.0)
        );
        assert_eq!(
            pointer_flyout_origin((1900.0, 600.0), 320.0, 500.0, MONITOR, 12.0),
            (1880.0 - 12.0 - 320.0, 350.0)
        );
    }

    #[test]
    fn pointer_flyout_without_a_position_opens_top_right() {
        let corner = (1920.0 - 12.0 - 320.0, 12.0);
        assert_eq!(
            pointer_flyout_origin((0.0, 0.0), 320.0, 500.0, MONITOR, 12.0),
            corner
        );
        assert_eq!(
            pointer_flyout_origin((-50.0, 300.0), 320.0, 500.0, MONITOR, 12.0),
            corner
        );
    }

    #[test]
    fn monitor_at_picks_the_one_under_the_pointer_else_the_first() {
        let right = Bounds {
            x: 1920.0,
            ..MONITOR
        };
        assert_eq!(monitor_at(&[MONITOR, right], 2000.0, 10.0), Some(right));
        assert_eq!(monitor_at(&[MONITOR, right], -5.0, 10.0), Some(MONITOR));
        assert_eq!(monitor_at(&[], 0.0, 0.0), None);
    }
}
