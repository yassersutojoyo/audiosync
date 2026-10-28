//! How the tray flyout opens: it slides in from the taskbar's side and its
//! content fades in, as Windows 11's own taskbar flyouts do. With Windows
//! animations off (Settings > Accessibility > Visual effects) it shows at
//! once.

use std::time::Duration;

use crate::placement::Edge;

/// The slide, decelerating to a stop. It outlasts the fade, so the last
/// pixels of travel happen with the content already shown.
pub const SLIDE: Duration = Duration::from_millis(250);
/// The content's fade, Fluent's 167 ms (`Fluent.normal` in widgets.slint).
pub const FADE: Duration = Duration::from_millis(167);
/// How far the flyout travels, in logical pixels: short enough that it
/// reads as settling into place rather than flying in.
pub const DISTANCE: f64 = 16.0;

/// An open motion under way, from where the flyout starts relative to its
/// place, in logical pixels; each tick scales it by the window's scale
/// then, which is the icon monitor's once the window is there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slide {
    from: (f64, f64),
}

/// What one tick of the motion does to the flyout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tick {
    /// `offset` physical pixels from its place, the content at `opacity`.
    Move { offset: (i32, i32), opacity: f32 },
    /// The motion ended: in place, the content fully shown.
    Settle,
    /// It closed mid-motion: the motion stops and the content stays faded,
    /// so the next open never shows it at full strength first.
    Abandon,
}

/// The motion for a flyout opening from `edge`; `None` opens it in place.
/// `animations` is Windows' setting; one Windows didn't report counts as
/// off, so a motion-sensitive user never gets motion by mistake.
pub fn plan(animations: Option<bool>, edge: Edge) -> Option<Slide> {
    if animations != Some(true) {
        return None;
    }
    let from = match edge {
        Edge::Bottom => (0.0, DISTANCE),
        Edge::Top => (0.0, -DISTANCE),
        Edge::Right => (DISTANCE, 0.0),
        Edge::Left => (-DISTANCE, 0.0),
    };
    Some(Slide { from })
}

impl Slide {
    /// Where the flyout is `elapsed` into its motion at `scale`; `visible`
    /// says whether it is still open.
    pub fn tick(self, elapsed: Duration, scale: f64, visible: bool) -> Tick {
        if !visible {
            return Tick::Abandon;
        }
        if elapsed >= SLIDE && elapsed >= FADE {
            return Tick::Settle;
        }
        let left = (1.0 - decelerate(fraction(elapsed, SLIDE))) * scale;
        Tick::Move {
            offset: (
                (self.from.0 * left).round() as i32,
                (self.from.1 * left).round() as i32,
            ),
            opacity: fraction(elapsed, FADE) as f32,
        }
    }
}

fn fraction(elapsed: Duration, of: Duration) -> f64 {
    (elapsed.as_secs_f64() / of.as_secs_f64()).clamp(0.0, 1.0)
}

/// Fluent's decelerate curve, cubic-bezier(0, 0, 0, 1), at time `t`. With
/// those control points x = s^3 and y = 3s^2 - 2s^3, so s is a cube root.
fn decelerate(t: f64) -> f64 {
    let s = t.cbrt();
    3.0 * s * s - 2.0 * s * s * s
}

/// Whether Windows animates controls and elements
/// (SPI_GETCLIENTAREAANIMATION); `None` when it didn't say.
#[cfg(windows)]
pub fn animations_on() -> Option<bool> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    use windows::core::BOOL;

    let mut on = BOOL(0);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer,
    // which points at `on` for the whole call.
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut on).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()
    .map(|()| on.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn moved(tick: Tick) -> ((i32, i32), f32) {
        match tick {
            Tick::Move { offset, opacity } => (offset, opacity),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_motion_when_windows_animations_are_off_or_unknown() {
        assert_eq!(plan(Some(false), Edge::Bottom), None);
        assert_eq!(plan(None, Edge::Bottom), None);
        assert!(plan(Some(true), Edge::Bottom).is_some());
    }

    #[test]
    fn it_starts_on_the_taskbars_side_scaled_and_faded_out() {
        let start = |edge, scale| {
            moved(
                plan(Some(true), edge)
                    .unwrap()
                    .tick(Duration::ZERO, scale, true),
            )
        };
        assert_eq!(start(Edge::Bottom, 1.5), ((0, 24), 0.0));
        assert_eq!(start(Edge::Top, 1.5).0, (0, -24));
        assert_eq!(start(Edge::Right, 1.5).0, (24, 0));
        assert_eq!(start(Edge::Left, 1.5).0, (-24, 0));
        // The scale is the window's at each tick, not at the click.
        assert_eq!(start(Edge::Bottom, 2.0).0, (0, 32));
    }

    #[test]
    fn it_decelerates_into_place_and_fades_in_first() {
        let slide = plan(Some(true), Edge::Bottom).unwrap();
        let early = moved(slide.tick(ms(50), 4.0, true)).0.1;
        let late = moved(slide.tick(ms(200), 4.0, true)).0.1;
        assert!(64 - early > late, "most of the way is covered early");
        assert!(late > 0 && late < 4, "{late}");

        let (offset, opacity) = moved(slide.tick(FADE, 4.0, true));
        assert_eq!(opacity, 1.0);
        assert!(offset.1 > 0, "still settling");

        assert_eq!(slide.tick(SLIDE, 1.0, true), Tick::Settle);
        assert_eq!(slide.tick(ms(400), 1.0, true), Tick::Settle);
    }

    #[test]
    fn a_close_mid_motion_leaves_the_content_faded() {
        let slide = plan(Some(true), Edge::Bottom).unwrap();
        assert_eq!(slide.tick(ms(60), 1.0, false), Tick::Abandon);
        assert_eq!(slide.tick(ms(400), 1.0, false), Tick::Abandon);
    }

    #[test]
    fn the_curve_runs_from_zero_to_one() {
        assert_eq!(decelerate(0.0), 0.0);
        assert_eq!(decelerate(1.0), 1.0);
        assert!(
            (decelerate(0.125) - 0.5).abs() < 1e-12,
            "s = 0.5 at t = 1/8"
        );
    }
}
