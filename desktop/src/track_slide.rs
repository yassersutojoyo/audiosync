//! Now Playing's motion, with the Mac's numbers and curves: another song
//! slides its cover in from the side while the old one recedes (S463), and
//! Play/Pause's icon morphs (S466).

use std::time::{Duration, Instant};

use crate::now_playing_card::{ART_HOLD, CardTrack, same_song};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Next,
    Previous,
}

/// Where the Now Playing card is: the main window's travel is long, the
/// tray panel's short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Window,
    Panel,
}

/// A change this long after a press still counts as the press's: past
/// `CAP`, so the slide the cap starts is the press's.
pub const PRESS_WINDOW: Duration = ART_HOLD.saturating_add(Duration::from_secs(1));
/// The longest wait from a press for the new song and its cover, in seconds.
const CAP: f64 = ART_HOLD.as_secs_f64();
/// The old cover's recede while it waits.
const RECEDE: f64 = 1.5;
/// A wait longer than `BREATHE_AFTER` pulses the receded cover's dim and
/// opacity by `BREATHE_DEPTH`, once per `BREATHE_PERIOD`.
const BREATHE_AFTER: f64 = 0.6;
const BREATHE_PERIOD: f64 = 1.2;
const BREATHE_DEPTH: f64 = 0.05;
/// An ignored press easing back.
const SETTLE: f64 = 0.15;
const COVER_FADE: f64 = 0.25;
const WAIT_SCALE: f64 = 0.95;
const RECEDE_SCALE: f64 = 0.85;
const WAIT_DIM: f64 = 0.15;
const RECEDE_DIM: f64 = 0.3;
const DIM_TO: f64 = 0.6;
const TEXT_AWAY: f64 = 10.0;
const TEXT_FROM: f64 = 16.0;
const ARTIST_DELAY: f64 = 0.06;

/// A cubic Bezier's control points x1, y1, x2, y2.
type Curve = (f64, f64, f64, f64);
const WAIT_CURVE: Curve = (0.25, 0.6, 0.35, 1.0);
const SLIDE_CURVE: Curve = (0.2, 0.8, 0.2, 1.0);
const SETTLE_CURVE: Curve = (0.0, 0.0, 0.58, 1.0);
const BREATHE_CURVE: Curve = (0.42, 0.0, 0.58, 1.0);
/// Core Animation's easeInEaseOut, the Mac's cover fade (MainWindow.swift:1520).
const COVER_FADE_CURVE: Curve = (0.42, 0.0, 0.58, 1.0);

/// The cubic Bezier `curve`'s value at `t`, 0 to 1 of its time.
pub fn eased(curve: Curve, t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t == 0.0 || t == 1.0 {
        return t;
    }
    let (x1, y1, x2, y2) = curve;
    let axis = |a: f64, b: f64, s: f64| {
        3.0 * a * s * (1.0 - s) * (1.0 - s) + 3.0 * b * s * s * (1.0 - s) + s * s * s
    };
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..40 {
        let mid = (low + high) / 2.0;
        if axis(x1, x2, mid) < t {
            low = mid;
        } else {
            high = mid;
        }
    }
    axis(y1, y2, (low + high) / 2.0)
}

fn seconds_between(from: Instant, to: Instant) -> f64 {
    to.saturating_duration_since(from).as_secs_f64()
}

/// A wait the player ignored, easing back from `reached`.
fn settled(reached: f64, elapsed: f64) -> f64 {
    reached * (1.0 - eased(SETTLE_CURVE, elapsed / SETTLE))
}

/// The breath `elapsed` seconds into a wait: up over half a period, back
/// over the other half.
fn breath(elapsed: f64) -> f64 {
    let half = BREATHE_PERIOD / 2.0;
    let into = (elapsed - BREATHE_AFTER).max(0.0) / half;
    let rising = into.floor() % 2.0 == 0.0;
    let part = into.fract();
    BREATHE_DEPTH * eased(BREATHE_CURVE, if rising { part } else { 1.0 - part })
}

impl Place {
    fn seconds(self) -> f64 {
        match self {
            Place::Window => 0.56,
            Place::Panel => 0.42,
        }
    }
}

/// One frame of a card's change of song; all zero at rest.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Look {
    /// The old song is drawn in place while the new cover comes in.
    pub sliding: bool,
    /// With the system's animations off: the new cover fades in in place.
    pub fades: bool,
    /// 1 when the new cover comes from the right (Next), -1 from the left.
    pub side: f64,
    /// How far the new cover has come, 0 to 1.
    pub slid: f64,
    pub old_shrink: f64,
    pub old_dim: f64,
    pub old_fade: f64,
    /// What is left of a cover's fade in, one that came mid-slide.
    pub cover_fade: f64,
    pub old_text_offset: f64,
    pub old_text_fade: f64,
    pub title_offset: f64,
    pub title_fade: f64,
    pub subtitle_offset: f64,
    pub subtitle_fade: f64,
}

struct Slide {
    start: Instant,
    direction: Direction,
    fades: bool,
    /// Where the press's wait got when the song changed.
    reached: f64,
    /// Where its breath stood then.
    breath: f64,
    cover_at: Option<Instant>,
}

/// One card's side of the motion: the press it waits on, the slide under
/// way, and the song before it for a change with no press.
pub struct TrackSlide {
    place: Place,
    pressed: Option<(Direction, Instant)>,
    played_before: Option<CardTrack>,
    wait: Option<Instant>,
    /// An ignored wait easing back: since when, from where, and its breath.
    settle: Option<(Instant, f64, f64)>,
    slide: Option<Slide>,
    /// The system's animations were off at the last press or change.
    still: bool,
}

impl TrackSlide {
    pub fn new(place: Place) -> TrackSlide {
        TrackSlide {
            place,
            pressed: None,
            played_before: None,
            wait: None,
            settle: None,
            slide: None,
            still: false,
        }
    }

    /// Next or Previous, or Play/Pause (`None`), was pressed; a card off
    /// screen waits for nothing, and one that is `still` (the system's
    /// animations off) only dims.
    pub fn press(
        &mut self,
        direction: Option<Direction>,
        on_screen: bool,
        still: bool,
        now: Instant,
    ) {
        let Some(direction) = direction else {
            if self.waiting(now) {
                self.settle = Some((now, self.waited(now), self.breathed(now)));
                self.wait = None;
            }
            return;
        };
        self.pressed = Some((direction, now));
        if !on_screen || self.slide.is_some() || self.waiting(now) {
            return;
        }
        self.still = still;
        self.settle = None;
        self.wait = Some(now);
    }

    fn waiting(&self, now: Instant) -> bool {
        self.wait
            .is_some_and(|start| seconds_between(start, now) < CAP)
    }

    /// How far the old cover has receded for a press, 0 to 1; past `CAP`
    /// it eases back.
    fn waited(&self, now: Instant) -> f64 {
        if let Some(start) = self.wait {
            let elapsed = seconds_between(start, now);
            return if elapsed < CAP {
                eased(WAIT_CURVE, elapsed / RECEDE)
            } else {
                settled(1.0, elapsed - CAP)
            };
        }
        if let Some((start, reached, _)) = self.settle {
            return settled(reached, seconds_between(start, now));
        }
        self.slide.as_ref().map_or(0.0, |slide| slide.reached)
    }

    /// The waiting cover's breath; a `still` card doesn't breathe.
    fn breathed(&self, now: Instant) -> f64 {
        if let Some(start) = self.wait {
            let elapsed = seconds_between(start, now);
            return if self.still {
                0.0
            } else if elapsed < CAP {
                breath(elapsed)
            } else {
                settled(breath(CAP), elapsed - CAP)
            };
        }
        if let Some((start, _, breathed)) = self.settle {
            return settled(breathed, seconds_between(start, now));
        }
        self.slide.as_ref().map_or(0.0, |slide| slide.breath)
    }

    /// The card's song went from `old` to `new`. Returns whether a slide
    /// began; off screen the song swaps at once, and a `still` card
    /// cross-fades in place.
    pub fn change(
        &mut self,
        old: Option<&CardTrack>,
        new: Option<&CardTrack>,
        on_screen: bool,
        still: bool,
        now: Instant,
    ) -> bool {
        let (Some(old), Some(new)) = (old, new) else {
            if old.map(|t| &t.app_id) != new.map(|t| &t.app_id) {
                self.stop();
            }
            return false;
        };
        if old.app_id != new.app_id {
            self.stop();
            return false;
        }
        if same_song(old, new) {
            if old.artwork.is_none()
                && new.artwork.is_some()
                && let Some(slide) = self.slide.as_mut()
            {
                slide.cover_at = Some(now);
            }
            return false;
        }
        let pressed = self
            .pressed
            .take()
            .filter(|(_, at)| now.saturating_duration_since(*at) <= PRESS_WINDOW)
            .map(|(direction, _)| direction);
        let guess = match &self.played_before {
            Some(before) if same_song(before, new) => Direction::Previous,
            _ => Direction::Next,
        };
        self.played_before = Some(old.clone());
        if !on_screen {
            self.stop();
            return false;
        }
        let reached = self.waited(now);
        let breath = self.breathed(now);
        self.still = still;
        self.wait = None;
        self.settle = None;
        self.slide = Some(Slide {
            start: now,
            direction: pressed.unwrap_or(guess),
            fades: still,
            reached,
            breath,
            cover_at: None,
        });
        true
    }

    fn seconds(&self, slide: &Slide) -> f64 {
        if slide.fades {
            SETTLE
        } else {
            self.place.seconds()
        }
    }

    fn stop(&mut self) {
        self.wait = None;
        self.settle = None;
        self.slide = None;
    }

    /// Drops what has run its course by `now`.
    fn expire(&mut self, now: Instant) {
        if self
            .wait
            .is_some_and(|start| seconds_between(start, now) >= CAP + SETTLE)
        {
            self.wait = None;
        }
        if self
            .settle
            .is_some_and(|(start, _, _)| seconds_between(start, now) >= SETTLE)
        {
            self.settle = None;
        }
        if let Some(slide) = &self.slide {
            let slid = seconds_between(slide.start, now) >= self.seconds(slide) + ARTIST_DELAY;
            let faded = slide
                .cover_at
                .is_none_or(|at| seconds_between(at, now) >= COVER_FADE);
            if slid && faded {
                self.slide = None;
            }
        }
    }

    /// The frame at `now`, and whether anything still moves.
    pub fn look(&mut self, now: Instant) -> (Look, bool) {
        self.expire(now);
        let waited = self.waited(now);
        let breath = self.breathed(now);
        let moving = self.wait.is_some() || self.settle.is_some() || self.slide.is_some();
        let text_dim = (1.0 - DIM_TO) * waited;
        let wait_shrink = if self.still {
            0.0
        } else {
            (1.0 - WAIT_SCALE) * waited
        };
        let Some(slide) = &self.slide else {
            let look = Look {
                old_shrink: wait_shrink,
                old_dim: WAIT_DIM * waited + breath,
                old_fade: breath,
                title_fade: text_dim,
                subtitle_fade: text_dim,
                ..Look::default()
            };
            return (look, moving);
        };
        let elapsed = seconds_between(slide.start, now);
        let seconds = self.seconds(slide);
        let slid = if slide.fades {
            (elapsed / seconds).clamp(0.0, 1.0)
        } else {
            eased(SLIDE_CURVE, elapsed / seconds)
        };
        let side = match slide.direction {
            Direction::Next => 1.0,
            Direction::Previous => -1.0,
        };
        let travel = if slide.fades { 0.0 } else { side };
        let shrink = wait_shrink;
        let recede = if slide.fades { 0.0 } else { 1.0 - RECEDE_SCALE };
        let dim = WAIT_DIM * waited + breath;
        let out = eased(SLIDE_CURVE, elapsed / (seconds / 2.0));
        let into = |delay: f64| eased(SLIDE_CURVE, (elapsed - delay) / seconds);
        let look = Look {
            sliding: true,
            fades: slide.fades,
            side,
            slid,
            old_shrink: shrink + (recede - shrink) * slid,
            old_dim: dim + (RECEDE_DIM - dim) * slid,
            old_fade: breath + (1.0 - breath) * slid,
            cover_fade: slide.cover_at.map_or(0.0, |at| {
                1.0 - eased(COVER_FADE_CURVE, seconds_between(at, now) / COVER_FADE)
            }),
            old_text_offset: -travel * TEXT_AWAY * out,
            old_text_fade: 1.0 - (1.0 - text_dim) * (1.0 - out),
            title_offset: travel * TEXT_FROM * (1.0 - into(0.0)),
            title_fade: 1.0 - into(0.0),
            subtitle_offset: travel * TEXT_FROM * (1.0 - into(ARTIST_DELAY)),
            subtitle_fade: 1.0 - into(ARTIST_DELAY),
        };
        (look, moving)
    }
}

/// Play/Pause's icon `elapsed` seconds after it changed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IconLook {
    pub shows_old: bool,
    pub scale: f64,
    pub opacity: f64,
    pub moving: bool,
}

const ICON_OUT: f64 = 0.1;
const ICON_IN: f64 = 0.16;
const ICON_OUT_SCALE: f64 = 0.6;

pub fn icon_morph(elapsed: f64) -> IconLook {
    if elapsed >= ICON_OUT + ICON_IN {
        return IconLook {
            shows_old: false,
            scale: 1.0,
            opacity: 1.0,
            moving: false,
        };
    }
    if elapsed < ICON_OUT {
        let t = elapsed.max(0.0) / ICON_OUT;
        return IconLook {
            shows_old: true,
            scale: 1.0 - (1.0 - ICON_OUT_SCALE) * t,
            opacity: 1.0 - t,
            moving: true,
        };
    }
    let t = (elapsed - ICON_OUT) / ICON_IN;
    // Ease-out-back, 1 + 2.70158 (t - 1)^3 + 1.70158 (t - 1)^2 expanded, so it starts at exactly 0.
    let back = t * (2.70158 * t * t - 6.40316 * t + 4.70158);
    IconLook {
        shows_old: false,
        scale: ICON_OUT_SCALE + (1.0 - ICON_OUT_SCALE) * back,
        opacity: t,
        moving: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::now_playing_card::fake::track;

    fn at(start: Instant, seconds: f64) -> Instant {
        start + Duration::from_secs_f64(seconds)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    // covers: S463
    #[test]
    fn the_curves_run_from_0_to_1_and_ease_out() {
        for curve in [WAIT_CURVE, SLIDE_CURVE, SETTLE_CURVE] {
            assert_eq!(eased(curve, 0.0), 0.0);
            assert_eq!(eased(curve, 1.0), 1.0);
            assert_eq!(eased(curve, -1.0), 0.0);
            assert_eq!(eased(curve, 2.0), 1.0);
            assert!(eased(curve, 0.5) > 0.5, "ease out: past half at half time");
            let mut last = 0.0;
            for step in 1..=20 {
                let value = eased(curve, f64::from(step) / 20.0);
                assert!(value >= last);
                last = value;
            }
        }
        assert!(eased(SLIDE_CURVE, 0.5) > 0.9, "the slide lands early");
    }

    // covers: S463
    #[test]
    fn a_press_recedes_the_old_cover_and_eases_back_after_4_s_when_no_song_follows() {
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        slide.press(Some(Direction::Next), true, false, start);
        let (look, moving) = slide.look(at(start, 0.75));
        assert!(moving);
        assert!(!look.sliding);
        assert!(look.old_shrink > 0.0 && look.old_shrink < 0.05);
        assert!(look.title_fade > 0.0 && look.subtitle_fade == look.title_fade);
        let (look, _) = slide.look(at(start, 1.8));
        assert!(close(look.old_shrink, 0.05), "{look:?}");
        assert!(close(look.old_dim, 0.15));
        assert!(close(look.title_fade, 0.4), "the lines dim to 0.6");
        let (look, moving) = slide.look(at(start, 4.08));
        assert!(moving && look.old_shrink < 0.05, "easing back");
        let (look, moving) = slide.look(at(start, 4.16));
        assert_eq!((look, moving), (Look::default(), false));
    }

    // covers: S463
    #[test]
    fn a_wait_past_0_6_s_breathes_and_the_slide_starts_from_the_breath() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        slide.press(Some(Direction::Next), true, false, start);
        assert_eq!(slide.look(at(start, 0.6)).0.old_fade, 0.0);
        let (look, moving) = slide.look(at(start, 1.2));
        assert!(moving && close(look.old_fade, 0.05), "{look:?}");
        let waited = eased(WAIT_CURVE, 1.2 / 1.5);
        assert!(close(look.old_dim, WAIT_DIM * waited + 0.05));
        assert!(close(slide.look(at(start, 1.8)).0.old_fade, 0.0));
        assert!(close(slide.look(at(start, 3.6)).0.old_fade, 0.05));
        let (look, moving) = slide.look(at(start, 3.9));
        assert!(
            moving && !look.sliding && close(look.old_shrink, 0.05),
            "still waiting"
        );
        let before = slide.look(at(start, 3.6)).0;
        assert!(slide.change(Some(&a), Some(&b), true, false, at(start, 3.6)));
        let after = slide.look(at(start, 3.6)).0;
        assert!(close(after.old_fade, before.old_fade) && close(after.old_dim, before.old_dim));

        let mut still = TrackSlide::new(Place::Window);
        still.press(Some(Direction::Next), true, true, start);
        assert_eq!(still.look(at(start, 1.2)).0.old_fade, 0.0, "no breath");
    }

    // covers: S463
    #[test]
    fn next_slides_from_the_right_and_previous_from_the_left_for_0_56_s_in_the_window() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        slide.press(Some(Direction::Next), true, false, start);
        assert!(slide.change(Some(&a), Some(&b), true, false, at(start, 0.4)));
        let (look, moving) = slide.look(at(start, 0.4));
        assert!(moving && look.sliding);
        assert_eq!(look.side, 1.0);
        assert_eq!(look.slid, 0.0);
        assert!(look.old_shrink > 0.0, "it recedes from where the wait got");
        assert!(close(look.title_offset, 16.0) && close(look.title_fade, 1.0));
        let (look, _) = slide.look(at(start, 0.4 + 0.28));
        assert!(close(look.old_fade, look.slid));
        assert!(
            close(look.old_text_fade, 1.0),
            "the old lines are gone at half time"
        );
        assert!(close(look.old_text_offset, -10.0));
        let (look, _) = slide.look(at(start, 0.4 + 0.56));
        assert!(close(look.slid, 1.0) && close(look.old_shrink, 0.15) && close(look.old_dim, 0.3));
        assert!(look.subtitle_fade > 0.0, "the artist follows 0.06 s later");
        let (_, moving) = slide.look(at(start, 0.4 + 0.63));
        assert!(!moving);

        slide.press(Some(Direction::Previous), true, false, at(start, 2.0));
        assert!(slide.change(Some(&b), Some(&a), true, false, at(start, 2.1)));
        assert_eq!(slide.look(at(start, 2.1)).0.side, -1.0);
        assert!(close(slide.look(at(start, 2.1)).0.title_offset, -16.0));
    }

    // covers: S463
    #[test]
    fn the_panels_slide_takes_0_42_s() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Panel);
        assert!(slide.change(Some(&a), Some(&b), true, false, start));
        assert!(close(slide.look(at(start, 0.42)).0.slid, 1.0));
        assert!(slide.look(at(start, 0.42)).1);
        assert!(!slide.look(at(start, 0.49)).1);
    }

    // covers: S463
    #[test]
    fn play_pause_eases_a_waiting_cover_back_and_the_press_before_it_keeps_its_side() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        slide.press(Some(Direction::Previous), true, false, start);
        slide.press(None, true, false, at(start, 0.5));
        let (look, moving) = slide.look(at(start, 0.5 + 0.16));
        assert_eq!((look, moving), (Look::default(), false));
        assert!(slide.change(Some(&a), Some(&b), true, false, at(start, 2.5)));
        assert_eq!(slide.look(at(start, 2.5)).0.side, -1.0);
    }

    // covers: S463
    #[test]
    fn a_change_with_no_press_slides_as_next_or_as_previous_back_to_the_song_before() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let c = track("Spotify.exe", "C", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        assert!(slide.change(Some(&a), Some(&b), true, false, start));
        assert_eq!(slide.look(start).0.side, 1.0);
        assert!(slide.change(Some(&b), Some(&a), true, false, at(start, 1.0)));
        assert_eq!(slide.look(at(start, 1.0)).0.side, -1.0);
        assert!(slide.change(Some(&a), Some(&c), true, false, at(start, 2.0)));
        assert_eq!(slide.look(at(start, 2.0)).0.side, 1.0);

        slide.press(Some(Direction::Previous), true, false, at(start, 3.0));
        assert!(slide.change(Some(&c), Some(&b), true, false, at(start, 8.5)));
        assert_eq!(
            slide.look(at(start, 8.5)).0.side,
            1.0,
            "a press over 5 s old no longer counts"
        );
    }

    /// Windows debug log 2026-11-20 03:54: Previous on the card, then Next
    /// in Spotify back to the song the Previous left.
    // covers: S463
    #[test]
    fn next_back_to_the_song_a_previous_left_slides_from_the_right() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        assert!(slide.change(Some(&a), Some(&b), true, false, start));
        slide.press(Some(Direction::Previous), true, false, at(start, 1.0));
        assert!(slide.change(Some(&b), Some(&a), true, false, at(start, 1.5)));
        assert_eq!(slide.look(at(start, 1.5)).0.side, -1.0);
        assert!(slide.change(Some(&a), Some(&b), true, false, at(start, 12.0)));
        assert_eq!(slide.look(at(start, 12.0)).0.side, 1.0, "Next");
        assert!(slide.change(Some(&b), Some(&a), true, false, at(start, 14.0)));
        assert_eq!(slide.look(at(start, 14.0)).0.side, -1.0, "Previous");
        assert!(slide.change(Some(&a), Some(&b), true, false, at(start, 16.0)));
        assert_eq!(slide.look(at(start, 16.0)).0.side, 1.0, "Next again");
    }

    // covers: S463
    #[test]
    fn a_cover_that_comes_mid_slide_fades_in_over_0_25_s() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let mut b_cover = b.clone();
        b_cover.artwork = Some(crate::now_playing_card::CardArtwork::new(
            1,
            vec![0u8, 0, 0, 255],
        ));
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        assert!(slide.change(Some(&a), Some(&b), true, false, start));
        assert_eq!(slide.look(start).0.cover_fade, 0.0);
        assert!(!slide.change(Some(&b), Some(&b_cover), true, false, at(start, 0.5)));
        assert!(close(slide.look(at(start, 0.5)).0.cover_fade, 1.0));
        assert!(close(slide.look(at(start, 0.625)).0.cover_fade, 0.5));
        let (look, moving) = slide.look(at(start, 0.7));
        assert!(moving, "the slide waits for the fade");
        assert!(look.sliding);
        assert!(!slide.look(at(start, 0.76)).1);
    }

    // The Mac fades the cover in on Core Animation's easeInEaseOut
    // (MainWindow.swift:1519-1520), which a check at half the fade can't tell
    // from a linear one.
    // covers: S463
    #[test]
    fn a_cover_that_comes_mid_slide_fades_in_on_the_macs_ease_in_ease_out() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let mut b_cover = b.clone();
        b_cover.artwork = Some(crate::now_playing_card::CardArtwork::new(
            1,
            vec![0u8, 0, 0, 255],
        ));
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        assert!(slide.change(Some(&a), Some(&b), true, false, start));
        assert!(!slide.change(Some(&b), Some(&b_cover), true, false, at(start, 0.5)));
        for quarter in [1.0, 3.0] {
            let fade = slide
                .look(at(start, 0.5 + 0.25 * quarter / 4.0))
                .0
                .cover_fade;
            let mac = 1.0 - eased((0.42, 0.0, 0.58, 1.0), quarter / 4.0);
            assert!(
                close(fade, mac),
                "{quarter}/4 into the fade: {fade}, the Mac {mac}"
            );
        }
    }

    // covers: S463
    #[test]
    fn with_animations_off_a_press_only_dims_and_the_songs_cross_fade_in_0_15_s() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        slide.press(Some(Direction::Next), true, true, start);
        let (look, moving) = slide.look(at(start, 1.0));
        assert!(moving && look.old_dim > 0.0 && look.title_fade > 0.0);
        assert_eq!(look.old_shrink, 0.0);
        assert!(slide.change(Some(&a), Some(&b), true, true, at(start, 1.0)));
        let (look, _) = slide.look(at(start, 1.075));
        assert!(look.sliding && look.fades);
        assert!(close(look.slid, 0.5) && close(look.old_fade, 0.5), "linear");
        assert_eq!(
            (look.old_shrink, look.title_offset, look.old_text_offset),
            (0.0, 0.0, 0.0)
        );
        assert!(slide.look(at(start, 1.2)).1);
        assert!(!slide.look(at(start, 1.22)).1);
    }

    // covers: S463
    #[test]
    fn a_still_card_swaps_at_once_and_a_press_does_not_wait() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        slide.press(Some(Direction::Next), false, false, start);
        assert_eq!(slide.look(at(start, 0.5)), (Look::default(), false));
        assert!(!slide.change(Some(&a), Some(&b), false, false, at(start, 0.6)));
        assert_eq!(slide.look(at(start, 0.6)), (Look::default(), false));
    }

    // covers: S463
    #[test]
    fn the_same_song_keeps_the_slide_and_another_player_ends_it() {
        let a = track("Spotify.exe", "A", "Band", true);
        let b = track("Spotify.exe", "B", "Band", true);
        let paused = track("Spotify.exe", "B", "Band", false);
        let other = track("vlc.exe", "C", "Band", true);
        let start = Instant::now();
        let mut slide = TrackSlide::new(Place::Window);
        assert!(slide.change(Some(&a), Some(&b), true, false, start));
        assert!(!slide.change(Some(&b), Some(&paused), true, false, at(start, 0.1)));
        assert!(slide.look(at(start, 0.1)).0.sliding);
        assert!(!slide.change(Some(&paused), Some(&other), true, false, at(start, 0.2)));
        assert_eq!(slide.look(at(start, 0.2)), (Look::default(), false));
        assert!(!slide.change(Some(&other), None, true, false, at(start, 0.3)));
    }

    // covers: S466
    #[test]
    fn the_play_pause_icon_shrinks_out_in_0_1_s_and_grows_back_with_an_overshoot() {
        let look = icon_morph(0.0);
        assert!(look.shows_old && look.moving);
        assert_eq!((look.scale, look.opacity), (1.0, 1.0));
        let look = icon_morph(0.09995);
        assert!(look.shows_old && close(look.scale, 0.6) && close(look.opacity, 0.0));
        let look = icon_morph(0.1);
        assert!(!look.shows_old && close(look.scale, 0.6) && close(look.opacity, 0.0));
        let peak = (0..=160)
            .map(|ms| icon_morph(0.1 + f64::from(ms) / 1000.0 * 0.999).scale)
            .fold(0.0, f64::max);
        assert!(peak > 1.0 && peak < 1.1, "{peak}");
        let look = icon_morph(0.26);
        assert_eq!(
            look,
            IconLook {
                shows_old: false,
                scale: 1.0,
                opacity: 1.0,
                moving: false
            }
        );
    }
}
