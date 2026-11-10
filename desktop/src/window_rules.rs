//! Layout and motion rules of the main window and the tray panel, kept apart
//! from Slint so they can be tested. They follow the Mac app's
//! `WindowRules.swift`, so both platforms size, fold and scroll alike.

/// The main window's two columns: the speaker list widens first, to its full
/// width, then the left column.
pub mod main_columns {
    pub const NARROW: f32 = 210.0;
    pub const WIDE: f32 = 280.0;
    pub const SPEAKER_LIST_FULL: f32 = 524.0;
    pub const WINDOW_MARGIN: f32 = 20.0;
    pub const COLUMN_GAP: f32 = 16.0;
    /// An estimate, not measured: wider than the widest transport symbol.
    pub const TRANSPORT_BUTTON: f32 = 26.0;
    pub const HERO_PADDING: f32 = 14.0;

    pub fn room(width: f32) -> f32 {
        width - 2.0 * WINDOW_MARGIN - COLUMN_GAP
    }

    pub fn left(width: f32) -> f32 {
        (room(width) - SPEAKER_LIST_FULL).clamp(NARROW, WIDE)
    }

    pub fn speaker_list(width: f32) -> f32 {
        (room(width) - left(width)).min(SPEAKER_LIST_FULL)
    }

    /// The gap between the Now Playing buttons, `modes` while Shuffle or Repeat shows.
    pub fn transport_spacing(modes: bool) -> f32 {
        if modes { 12.0 } else { 28.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn max_x(&self) -> f32 {
        self.x + self.width
    }
}

/// Where the zoom button takes the main window: anything narrower than the
/// widest goes to 860, the widest back to the slim 696. Top edge and height
/// stay; a wider window moves left as far as it must to stay on `visible`.
pub mod main_zoom {
    use super::Rect;

    pub const SLIM: f32 = 696.0;
    pub const WIDE: f32 = 860.0;

    pub fn target(frame: Rect, visible: Rect) -> Rect {
        let mut out = frame;
        out.width = if frame.width < WIDE - 0.5 { WIDE } else { SLIM };
        if out.max_x() > visible.max_x() {
            out.x = visible.max_x() - out.width;
        }
        out.x = out.x.max(visible.x);
        out
    }
}

/// How much the Now Playing art moves, `env`, 0 when still. It follows the
/// meter level's rises above its own recent average, not the loudness, so a
/// long flat note calms it and silence stills it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tide {
    env: f64,
    drive: f64,
    mean: f64,
}

impl Tide {
    pub const ONSET: f64 = 0.35;
    pub const RISE: f64 = 0.45;
    pub const FALL: f64 = 1.6;
    pub const TAIL: f64 = 3.0;
    pub const AVERAGE: f64 = 1.4;
    pub const STOP_AT: f64 = 0.02;
    /// Drive per second of a level 1.0 above its average.
    pub const GAIN: f64 = 40.0;

    pub fn env(&self) -> f64 {
        self.env
    }

    /// `level` is the meter's 0-to-1 reading, `None` when nothing is captured.
    pub fn step(&mut self, level: Option<f64>, dt: f64) {
        let now = level.unwrap_or(0.0);
        self.drive = (self.drive * (-dt / Self::ONSET).exp()
            + (now - self.mean).max(0.0) * Self::GAIN * dt)
            .min(1.2);
        self.mean += (now - self.mean) * (1.0 - (-dt / Self::AVERAGE).exp());
        let playing = now > Self::STOP_AT;
        let tau = if self.drive > self.env {
            Self::RISE
        } else if playing {
            Self::FALL
        } else {
            Self::TAIL
        };
        self.env += (self.drive - self.env) * (1.0 - (-dt / tau).exp());
        let floor = if playing { 0.004 } else { Self::STOP_AT };
        if self.env < floor {
            self.env = 0.0;
        }
    }
}

/// A line too long for its room rests at its start, moves to its end at
/// `SPEED`, rests, moves back, and repeats. The Slint `Marquee` global runs
/// the same table; a test pins its numbers to these.
pub mod marquee {
    /// Pixels a second.
    pub const SPEED: f32 = 30.0;
    pub const PAUSE: f64 = 2.0;
    pub const EDGE_FADE: f32 = 8.0;
    /// Longer at the leading edge: a sliver of the cut letter there read as a thin line.
    pub const LEADING_FADE: f32 = 16.0;

    /// How far the line runs past its room; 0 when it fits, the room isn't
    /// laid out yet, or reduce motion cuts it with an ellipsis.
    pub fn overflow(text: f32, line: f32, reduce_motion: bool) -> f32 {
        if !reduce_motion && line > 0.0 && text > line {
            text - line
        } else {
            0.0
        }
    }

    pub fn offset(elapsed: f64, overflow: f32) -> f32 {
        let travel = f64::from(overflow / SPEED);
        let at = elapsed % (2.0 * (PAUSE + travel));
        if at < PAUSE {
            0.0
        } else if at < PAUSE + travel {
            (at - PAUSE) as f32 * SPEED
        } else if at < 2.0 * PAUSE + travel {
            overflow
        } else {
            overflow - (at - 2.0 * PAUSE - travel) as f32 * SPEED
        }
    }

    /// `(leading, trailing)` fade widths at `offset`: only an edge that cuts a letter fades.
    pub fn edges(offset: f32, overflow: f32) -> (f32, f32) {
        if overflow <= 0.0 {
            return (0.0, 0.0);
        }
        (
            offset.clamp(0.0, LEADING_FADE),
            (overflow - offset).clamp(0.0, EDGE_FADE),
        )
    }

    /// The mask over a line `width` wide: `(location 0 to 1, opacity)` stops.
    pub fn mask_stops(fades: (f32, f32), width: f32) -> [(f32, f32); 5] {
        // Each fade within its half, so the stops stay in order on a very short line.
        let leading = (fades.0 / width).min(0.5);
        let trailing = (fades.1 / width).min(0.5);
        // Clear over the leading fade's first quarter: a cut letter's sliver still showed on the ramp.
        [
            (0.0, 0.0),
            (leading / 4.0, 0.0),
            (leading, 1.0),
            (1.0 - trailing, 1.0),
            (1.0, 0.0),
        ]
    }
}

/// How a zone folds: rows, chevron and the panel's height on one progress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelFold {
    pub duration: f64,
}

impl PanelFold {
    pub const DURATION: f64 = 0.25;

    pub fn new(reduce_motion: bool) -> Self {
        Self {
            duration: if reduce_motion { 0.0 } else { Self::DURATION },
        }
    }

    pub fn moves(&self) -> bool {
        self.duration > 0.0
    }

    /// Whether the window takes its content's height on each frame at `now`
    /// for the fold that started at `start`; a little past the motion, so its
    /// last frame's height still arrives.
    pub fn follows(&self, start: f64, now: f64) -> bool {
        self.moves() && now >= start && now - start <= self.duration + 0.05
    }
}

/// A fold pane at `progress` (0 folded, 1 open): how far its edge has moved
/// and its content's opacity. The content fades only while the edge holds
/// still, so no edge crosses visible text; closing runs the same curve back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoldPhase {
    pub edge: f64,
    pub fade: f64,
}

impl FoldPhase {
    /// The chevron's turn in degrees, on the edge's curve.
    pub fn turn(&self) -> f64 {
        90.0 * self.edge
    }
}

pub fn fold_phase(progress: f64, reduce_motion: bool) -> FoldPhase {
    let ease = |t: f64| t * t * (3.0 - 2.0 * t);
    if reduce_motion {
        let edge = if progress > 0.0 { 1.0 } else { 0.0 };
        return FoldPhase {
            edge,
            fade: progress,
        };
    }
    FoldPhase {
        edge: ease((progress * 2.0).min(1.0)),
        fade: ease((progress * 2.0 - 1.0).max(0.0)),
    }
}

/// Which of the Now Playing art's two motions shows: the sound's (`live` 1)
/// while captured sound drives it, else a slow breath (`live` 0). It turns
/// over `BLEND` seconds, and back to the breath only after `SETTLE` seconds of
/// quiet, so a quiet passage or the gap between songs doesn't flip it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientMode {
    live: f64,
    quiet: f64,
}

impl Default for AmbientMode {
    fn default() -> Self {
        Self {
            live: 0.0,
            quiet: Self::SETTLE,
        }
    }
}

impl AmbientMode {
    pub const BLEND: f64 = 0.4;
    pub const SETTLE: f64 = 1.0;

    pub fn live(&self) -> f64 {
        self.live
    }

    /// `meter` is the 0-to-1 reading, `None` when streaming is off or nothing is captured.
    pub fn sounding(meter: Option<f64>) -> bool {
        meter.unwrap_or(0.0) > Tide::STOP_AT
    }

    pub fn step(&mut self, sounding: bool, dt: f64) {
        self.quiet = if sounding { 0.0 } else { self.quiet + dt };
        // 1e-9: ten 0.1 s steps sum to just under 1 s.
        let target = if self.quiet < Self::SETTLE - 1e-9 {
            1.0
        } else {
            0.0
        };
        let step = dt / Self::BLEND;
        self.live = if (target - self.live).abs() <= step + 1e-9 {
            target
        } else if target > self.live {
            self.live + step
        } else {
            self.live - step
        };
    }
}

pub type Rgb = [f64; 3];

/// The art's calm motion, its colour against the hero card, and how often it draws.
pub mod ambient_breath {
    use super::Rgb;

    pub const PERIOD: f64 = 5.0;
    /// In `Tide` env units, a quarter of the sound's widest.
    pub const SPREAD: f64 = 0.25;
    pub const OPACITY: f64 = 0.8;
    pub const MIN_CONTRAST: f64 = 3.0;
    /// The hero card (primary at 5% over the window) in each theme.
    pub const LIGHT_CARD: Rgb = [0.9, 0.9, 0.9];
    pub const DARK_CARD: Rgb = [0.2, 0.2, 0.2];
    pub const LIGHT_ACCENT: Rgb = [20.0 / 255.0, 113.0 / 255.0, 103.0 / 255.0];
    pub const DARK_ACCENT: Rgb = [60.0 / 255.0, 203.0 / 255.0, 185.0 / 255.0];
    pub const LIGHT_GREY: Rgb = [0.2, 0.2, 0.2];
    pub const DARK_GREY: Rgb = [0.85, 0.85, 0.85];

    pub fn spread(time: f64) -> f64 {
        SPREAD * (0.6 - 0.4 * (std::f64::consts::TAU * time / PERIOD).cos())
    }

    /// The breath gives way to the sound's motion, never to a dead line.
    pub fn motion(env: f64, live: f64, breath: f64) -> f64 {
        breath + live * (env - breath).max(0.0)
    }

    /// Points each way of the resting swing, in a slot of any height.
    pub const SWAY: f64 = 3.0;
    /// `(amplitude, length, offset)` of each strand's ripple.
    pub const WAVES: [(f64, f64, f64); 3] = [(7.0, 92.0, 0.0), (5.0, 61.0, 1.7), (3.2, 137.0, 3.1)];

    /// Strand `index`'s offset from the middle at `x` (0 to 1 across), before the
    /// taper, in a 22 pt slot drawn `slot` points high; `idle` is 1 at rest, 0 with the sound.
    pub fn lift(index: usize, x: f64, time: f64, spread: f64, idle: f64, slot: f64) -> f64 {
        use std::f64::consts::{PI, TAU};
        let (amplitude, length, offset) = WAVES[index];
        let strand = index as f64;
        let angle = TAU * 300.0 * x / length + time * 2.0 * (1.0 + strand * 0.35) + offset;
        // A third of a cycle apart, so the resting strands read as three lines.
        let swing = (TAU * time / PERIOD + strand * TAU / 3.0 + PI * x).sin();
        amplitude * spread * angle.sin()
            + (strand - 1.0) * 2.2 * spread
            + idle * SWAY * 22.0 / slot * swing
    }

    /// `(breath, sound)` opacities: the breath's line stays while the sound's strands are faint.
    pub fn layers(live: f64, env: f64) -> (f64, f64) {
        (
            OPACITY * (1.0 - live * (env * 2.5).min(1.0)),
            live * (0.25 + env * 2.5).min(1.0),
        )
    }

    /// Seconds between frames, `None` for none.
    pub fn frame_interval(on_screen: bool, reduce_motion: bool, live: f64) -> Option<f64> {
        if !on_screen || reduce_motion {
            return None;
        }
        Some(if live > 0.0 { 1.0 / 60.0 } else { 1.0 / 15.0 })
    }

    pub fn contrast(a: Rgb, b: Rgb) -> f64 {
        let luminance = |c: Rgb| {
            let linear = |v: f64| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(c[0]) + 0.7152 * linear(c[1]) + 0.0722 * linear(c[2])
        };
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    pub fn over(color: Rgb, card: Rgb, opacity: f64) -> Rgb {
        [0, 1, 2].map(|i| color[i] * opacity + card[i] * (1.0 - opacity))
    }

    /// The cover colour that stands out most on the card, else the accent,
    /// else grey, so the line drawn at `opacity` keeps `MIN_CONTRAST`.
    pub fn line(cover: &[Rgb], dark: bool, opacity: f64) -> Rgb {
        let card = if dark { DARK_CARD } else { LIGHT_CARD };
        let seen = |color: Rgb| contrast(over(color, card, opacity), card);
        let best = cover
            .iter()
            .copied()
            .max_by(|a, b| seen(*a).total_cmp(&seen(*b)));
        if let Some(best) = best.filter(|c| seen(*c) >= MIN_CONTRAST) {
            return best;
        }
        let accent = if dark { DARK_ACCENT } else { LIGHT_ACCENT };
        if seen(accent) >= MIN_CONTRAST {
            return accent;
        }
        if dark { DARK_GREY } else { LIGHT_GREY }
    }
}

/// The streaming switch: a 38 x 22 track whose knob is 18 on, and 21 off so
/// it meets the off track's ring with no track between them.
pub mod power_switch {
    pub const TRACK_WIDTH: f32 = 38.0;
    pub const TRACK_HEIGHT: f32 = 22.0;

    /// The knob's `(size, x)` inside the track.
    pub fn knob(on: bool) -> (f32, f32) {
        if on {
            (18.0, TRACK_WIDTH - 2.0 - 18.0)
        } else {
            (21.0, 0.5)
        }
    }
}

/// A speaker's badge: off, waiting for streaming to start, or playing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    Off,
    Ring,
    Filled,
}

impl Badge {
    pub fn of(enabled: bool, streaming: bool) -> Self {
        match (enabled, streaming) {
            (false, _) => Badge::Off,
            (true, false) => Badge::Ring,
            (true, true) => Badge::Filled,
        }
    }

    /// The waiting ring's width on a badge `size` wide.
    pub fn ring_width(size: f32) -> f32 {
        (size / 18.0).max(1.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(width: f32) -> (f32, f32) {
        (main_columns::left(width), main_columns::speaker_list(width))
    }

    #[test]
    fn columns_widen_the_speaker_list_first_then_the_left_column() {
        assert_eq!(columns(696.0), (210.0, 430.0));
        assert_eq!(columns(790.0), (210.0, 524.0));
        assert_eq!(columns(825.0), (245.0, 524.0));
        assert_eq!(columns(860.0), (280.0, 524.0));
        assert_eq!(columns(900.0), (280.0, 524.0));
    }

    #[test]
    fn five_transport_buttons_fit_the_narrow_hero() {
        use main_columns::*;
        let row = 5.0 * TRANSPORT_BUTTON + 4.0 * transport_spacing(true);
        assert!(row <= NARROW - 2.0 * HERO_PADDING, "{row}");
        assert_eq!(transport_spacing(false), 28.0);
    }

    #[test]
    fn zoom_toggles_between_slim_and_wide_and_stays_on_screen() {
        let visible = Rect {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 875.0,
        };
        let at = |x: f32, width: f32| Rect {
            x,
            y: 100.0,
            width,
            height: 600.0,
        };
        let wide = main_zoom::target(at(300.0, 696.0), visible);
        assert_eq!(wide, at(300.0, 860.0));
        assert_eq!(main_zoom::target(wide, visible), at(300.0, 696.0));
        assert_eq!(main_zoom::target(at(300.0, 780.0), visible).width, 860.0);
        let edge = main_zoom::target(at(1400.0 - 696.0, 696.0), visible);
        assert_eq!((edge.max_x(), edge.width), (visible.max_x(), 860.0));
        assert_eq!((edge.y, edge.height), (100.0, 600.0));
    }

    fn tide_run(seconds: f64, tide: &mut Tide, level: impl Fn(f64) -> Option<f64>) -> Vec<f64> {
        (0..(seconds * 10.0) as usize)
            .map(|tick| {
                tide.step(level(tick as f64 / 10.0), 0.1);
                tide.env()
            })
            .collect()
    }

    #[test]
    fn tide_is_still_in_silence_and_calms_on_a_flat_note() {
        let mut silent = Tide::default();
        assert!(
            tide_run(5.0, &mut silent, |_| Some(0.0))
                .iter()
                .all(|e| *e == 0.0)
        );
        assert!(
            tide_run(5.0, &mut silent, |_| None)
                .iter()
                .all(|e| *e == 0.0)
        );
        let mut flat = Tide::default();
        let run = tide_run(20.0, &mut flat, |_| Some(0.6));
        assert!(run[..10].iter().any(|e| *e > 0.3), "{run:?}");
        assert_eq!(run.last(), Some(&0.0));
    }

    #[test]
    fn tide_keeps_moving_with_changing_sound_and_settles_after_it_stops() {
        let mut song = Tide::default();
        let playing = tide_run(10.0, &mut song, |t| {
            Some(if (t * 2.0) as i64 % 2 == 0 { 0.35 } else { 0.6 })
        });
        let stop = tide_run(15.0, &mut song, |_| None);
        assert!(playing[playing.len() - 20..].iter().any(|e| *e > 0.3));
        assert!(
            stop[30] > 0.0 && stop[140] == 0.0,
            "{} {}",
            stop[30],
            stop[140]
        );
        assert!(stop.windows(2).all(|w| w[1] <= w[0]));
    }

    #[test]
    fn marquee_scrolls_only_what_runs_past_its_line() {
        assert_eq!(marquee::overflow(300.0, 200.0, false), 100.0);
        assert_eq!(marquee::overflow(150.0, 200.0, false), 0.0);
        assert_eq!(marquee::overflow(300.0, 0.0, false), 0.0);
        assert_eq!(marquee::overflow(300.0, 200.0, true), 0.0);
    }

    #[test]
    fn marquee_rests_scrolls_rests_and_scrolls_back() {
        let run: Vec<f32> = [0.0, 1.9, 3.0, 4.0, 5.9, 7.0, 8.0, 9.9, 11.0]
            .iter()
            .map(|t| marquee::offset(*t, 60.0))
            .collect();
        let expected = [0.0, 0.0, 30.0, 60.0, 60.0, 30.0, 0.0, 0.0, 30.0];
        assert!(
            run.iter().zip(expected).all(|(a, b)| (a - b).abs() < 1e-3),
            "{run:?}"
        );
        assert_eq!(marquee::offset(5.0, 0.0), 0.0);
    }

    #[test]
    fn marquee_fades_only_an_edge_that_cuts_a_letter() {
        assert_eq!(marquee::edges(0.0, 60.0), (0.0, 8.0));
        assert_eq!(marquee::edges(60.0, 60.0), (16.0, 0.0));
        assert_eq!(marquee::edges(4.0, 60.0), (4.0, 8.0));
        assert_eq!(marquee::edges(30.0, 0.0), (0.0, 0.0));
        let stops = marquee::mask_stops((16.0, 8.0), 160.0);
        let expected = [
            (0.0, 0.0),
            (0.025, 0.0),
            (0.1, 1.0),
            (0.95, 1.0),
            (1.0, 0.0),
        ];
        assert!(
            stops
                .iter()
                .zip(expected)
                .all(|(a, b)| (a.0 - b.0).abs() < 1e-6 && a.1 == b.1),
            "{stops:?}"
        );
        let short = marquee::mask_stops((16.0, 8.0), 10.0);
        assert!(short.windows(2).all(|w| w[0].0 <= w[1].0), "{short:?}");
    }

    #[test]
    fn slint_marquee_runs_the_same_table() {
        let widgets = include_str!("../ui/widgets.slint");
        let global = widgets
            .split("global Marquee {")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .unwrap_or("");
        let speed = format!("/ {}px * 1000", marquee::SPEED);
        let pause = format!("at < {}", marquee::PAUSE * 1000.0);
        assert!(
            global.contains(&speed) && global.contains(&pause),
            "{global}"
        );
    }

    #[test]
    fn panel_fold_takes_a_quarter_second_or_none_under_reduce_motion() {
        let fold = PanelFold::new(false);
        let still = PanelFold::new(true);
        assert!(fold.moves() && fold.duration == 0.25);
        assert!(!still.moves() && still.duration == 0.0);
        assert!(fold.follows(1.0, 1.0) && fold.follows(1.0, 1.29));
        assert!(!fold.follows(1.0, 1.31) && !fold.follows(1.0, 0.9));
        assert!(!still.follows(1.0, 1.0));
    }

    #[test]
    fn fold_moves_the_edge_first_and_fades_the_content_after() {
        let half = fold_phase(0.5, false);
        assert_eq!((half.edge, half.fade), (1.0, 0.0));
        let quarter = fold_phase(0.25, false);
        assert_eq!((quarter.edge, quarter.fade), (0.5, 0.0));
        let late = fold_phase(0.75, false);
        assert_eq!((late.edge, late.fade), (1.0, 0.5));
        assert_eq!(fold_phase(1.0, false).turn(), 90.0);
        assert_eq!(
            fold_phase(0.0, false),
            FoldPhase {
                edge: 0.0,
                fade: 0.0
            }
        );
        let steps: Vec<FoldPhase> = (0..=20)
            .map(|i| fold_phase(i as f64 / 20.0, false))
            .collect();
        assert!(
            steps
                .windows(2)
                .all(|w| w[1].edge >= w[0].edge && w[1].fade >= w[0].fade)
        );
    }

    #[test]
    fn fold_under_reduce_motion_jumps_the_edge_and_only_fades() {
        let start = fold_phase(0.1, true);
        assert_eq!((start.edge, start.fade), (1.0, 0.1));
        assert_eq!(fold_phase(0.0, true).edge, 0.0);
    }

    fn mode_run(seconds: f64, mode: &mut AmbientMode, sounding: impl Fn(f64) -> bool) -> Vec<f64> {
        (0..(seconds * 10.0).round() as usize)
            .map(|tick| {
                mode.step(sounding(tick as f64 / 10.0), 0.1);
                mode.live()
            })
            .collect()
    }

    #[test]
    fn ambient_counts_only_captured_sound() {
        assert!(!AmbientMode::sounding(None));
        assert!(!AmbientMode::sounding(Some(0.0)));
        assert!(AmbientMode::sounding(Some(0.3)));
    }

    #[test]
    fn ambient_blends_in_fast_and_back_to_the_breath_after_a_second_of_quiet() {
        let mut mode = AmbientMode::default();
        assert_eq!(mode.live(), 0.0);
        let rising = mode_run(0.5, &mut mode, |_| true);
        let held = mode_run(0.9, &mut mode, |_| false);
        let falling = mode_run(0.8, &mut mode, |_| false);
        assert!(
            rising[0] > 0.0 && rising[0] < 1.0 && rising.last() == Some(&1.0),
            "{rising:?}"
        );
        assert!(held.iter().all(|l| *l == 1.0), "{held:?}");
        assert!(falling.iter().any(|l| *l > 0.0 && *l < 1.0), "{falling:?}");
        assert_eq!(falling.last(), Some(&0.0));
        let mut gaps = AmbientMode::default();
        let run = mode_run(6.0, &mut gaps, |t| t % 1.4 < 0.8);
        assert!(run[5..].iter().all(|l| *l == 1.0), "{run:?}");
    }

    #[test]
    fn ambient_breath_is_slow_small_and_gives_way_to_the_sound() {
        use ambient_breath::*;
        let breaths: Vec<f64> = (0..100)
            .map(|i| spread(PERIOD * i as f64 / 100.0))
            .collect();
        let max = breaths.iter().cloned().fold(f64::MIN, f64::max);
        let min = breaths.iter().cloned().fold(f64::MAX, f64::min);
        assert!((4.0..=6.0).contains(&PERIOD));
        assert!(
            (0.2..=0.26).contains(&max) && max - min > 0.1,
            "{min} {max}"
        );
        assert!((spread(0.0) - spread(PERIOD)).abs() < 1e-9);
        assert_eq!(motion(0.9, 0.0, 0.2), 0.2);
        assert!((motion(0.9, 1.0, 0.2) - 0.9).abs() < 1e-9);
        assert_eq!(motion(0.0, 1.0, 0.2), 0.2);
        assert_eq!(layers(0.0, 0.8), (OPACITY, 0.0));
        assert_eq!(layers(1.0, 0.8), (0.0, 1.0));
        const { assert!(OPACITY < 1.0) };
    }

    #[test]
    fn ambient_rest_sways_three_points_each_way_with_three_strands_out_of_step() {
        use ambient_breath::*;
        use std::f64::consts::PI;
        let times: Vec<f64> = (0..100).map(|i| PERIOD * i as f64 / 100.0).collect();
        // The main window's 22 pt slot at density 1 and the panel's 16 pt one at 0.7.
        for (slot, density) in [(22.0, 1.0), (16.0, 0.7)] {
            let drawn = |strand: usize, x: f64, t: f64| {
                lift(strand, x, t, spread(t) * density, 1.0, slot) * slot / 22.0 * (PI * x).sin()
            };
            for strand in 0..3 {
                let peak = times
                    .iter()
                    .flat_map(|t| (0..=20).map(move |s| drawn(strand, s as f64 / 20.0, *t).abs()))
                    .fold(0.0, f64::max);
                assert!((3.0..=5.5).contains(&peak), "{slot} {strand} {peak}");
            }
            for (a, b) in [(0, 1), (0, 2), (1, 2)] {
                let apart = times
                    .iter()
                    .map(|t| (drawn(a, 0.5, *t) - drawn(b, 0.5, *t)).abs())
                    .sum::<f64>()
                    / times.len() as f64;
                assert!(apart >= 3.0, "{slot} {a} {b} {apart}");
            }
        }
        assert_eq!(lift(1, 0.5, 1.3, 0.0, 0.0, 16.0), 0.0);
    }

    #[test]
    fn ambient_draws_slowly_at_rest_and_never_off_screen() {
        use ambient_breath::frame_interval;
        assert_eq!(frame_interval(false, false, 1.0), None);
        assert_eq!(frame_interval(true, true, 1.0), None);
        assert!(frame_interval(true, false, 0.0).unwrap_or(0.0) >= 1.0 / 20.0);
        assert!(frame_interval(true, false, 1.0).unwrap_or(1.0) <= 1.0 / 30.0);
    }

    #[test]
    fn ambient_line_keeps_three_to_one_on_the_card_for_any_cover() {
        use ambient_breath::*;
        let levels = [0.0, 0.25, 0.5, 0.75, 0.93, 1.0];
        let mut covers: Vec<Vec<Rgb>> = levels
            .iter()
            .flat_map(|r| levels.iter().map(move |g| vec![[*r, *g, 0.85]]))
            .collect();
        covers.push(vec![]);
        covers.push(vec![[0.94, 0.92, 0.88], [0.91, 0.9, 0.86]]);
        let mut dim = Vec::new();
        for cover in &covers {
            for dark in [false, true] {
                let card = if dark { DARK_CARD } else { LIGHT_CARD };
                for opacity in [OPACITY, 1.0] {
                    let seen = contrast(over(line(cover, dark, opacity), card, opacity), card);
                    if seen < 3.0 {
                        dim.push((cover.first().copied(), dark, seen));
                    }
                }
            }
        }
        assert!(dim.is_empty(), "{:?}", &dim[..dim.len().min(4)]);
    }

    #[test]
    fn ambient_line_takes_a_visible_cover_colour_else_the_accent() {
        use ambient_breath::*;
        let deep = [0.1, 0.15, 0.4];
        let pale = [0.94, 0.92, 0.88];
        assert_eq!(line(&[pale, deep], false, OPACITY), deep);
        assert_eq!(line(&[pale], false, OPACITY), LIGHT_ACCENT);
        assert!((contrast([1.0; 3], [0.0; 3]) - 21.0).abs() < 1e-9);
    }

    #[test]
    fn ambient_accent_is_the_teal_text_token() {
        let teal = crate::tokens::COLORS
            .iter()
            .find(|c| c.name == "tealText")
            .map(|c| (c.light, c.dark));
        let bytes = |rgb: Rgb| rgb.map(|v| (v * 255.0).round() as u8);
        assert_eq!(
            teal,
            Some((
                bytes(ambient_breath::LIGHT_ACCENT),
                bytes(ambient_breath::DARK_ACCENT)
            ))
        );
    }

    #[test]
    fn power_knob_sits_inside_the_track_on_and_meets_the_ring_off() {
        use power_switch::*;
        assert_eq!(knob(true), (18.0, 18.0));
        assert_eq!(knob(false), (21.0, 0.5));
        let (size, x) = knob(true);
        assert_eq!(TRACK_WIDTH - x - size, 2.0);
        assert!(knob(false).0 < TRACK_HEIGHT);
    }

    #[test]
    fn badge_follows_choice_and_streaming() {
        assert_eq!(Badge::of(false, true), Badge::Off);
        assert_eq!(Badge::of(false, false), Badge::Off);
        assert_eq!(Badge::of(true, false), Badge::Ring);
        assert_eq!(Badge::of(true, true), Badge::Filled);
        assert_eq!(Badge::ring_width(18.0), 1.5);
        assert_eq!(Badge::ring_width(36.0), 2.0);
    }
}
