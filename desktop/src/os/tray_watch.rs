//! The Linux tray icon's decisions, apart from the session bus they are
//! made over: whether a StatusNotifier icon can show at launch, when a
//! shown icon counts as lost, the dimmed icon for streaming off and its
//! notice badge.

use std::time::{Duration, Instant};

/// How long the StatusNotifier watcher may be gone before the app stops
/// counting on its tray icon and shows the main window.
pub const LOSS_GRACE: Duration = Duration::from_secs(10);

/// What the session bus says about the StatusNotifier watcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watcher {
    /// Nobody owns the watcher's name: the desktop has no StatusNotifier
    /// support, or it is turned off.
    Missing,
    /// A watcher runs, but no host draws the items it lists.
    NoHost,
    /// A watcher with at least one host: an icon shows.
    Ready,
}

impl Watcher {
    /// Why there is no tray icon, for the log; `None` when one shows.
    pub fn missing_reason(self) -> Option<&'static str> {
        match self {
            Watcher::Missing => Some("no StatusNotifier watcher on the session bus"),
            Watcher::NoHost => Some("the StatusNotifier watcher has no host"),
            Watcher::Ready => None,
        }
    }
}

/// Follows the watcher while the tray icon shows. A watcher that comes
/// back within [`LOSS_GRACE`] (a shell restart) keeps the icon.
#[derive(Default)]
pub struct LossWatch {
    gone_since: Option<Instant>,
}

impl LossWatch {
    /// Takes one look at the watcher; true once it has been gone for
    /// [`LOSS_GRACE`].
    pub fn observe(&mut self, watcher: Watcher, now: Instant) -> bool {
        if watcher != Watcher::Missing {
            self.gone_since = None;
            return false;
        }
        let since = *self.gone_since.get_or_insert(now);
        now.duration_since(since) >= LOSS_GRACE
    }
}

/// Halves every pixel's opacity in an RGBA image: the icon while
/// streaming is off.
pub fn dim(rgba: &mut [u8]) {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel[3] /= 2;
    }
}

/// Puts the notice badge on an RGBA icon `width` by `height`: the Windows
/// tray badge's dot and gap at the bottom right, in the warning amber with a
/// dark amber rim, since the panel's theme isn't known here.
pub fn badge(rgba: &mut [u8], width: u32, height: u32) {
    const FILL: [u8; 4] = [0xFF, 0xB5, 0x47, 255];
    const RIM: [u8; 4] = [0x9A, 0x52, 0x00, 255];
    let side = width.min(height) as f32;
    let radius = side * 3.0 / 16.0;
    let (cx, cy) = (width as f32 - radius, height as f32 - radius);
    for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = (index % width as usize) as f32 + 0.5 - cx;
        let y = (index / width as usize) as f32 + 0.5 - cy;
        let distance = x.hypot(y);
        if distance <= radius - side / 32.0 {
            *pixel = FILL;
        } else if distance <= radius {
            *pixel = RIM;
        } else if distance <= radius + side / 16.0 {
            *pixel = [0; 4];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_puts_a_rimmed_dot_with_a_clear_gap_at_the_bottom_right() {
        const TEAL: [u8; 4] = [0, 128, 128, 255];
        let mut rgba = TEAL.repeat(256 * 256);
        dim(&mut rgba);
        badge(&mut rgba, 256, 256);
        let at = |x: usize, y: usize| &rgba[(y * 256 + x) * 4..][..4];
        // A 96 px dot centred 48 px in from the corner, an 8 px rim, a 16 px gap.
        assert_eq!(
            at(208, 208),
            [0xFF, 0xB5, 0x47, 255],
            "full strength while dimmed"
        );
        assert_eq!(at(253, 208), [0x9A, 0x52, 0x00, 255]);
        assert_eq!(at(208, 152), [0, 0, 0, 0]);
        assert_eq!(at(140, 208), [0, 128, 128, 127]);
        assert_eq!(at(20, 20), [0, 128, 128, 127]);
    }

    #[test]
    fn only_a_watcher_with_a_host_shows_an_icon() {
        assert!(Watcher::Missing.missing_reason().is_some());
        assert!(Watcher::NoHost.missing_reason().is_some());
        assert_eq!(Watcher::Ready.missing_reason(), None);
    }

    #[test]
    fn the_icon_counts_as_lost_after_the_watcher_is_gone_for_the_grace() {
        let start = Instant::now();
        let mut watch = LossWatch::default();
        assert!(!watch.observe(Watcher::Ready, start));
        assert!(!watch.observe(Watcher::Missing, start + Duration::from_secs(2)));
        assert!(!watch.observe(Watcher::Missing, start + Duration::from_secs(11)));
        assert!(watch.observe(Watcher::Missing, start + Duration::from_secs(12)));
    }

    /// A shell restart takes the watcher away for a moment; the icon
    /// registers again, so nothing changes.
    #[test]
    fn a_watcher_back_within_the_grace_starts_the_count_again() {
        let start = Instant::now();
        let mut watch = LossWatch::default();
        assert!(!watch.observe(Watcher::Missing, start));
        assert!(!watch.observe(Watcher::NoHost, start + Duration::from_secs(9)));
        assert!(!watch.observe(Watcher::Missing, start + Duration::from_secs(10)));
        assert!(!watch.observe(Watcher::Missing, start + Duration::from_secs(19)));
        assert!(watch.observe(Watcher::Missing, start + Duration::from_secs(20)));
    }

    #[test]
    fn dimming_halves_the_opacity_and_keeps_the_color() {
        let mut rgba = [10, 20, 30, 255, 1, 2, 3, 0];
        dim(&mut rgba);
        assert_eq!(rgba, [10, 20, 30, 127, 1, 2, 3, 0]);
    }
}
