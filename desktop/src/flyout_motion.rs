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
/// place, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slide {
    from: (f64, f64),
}

/// Where the flyout is at one moment of its motion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    /// How far from its place, in physical pixels (rounded by the caller).
    pub offset: (f64, f64),
    pub opacity: f32,
    pub done: bool,
}

/// The motion for a flyout opening from `edge` at `scale`; `None` opens it
/// in place. `animations` is Windows' setting; one Windows didn't report
/// counts as off, so a motion-sensitive user never gets motion by mistake.
pub fn plan(animations: Option<bool>, edge: Edge, scale: f64) -> Option<Slide> {
    if animations != Some(true) {
        return None;
    }
    let distance = DISTANCE * scale;
    let from = match edge {
        Edge::Bottom => (0.0, distance),
        Edge::Top => (0.0, -distance),
        Edge::Right => (distance, 0.0),
        Edge::Left => (-distance, 0.0),
    };
    Some(Slide { from })
}

impl Slide {
    pub fn at(self, elapsed: Duration) -> Step {
        let left = 1.0 - decelerate(fraction(elapsed, SLIDE));
        Step {
            offset: (self.from.0 * left, self.from.1 * left),
            opacity: fraction(elapsed, FADE) as f32,
            done: elapsed >= SLIDE && elapsed >= FADE,
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

    #[test]
    fn no_motion_when_windows_animations_are_off_or_unknown() {
        assert_eq!(plan(Some(false), Edge::Bottom, 1.0), None);
        assert_eq!(plan(None, Edge::Bottom, 1.0), None);
        assert!(plan(Some(true), Edge::Bottom, 1.0).is_some());
    }

    #[test]
    fn it_starts_on_the_taskbars_side_scaled_and_faded_out() {
        let start = |edge| plan(Some(true), edge, 1.5).unwrap().at(Duration::ZERO);
        assert_eq!(start(Edge::Bottom).offset, (0.0, 24.0));
        assert_eq!(start(Edge::Top).offset, (0.0, -24.0));
        assert_eq!(start(Edge::Right).offset, (24.0, 0.0));
        assert_eq!(start(Edge::Left).offset, (-24.0, 0.0));
        assert_eq!(start(Edge::Bottom).opacity, 0.0);
        assert!(!start(Edge::Bottom).done);
    }

    #[test]
    fn it_decelerates_into_place_and_fades_in_first() {
        let slide = plan(Some(true), Edge::Bottom, 1.0).unwrap();
        let early = slide.at(ms(50)).offset.1;
        let late = slide.at(ms(200)).offset.1;
        assert!(16.0 - early > late, "most of the way is covered early");
        assert!(late > 0.0 && late < 1.0, "{late}");

        let faded = slide.at(FADE);
        assert_eq!(faded.opacity, 1.0);
        assert!(faded.offset.1 > 0.0 && !faded.done, "still settling");

        let end = slide.at(SLIDE);
        assert_eq!((end.offset, end.opacity, end.done), ((0.0, 0.0), 1.0, true));
        assert_eq!(slide.at(ms(400)).offset, (0.0, 0.0));
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
