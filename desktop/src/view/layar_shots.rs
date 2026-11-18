//! The Windows photos of the LAYAR rows a headless window can show, to set
//! beside the Mac's. Each row's scene is built on a fresh App over example
//! data, once light and once dark, and saved as `<ROW-ID>-terang.png` and
//! `<ROW-ID>-gelap.png`, each with a `.json` list of its labelled elements
//! for scripts/screen_compare.py.
//!
//! `cargo test layar_photos -- --ignored --nocapture` (not `--release`:
//! build.rs leaves out the debug info the element lists are read from), with
//! `AUDIOSYNC_LAYAR_BATCH=B03` (or `B01,B05`) for some batches only and
//! `AUDIOSYNC_LAYAR_DIR` for another folder than the main checkout's ignored
//! `.build/layar`. The rows it could not set up are printed and written to
//! `not-set-up.txt` there.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use audiosync_core::calibrate_all::{
    AirPlayShared, Class, Measurement, RowState as SheetRowState, SharedState,
};
use audiosync_core::eq::{EqSettings, UserCurve};
use audiosync_core::policy::device_volume::Hint;
use audiosync_core::policy::source_quit::WhenSourceQuits;
use i_slint_backend_testing::{ElementHandle, ElementQuery};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, SharedString};

use super::tests::{app_over, hold_clock, leaked};
use super::{App, RenamePlace};
use crate::audio::calibration::{Failure, Phase, Step, Which};
use crate::now_playing_card::fake::FakeControls;
use crate::now_playing_card::{CardArtwork, CardTrack, PlayModes, Repeat};
use crate::ui_model::{
    self, AllPhase, AllProgress, AppState, Backend, CalibrationState, LeftOutRow, MicKind, MixApp,
    MockBackend, Notice, NoticeMessage, PresetError, PresetLeftOff, PresetsPreview, SheetRow,
    SheetState, SourceName, Speaker, SpeakerKind, Waiting, ZoneError,
};
use crate::window_rules::FocusInput;
use crate::{Motion, RowPart};

type Size = (f32, f32);

/// main.slint's preferred size and its smallest (min-width, min-height).
const MAIN: Size = (696.0, 632.0);
const MAIN_SMALLEST: Size = (696.0, 440.0);
/// equalizer.slint's fixed width; 420 is the height the older shot test used.
const EQUALIZER: Size = (520.0, 420.0);
const EQUALIZER_SMALLEST: Size = (520.0, 360.0);
/// help.slint's and remote.slint's preferred sizes.
const GUIDE: Size = (640.0, 600.0);
const REMOTE: Size = (460.0, 640.0);
/// flyout.slint's width; its height is its fit-height.
const FLYOUT_WIDTH: f32 = 300.0;

const SPEAKERS: &str = "local:speakers";
const HEADPHONES: &str = "local:headphones";
const KITCHEN: &str = "airplay:kitchen";
const DESK: &str = "local:desk";
const HEADSET: &str = "local:bt";
const BEDROOM: &str = "airplay:bedroom";

/// About 60 characters, for the long-name rows.
const LONG: &str = "Living Room Speaker Next to the Big Window by the Bookshelves";

/// Rows a headless window cannot show, and why.
const NOT_HEADLESS: [(&str, &str, &str); 2] = [
    (
        "B01",
        "MAIN-TRANSPORT-DISABLED",
        "the Now Playing card has no disabled transport state on Windows",
    ),
    (
        "B09",
        "NOTICE-WINDOW-LIST",
        "no written meaning says which window list the row shows",
    ),
];

type Setup = fn(&Scene) -> Result<Win, String>;

struct Row {
    batch: &'static str,
    id: &'static str,
    setup: Setup,
}

fn row(batch: &'static str, id: &'static str, setup: Setup) -> Row {
    Row { batch, id, setup }
}

#[derive(Clone, Copy)]
enum Shown {
    Main,
    Flyout,
    Equalizer,
    Settings,
    Guide,
    Remote,
    About,
}

/// The window a row photographs, at the size it is taken.
#[derive(Clone, Copy)]
struct Win(Shown, Size);

/// The example data the scene edits; the app's own changes reach it only
/// where a photo shows them.
struct Stage(Rc<RefCell<AppState>>);

impl Backend for Stage {
    fn snapshot(&self) -> AppState {
        self.0.borrow().clone()
    }
    fn set_running(&mut self, on: bool) {
        self.0.borrow_mut().running = on;
    }
    fn select_source(&mut self, _id: &str) {}
    fn select_input_pair(&mut self, _pair: u16) {}
    fn toggle_mix_app(&mut self, _executable: &str) {}
    fn select_mix(&mut self, _executables: &[String]) {}
    fn set_mix_all_except(&mut self, _on: bool) {}
    fn set_master_volume(&mut self, volume: f64) {
        self.0.borrow_mut().master_volume = volume;
    }
    fn set_speaker_enabled(&mut self, _id: &str, _on: bool) {}
    fn set_speaker_volume(&mut self, _id: &str, _volume: f64) {}
    fn set_speaker_delay(&mut self, _id: &str, _ms: f64) {}
    fn set_airplay_delay(&mut self, _ms: f64) {}
    fn set_auto_connect(&mut self, _id: &str, _on: bool) {}
    fn rename_speaker(&mut self, _id: &str, _name: &str) {}
    fn toggle_zone(&mut self, _id: &str) {}
    fn new_zone(&mut self, _name: &str) -> Result<(), ZoneError> {
        Ok(())
    }
    fn new_zone_with(&mut self, _speaker_id: &str, _name: &str) {}
    fn rename_zone(&mut self, _id: &str, _name: &str) {}
    fn delete_zone(&mut self, _id: &str) {}
    fn move_to_zone(&mut self, _speaker_id: &str, _zone_id: &str) {}
    fn remove_from_zone(&mut self, _speaker_id: &str) {}
    fn set_zone_volume(&mut self, _id: &str, _volume: f64) {}
    fn use_target_instead(&mut self, _speaker_id: &str) {}
    fn toggle_zone_from_phone(&mut self, _id: &str, _keep_off: &[String]) {}
    fn recall_preset_from_phone(&mut self, _id: &str, _keep_off: &[String]) {}
    fn set_equalizer(&mut self, settings: EqSettings) {
        self.0.borrow_mut().equalizer = settings;
    }
    fn set_equalizer_curves(&mut self, curves: Vec<UserCurve>) {
        self.0.borrow_mut().equalizer_curves = curves;
    }
    fn save_preset(&mut self, _name: &str) -> Result<(), PresetError> {
        Ok(())
    }
    fn rename_preset(&mut self, _id: &str, _name: &str) {}
    fn update_preset(&mut self, _id: &str) -> Result<(), PresetError> {
        Ok(())
    }
    fn delete_preset(&mut self, _id: &str) {}
    fn recall_preset(&mut self, _id: &str) {}
    fn dismiss_notice(&mut self, _id: u64) {}
    fn dismiss_all_notices(&mut self) {}
    fn refresh_devices(&mut self) {}
    fn set_when_source_quits(&mut self, _setting: WhenSourceQuits) {}
    fn set_resume_after_sleep(&mut self, _on: bool) {}
    fn input_peak(&mut self) -> Option<f32> {
        self.0
            .borrow()
            .source_id
            .starts_with("input:")
            .then_some(0.42)
    }
}

/// One photo's App: streaming on over the mock backend's example speakers,
/// the main window shown at its preferred size, no poll and no motion, so
/// nothing the scene sets is redrawn away before the photo.
struct Scene {
    app: Rc<App>,
    state: Rc<RefCell<AppState>>,
    media: FakeControls,
}

impl Scene {
    fn new(appearance: ui_model::Appearance) -> Scene {
        let mut data = MockBackend::new().snapshot();
        data.running = true;
        let state = Rc::new(RefCell::new(data));
        let app = app_over(Stage(state.clone()));
        app.poll_timer.stop();
        hold_clock();
        let media = FakeControls::default();
        app.set_media_controls(media.controls());
        app.main.global::<Motion>().set_on(false);
        app.apply_appearance(appearance);
        app.refresh();
        app.main.show().expect("the main window shows headless");
        let scene = Scene { app, state, media };
        scene.size(scene.app.main.window(), MAIN);
        scene
    }

    fn edit(&self, change: impl FnOnce(&mut AppState)) {
        change(&mut self.state.borrow_mut());
        self.app.refresh();
    }

    fn speaker(&self, id: &str, change: impl FnOnce(&mut Speaker)) -> Result<(), String> {
        {
            let mut state = self.state.borrow_mut();
            let speaker = state
                .speakers
                .iter_mut()
                .find(|speaker| speaker.id == id)
                .ok_or_else(|| format!("no example speaker {id}"))?;
            change(speaker);
        }
        self.app.refresh();
        Ok(())
    }

    fn load(&self, example: MockBackend) {
        self.edit(|state| *state = example.snapshot());
    }

    fn play(&self, track: CardTrack) {
        self.media.read(Some(track));
        self.app.sync_now_playing();
        render(self.app.main.window());
    }

    fn main(&self) -> Result<Win, String> {
        Ok(Win(Shown::Main, MAIN))
    }

    fn main_at(&self, size: Size) -> Result<Win, String> {
        self.size(self.app.main.window(), size);
        Ok(Win(Shown::Main, size))
    }

    fn flyout(&self) -> Result<Win, String> {
        self.app.prepare_flyout_open(false);
        self.app.flyout.show().map_err(|error| error.to_string())?;
        self.fit_flyout();
        Ok(Win(Shown::Flyout, (FLYOUT_WIDTH, 0.0)))
    }

    fn fit_flyout(&self) {
        let window = self.app.flyout.window();
        self.size(window, (FLYOUT_WIDTH, 720.0));
        self.size(window, (FLYOUT_WIDTH, self.app.flyout.get_fit_height()));
    }

    fn equalizer(&self, size: Size) -> Result<Win, String> {
        let equalizer = &self.app.equalizer();
        equalizer.show().map_err(|error| error.to_string())?;
        equalizer.invoke_enabled_toggled(true);
        equalizer.invoke_gain_changed(1, 6.0);
        equalizer.invoke_gain_changed(4, -4.5);
        self.size(equalizer.window(), size);
        Ok(Win(Shown::Equalizer, size))
    }

    fn settings(&self) -> Result<Win, String> {
        self.app.show_settings();
        // The window's height is bound to its content's; the focus scope
        // filling it reads that before any size is set.
        let height =
            ElementHandle::find_by_element_id(&self.app.settings(), "SettingsWindow::keys")
                .next()
                .map(|keys| keys.size().height)
                .filter(|height| *height > 0.0)
                .unwrap_or(640.0);
        let size = (460.0, height);
        self.size(leaked(self.app.settings()), size);
        Ok(Win(Shown::Settings, size))
    }

    fn guide(&self) -> Result<Win, String> {
        self.app.show_help();
        self.size(leaked(self.app.help()), GUIDE);
        Ok(Win(Shown::Guide, GUIDE))
    }

    fn remote(&self) -> Result<Win, String> {
        self.app.show_remote();
        self.size(leaked(self.app.remote_window()), REMOTE);
        Ok(Win(Shown::Remote, REMOTE))
    }

    fn window(&self, shown: Shown) -> &slint::Window {
        match shown {
            Shown::Main => self.app.main.window(),
            Shown::Flyout => self.app.flyout.window(),
            Shown::Equalizer => leaked(self.app.equalizer()),
            Shown::Settings => leaked(self.app.settings()),
            Shown::Guide => leaked(self.app.help()),
            Shown::Remote => leaked(self.app.remote_window()),
            Shown::About => leaked(self.app.about()),
        }
    }

    fn size(&self, window: &slint::Window, (width, height): Size) {
        window.set_size(LogicalSize::new(width, height));
        render(window);
    }

    /// The photo as a PNG and its elements as the JSON screen_compare.py reads.
    fn photo(&self, Win(shown, size): Win) -> Result<(Vec<u8>, String), String> {
        let window = self.window(shown);
        settle(window);
        match shown {
            Shown::Flyout => self.fit_flyout(),
            _ => self.size(window, size),
        }
        let shot = window.take_snapshot().map_err(|error| error.to_string())?;
        let elements = match shown {
            Shown::Main => ElementQuery::from_root(&self.app.main).find_all(),
            Shown::Flyout => ElementQuery::from_root(&self.app.flyout).find_all(),
            Shown::Equalizer => ElementQuery::from_root(&self.app.equalizer()).find_all(),
            Shown::Settings => ElementQuery::from_root(&self.app.settings()).find_all(),
            Shown::Guide => ElementQuery::from_root(&self.app.help()).find_all(),
            Shown::Remote => ElementQuery::from_root(&self.app.remote_window()).find_all(),
            Shown::About => ElementQuery::from_root(&self.app.about()).find_all(),
        };
        let listed = elements_json(&elements, &shot, window.scale_factor());
        Ok((png(shot.width(), shot.height(), shot.as_bytes()), listed))
    }
}

/// The labelled elements a photo shows. Slint's test API reads no fonts, so
/// `font` is null; the colours are sampled from the photo as the Mac renderer
/// samples its own (scripts/screenshots/render.swift, `sampledColours`).
fn elements_json(elements: &[ElementHandle], shot: &Snapshot, scale: f32) -> String {
    let (width, height) = (shot.width() as f32 / scale, shot.height() as f32 / scale);
    let listed: Vec<serde_json::Value> = elements
        .iter()
        .filter_map(|element| {
            let label = element
                .accessible_label()
                .filter(|label| !label.trim().is_empty())?;
            // A labelled group (This PC, AirPlay Delay) is a Narrator heading the
            // Mac has no layarKey for; its controls are listed on their own.
            if element.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Groupbox)
            {
                return None;
            }
            let (at, size) = (element.absolute_position(), element.size());
            let shown = size.width > 0.0
                && size.height > 0.0
                && element.computed_opacity() > 0.0
                && at.x < width
                && at.y < height
                && at.x + size.width > 0.0
                && at.y + size.height > 0.0;
            shown.then(|| {
                let (fg, bg) = sampled_colours(shot, scale, at, size);
                serde_json::json!({
                    "key": label.as_str(),
                    "frame": listed_frame(
                        element.accessible_role()
                            == Some(i_slint_backend_testing::AccessibleRole::Slider),
                        at,
                        size,
                    ),
                    "font": null,
                    "fg": fg,
                    "bg": bg,
                })
            })
        })
        .collect();
    serde_json::json!({ "window": [width, height], "elements": listed }).to_string()
}

/// How far inside its box the style's slider track starts and ends, measured
/// in the photos at 100 % (Fluent.slider-inset in widgets.slint says 10 px).
const SLIDER_TRACK_INSET: f32 = 8.0;

/// The frame listed for an element: for a horizontal slider, the style's
/// track rather than its box, since the Mac's slider frame is its track.
fn listed_frame(slider: bool, at: LogicalPosition, size: LogicalSize) -> [f32; 4] {
    if slider && size.width > size.height {
        let inset = SLIDER_TRACK_INSET;
        return [at.x + inset, at.y, size.width - 2.0 * inset, size.height];
    }
    [at.x, at.y, size.width, size.height]
}

#[test]
fn a_horizontal_slider_is_listed_by_its_track() {
    let (at, size) = (
        LogicalPosition::new(290.0, 133.0),
        LogicalSize::new(334.0, 14.0),
    );
    assert_eq!(listed_frame(true, at, size), [298.0, 133.0, 318.0, 14.0]);
    assert_eq!(listed_frame(false, at, size), [290.0, 133.0, 334.0, 14.0]);
    let band = LogicalSize::new(32.0, 200.0);
    assert_eq!(listed_frame(true, at, band), [290.0, 133.0, 32.0, 200.0]);
}

type Snapshot = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

/// The commonest pixel inside the frame as the background, and the one farthest
/// from it (channel sum, more than 96) as the foreground: the commonest far
/// pixel was often an anti-aliased edge, not the ink.
fn sampled_colours(
    shot: &Snapshot,
    scale: f32,
    at: LogicalPosition,
    size: LogicalSize,
) -> (Option<String>, Option<String>) {
    let span = |start: f32, length: f32, limit: u32| {
        let first = (start * scale).max(0.0) as u32;
        let end = (((start + length) * scale).max(0.0) as u32).min(limit);
        first..end
    };
    let pixels = shot.as_slice();
    let mut counts = std::collections::HashMap::<[u8; 3], u32>::new();
    for y in span(at.y, size.height, shot.height()) {
        for x in span(at.x, size.width, shot.width()) {
            let pixel = pixels[(y * shot.width() + x) as usize];
            *counts.entry([pixel.r, pixel.g, pixel.b]).or_default() += 1;
        }
    }
    let commonest = |keep: &dyn Fn(&[u8; 3]) -> bool| {
        counts
            .iter()
            .filter(|(rgb, _)| keep(rgb))
            .max_by_key(|(rgb, count)| (**count, **rgb))
            .map(|(rgb, _)| *rgb)
    };
    let Some(bg) = commonest(&|_| true) else {
        return (None, None);
    };
    let distance = |rgb: &[u8; 3]| {
        rgb.iter()
            .zip(bg)
            .map(|(a, b)| u32::from(a.abs_diff(b)))
            .sum::<u32>()
    };
    let farthest = counts
        .iter()
        .filter(|(rgb, _)| distance(rgb) > 96)
        .max_by_key(|(rgb, count)| (distance(rgb), **count, **rgb))
        .map(|(rgb, _)| *rgb);
    let hex = |[r, g, b]: [u8; 3]| format!("#{r:02x}{g:02x}{b:02x}");
    (farthest.map(hex), Some(hex(bg)))
}

#[test]
fn text_colour_is_the_pixel_farthest_from_the_background() {
    // 10x1: six background pixels, three anti-aliased edge pixels, one ink pixel.
    let mut shot = Snapshot::new(10, 1);
    let row = [
        [255, 255, 255],
        [255, 255, 255],
        [255, 255, 255],
        [255, 255, 255],
        [255, 255, 255],
        [255, 255, 255],
        [160, 160, 160],
        [160, 160, 160],
        [160, 160, 160],
        [32, 32, 32],
    ];
    for (pixel, [r, g, b]) in shot.make_mut_slice().iter_mut().zip(row) {
        *pixel = slint::Rgba8Pixel { r, g, b, a: 255 };
    }
    let (fg, bg) = sampled_colours(
        &shot,
        1.0,
        LogicalPosition::new(0.0, 0.0),
        LogicalSize::new(10.0, 1.0),
    );
    assert_eq!(fg.as_deref(), Some("#202020"));
    assert_eq!(bg.as_deref(), Some("#ffffff"));
}

/// Draws a frame of `window`, which builds the list rows a change added.
fn render(window: &slint::Window) {
    let _ = window.take_snapshot();
}

/// Lets the timers and animations run for 600 ms of Slint's held clock.
fn settle(window: &slint::Window) {
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        hold_clock();
        slint::platform::update_timers_and_animations();
        render(window);
    }
}

/// The first element of `component` with a size whose accessible label is
/// `label`, else one whose label starts with it.
fn find<T: ComponentHandle>(component: &T, label: &str) -> Result<ElementHandle, String> {
    let laid_out = |element: &ElementHandle| {
        let size = element.size();
        size.width > 0.0 && size.height > 0.0
    };
    if let Some(element) = ElementHandle::find_by_accessible_label(component, label).find(laid_out)
    {
        return Ok(element);
    }
    let start = label.to_owned();
    ElementQuery::from_root(component)
        .match_predicate(move |element| {
            element
                .accessible_label()
                .is_some_and(|text| text.starts_with(start.as_str()))
        })
        .find_all()
        .into_iter()
        .find(laid_out)
        .ok_or_else(|| format!("no control labelled \"{label}\""))
}

fn center(element: &ElementHandle) -> LogicalPosition {
    let (at, size) = (element.absolute_position(), element.size());
    LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0)
}

fn hover<T: ComponentHandle>(component: &T, label: &str) -> Result<(), String> {
    let position = center(&find(component, label)?);
    let window = component.window();
    window.dispatch_event(WindowEvent::PointerMoved { position });
    settle(window);
    Ok(())
}

fn click_element(window: &slint::Window, element: &ElementHandle) {
    let position = center(element);
    let button = PointerEventButton::Left;
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
    settle(window);
}

fn click<T: ComponentHandle>(component: &T, label: &str) -> Result<(), String> {
    click_element(component.window(), &find(component, label)?);
    Ok(())
}

fn scroll(window: &slint::Window, position: LogicalPosition, delta_y: f32) {
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y,
    });
    settle(window);
}

fn key(window: &slint::Window, key: Key) {
    let text: SharedString = key.into();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

/// A cover drawn here: a two-way gradient, so the ambient art has bands.
fn cover() -> CardArtwork {
    const SIDE: u32 = 240;
    let mut rgba = Vec::with_capacity((SIDE * SIDE * 4) as usize);
    for y in 0..SIDE {
        for x in 0..SIDE {
            rgba.extend_from_slice(&[(x * 255 / SIDE) as u8, 96, (y * 255 / SIDE) as u8, 255]);
        }
    }
    CardArtwork::new(SIDE, rgba)
}

fn track(playing: bool, with_cover: bool) -> CardTrack {
    CardTrack {
        app_id: "Spotify.exe".into(),
        title: "A track title".into(),
        artist: "Artist".into(),
        playing,
        artwork: with_cover.then(cover),
        cover_failed: false,
        modes: PlayModes {
            shuffle: Some(false),
            repeat: Some(Repeat::Off),
        },
    }
}

fn long_track() -> CardTrack {
    CardTrack {
        title: "A track title long enough to need more than the two lines the card shows".into(),
        artist: "An artist name long enough to run past the end of its line".into(),
        ..track(true, true)
    }
}

fn modes(s: &Scene, shuffle: bool, repeat: Repeat) -> Result<Win, String> {
    let mut playing = track(true, true);
    playing.modes = PlayModes {
        shuffle: Some(shuffle),
        repeat: Some(repeat),
    };
    s.play(playing);
    s.main()
}

/// Six more AirPlay speakers, so the list is longer than the window.
fn more_speakers(state: &mut AppState) {
    let Some(template) = state.speakers.iter().find(|s| s.id == BEDROOM).cloned() else {
        return;
    };
    for name in [
        "Office",
        "Garage",
        "Hallway",
        "Studio",
        "Patio",
        "Guest Room",
    ] {
        state.speakers.push(Speaker {
            id: format!("airplay:{}", name.to_lowercase()),
            name: name.into(),
            system_name: name.into(),
            ..template.clone()
        });
    }
}

fn rename(speaker: &mut Speaker) {
    speaker.name = LONG.into();
    speaker.system_name = LONG.into();
}

fn many_apps(state: &mut AppState) {
    state.mix_apps = (1..=30)
        .map(|n| MixApp {
            id: format!("app{n}.exe"),
            name: format!("App {n}"),
            checked: n % 4 == 0,
            running: true,
        })
        .collect();
}

fn several_apps<T: ComponentHandle>(component: &T) -> Result<(), String> {
    click(component, "Audio Source")?;
    click(component, "Several Apps")
}

fn options(s: &Scene, name: &str) -> Result<(), String> {
    let label = s.app.strings().invoke_speaker_options(name.into());
    click(&s.app.main, &label)
}

fn focus_row(s: &Scene, id: &str) {
    s.app.note_focus_input(FocusInput::Tab);
    s.app.main.set_focus_speaker_part(RowPart::Toggle);
    s.app.main.set_focus_speaker_id(id.into());
    render(s.app.main.window());
}

fn calibrating(s: &Scene, phase: Phase) -> Result<Win, String> {
    s.speaker(DESK, |desk| desk.enabled = true)?;
    let running = phase.is_running();
    s.edit(move |state| {
        state.calibration = CalibrationState {
            speaker_id: Some(DESK.into()),
            phase,
            busy: running,
            own_run_holds: running,
        }
    });
    s.app.toggle_sync(DESK);
    s.main()
}

/// The Calibrate All results over four speakers, as the card test builds them.
fn results(names: [&str; 4]) -> SheetState {
    let clear = Measurement {
        new_ms: 2040.0,
        uncertainty_ms: 1.2,
        difference_ms: 40.0,
        clamped: false,
    };
    let row = |name: &str, class, state| SheetRow {
        name: name.into(),
        class,
        old_ms: Some(2000.0),
        state,
        can_measure_again: true,
    };
    SheetState {
        rows: vec![
            row(names[0], Class::Local, SheetRowState::Reference),
            row(
                names[1],
                Class::Local,
                SheetRowState::Certain(Measurement {
                    new_ms: 132.0,
                    ..clear
                }),
            ),
            row(names[2], Class::AirPlay, SheetRowState::Certain(clear)),
            row(
                names[3],
                Class::AirPlay,
                SheetRowState::Uncertain(Measurement {
                    uncertainty_ms: 14.0,
                    ..clear
                }),
            ),
        ],
        airplay: Some(AirPlayShared {
            old_ms: Some(2000.0),
            new_ms: Some(2040.0),
            spread_ms: 23.0,
            certain_rows: 1,
            state: SharedState::Spread,
            clamped: false,
        }),
        ended: None,
        failure: None,
        again_ended: None,
        can_apply: true,
        applied: false,
        can_undo: false,
        undone: false,
    }
}

const SHEET_NAMES: [&str; 4] = ["Speakers", "Desk Speaker", "Kitchen", "Bedroom"];

/// Desk Speaker and Headset on, so Calibrate All has speakers to measure.
fn calibrate_all(s: &Scene, change: impl FnOnce(&mut AppState)) -> Result<(), String> {
    s.speaker(DESK, |desk| desk.enabled = true)?;
    s.speaker(HEADSET, |headset| headset.enabled = true)?;
    s.edit(change);
    Ok(())
}

fn left_out(s: &Scene, included: bool, only: bool) -> Result<Win, String> {
    s.speaker(HEADPHONES, |headphones| headphones.enabled = true)?;
    if !only {
        s.speaker(DESK, |desk| desk.enabled = true)?;
    }
    s.edit(|state| {
        if only {
            for speaker in &mut state.speakers {
                speaker.enabled = speaker.id == SPEAKERS || speaker.id == HEADPHONES;
            }
        }
        state.calibrate_all.left_out = vec![LeftOutRow {
            id: HEADPHONES.into(),
            name: "Headphones".into(),
            kind: MicKind::Headphones,
            included,
        }];
        state.calibrate_all.shown = true;
    });
    s.main()
}

fn notices(s: &Scene, messages: Vec<NoticeMessage>) {
    s.edit(|state| {
        state.notices = messages
            .into_iter()
            .zip(1u64..)
            .map(|(message, id)| Notice {
                id,
                subject: format!("subject {id}"),
                message,
                time: "09.40".into(),
                post: 0,
            })
            .collect();
    });
}

fn notice(s: &Scene, message: NoticeMessage) -> Result<Win, String> {
    notices(s, vec![message]);
    s.main()
}

fn music_player() -> SourceName {
    SourceName::Named("Music Player".into())
}

fn b01() -> Vec<Row> {
    vec![
        row("B01", "MAIN-IDLE", |s| {
            s.edit(|state| state.running = false);
            s.main()
        }),
        row("B01", "MAIN-PLAYING", |s| {
            s.play(track(true, true));
            s.main()
        }),
        row("B01", "MAIN-HEADER", |s| {
            s.play(track(true, true));
            s.main()
        }),
        row("B01", "MAIN-HEADER-OFF", |s| {
            s.edit(|state| state.running = false);
            s.main()
        }),
        row("B01", "MAIN-VOLUME-ROWS", |s| {
            s.speaker(DESK, |desk| desk.enabled = true)?;
            s.play(track(true, true));
            s.main()
        }),
        row("B01", "MAIN-ROW-HOVER", |s| {
            hover(&s.app.main, "Kitchen")?;
            s.main()
        }),
        row("B01", "MAIN-ROW-FOCUS", |s| {
            focus_row(s, KITCHEN);
            s.main()
        }),
        row("B01", "MAIN-SCROLLED", |s| {
            s.edit(more_speakers);
            let at = center(&find(&s.app.main, "Kitchen")?);
            scroll(s.app.main.window(), at, -240.0);
            s.main()
        }),
        row("B01", "MAIN-AMBIENT-REST", |s| {
            s.play(track(false, true));
            s.main()
        }),
        row("B01", "MAIN-NOWPLAYING-NOART", |s| {
            s.play(track(true, false));
            s.main()
        }),
        row("B01", "MAIN-NOWPLAYING-PAUSED", |s| {
            s.play(track(false, false));
            s.main()
        }),
        row("B01", "MAIN-SHUFFLE-OFF", |s| modes(s, false, Repeat::Off)),
        row("B01", "MAIN-SHUFFLE-ON", |s| modes(s, true, Repeat::Off)),
        row("B01", "MAIN-REPEAT-OFF", |s| modes(s, false, Repeat::Off)),
        row("B01", "MAIN-REPEAT-ALL", |s| modes(s, false, Repeat::All)),
        row("B01", "MAIN-REPEAT-ONE", |s| modes(s, false, Repeat::One)),
        row("B01", "MAIN-TRANSPORT-HOVER", |s| {
            s.play(track(true, true));
            hover(&s.app.main, "Next Track")?;
            s.main()
        }),
        row("B01", "MAIN-EQ-BUTTON-HOVER", |s| {
            hover(&s.app.main, "Equalizer")?;
            s.main()
        }),
        row("B01", "MAIN-INPUT-LEVEL", |s| {
            s.edit(|state| state.source_id = "input:line".into());
            s.main()
        }),
        row("B01", "MAIN-LIVEINPUT-NOTE", |s| {
            s.edit(|state| state.source_id = "input:line".into());
            s.speaker(KITCHEN, |kitchen| kitchen.enabled = true)?;
            s.main()
        }),
        row("B01", "MAIN-MASTER-MUTED", |s| {
            s.app.toggle_master_mute();
            s.main()
        }),
        row("B01", "MAIN-NETWORK-DIVIDERS", |s| {
            s.play(track(true, true));
            s.main()
        }),
        row("B01", "MAIN-LOCALNET-HINT", |s| {
            s.edit(|state| {
                state.airplay_none_found = true;
                state.speakers.retain(|s| s.kind != SpeakerKind::AirPlay);
            });
            s.main()
        }),
        row("B01", "MAIN-NO-SPEAKER-ON", |s| {
            s.edit(|state| {
                for speaker in &mut state.speakers {
                    speaker.enabled = false;
                }
            });
            s.play(track(true, true));
            s.main()
        }),
    ]
}

fn b02() -> Vec<Row> {
    vec![
        row("B02", "MAIN-LONGNAME", |s| {
            s.speaker(KITCHEN, rename)?;
            s.play(long_track());
            s.main()
        }),
        row("B02", "MAIN-EMPTY", |s| {
            s.edit(|state| state.speakers.clear());
            s.main()
        }),
        row("B02", "MAIN-SMALLEST", |s| s.main_at(MAIN_SMALLEST)),
        row("B02", "MAIN-SMALLEST-PLAYING", |s| {
            s.play(track(true, true));
            s.main_at(MAIN_SMALLEST)
        }),
        row("B02", "MAIN-SYSVOL-SMALLEST", |s| {
            s.app.toggle_sync(SPEAKERS);
            s.main_at(MAIN_SMALLEST)
        }),
        row("B02", "MAIN-SOURCE-APPS-SMALLEST", |s| {
            let win = s.main_at(MAIN_SMALLEST)?;
            several_apps(&s.app.main)?;
            Ok(win)
        }),
        row("B02", "CAL-SHEET-SMALLEST", |s| {
            let win = s.main_at(MAIN_SMALLEST)?;
            calibrate_all(s, |state| {
                state.calibrate_all.sheet = Some(results(SHEET_NAMES));
            })?;
            Ok(win)
        }),
        row("B02", "CAL-SHEET-LONGNAME", |s| {
            calibrate_all(s, |state| {
                state.calibrate_all.sheet = Some(results(["Speakers", LONG, "Kitchen", LONG]));
            })?;
            s.main()
        }),
        row("B02", "MAIN-DELAY-LONGNAME", |s| {
            s.speaker(DESK, |desk| {
                rename(desk);
                desk.enabled = true;
            })?;
            s.app.toggle_sync(DESK);
            s.main()
        }),
        row("B02", "MAIN-ROW-LOWVOL-LONGNAME", |s| {
            s.speaker(DESK, |desk| {
                rename(desk);
                desk.enabled = true;
                desk.device_volume = Some(Hint::Low { percent: 18 });
            })?;
            s.main()
        }),
        row("B02", "MAIN-ROW-THISPC-LONG", |s| {
            s.speaker(SPEAKERS, rename)?;
            s.main()
        }),
        row("B02", "MAIN-ROW-HINT-LONG", |s| {
            s.speaker(SPEAKERS, |speakers| {
                rename(speakers);
                speakers.device_volume = Some(Hint::Low { percent: 8 });
            })?;
            s.main()
        }),
        row("B02", "MAIN-ROW-HINT-TALL", |s| {
            s.speaker(SPEAKERS, |speakers| {
                rename(speakers);
                speakers.device_volume = Some(Hint::MutedAndLow { percent: 8 });
            })?;
            s.main()
        }),
        row("B02", "CAL-SHEET-LEFTOUT-LONG", |s| {
            s.speaker(HEADPHONES, |headphones| {
                rename(headphones);
                headphones.enabled = true;
            })?;
            s.speaker(DESK, |desk| desk.enabled = true)?;
            s.edit(|state| {
                state.calibrate_all.left_out = vec![LeftOutRow {
                    id: HEADPHONES.into(),
                    name: LONG.into(),
                    kind: MicKind::Headphones,
                    included: false,
                }];
                state.calibrate_all.shown = true;
            });
            s.main()
        }),
        row("B02", "MAIN-THISPC-NONE", |s| {
            s.edit(|state| state.speakers.retain(|s| !s.kind.is_this_pc()));
            s.main()
        }),
    ]
}

fn b03() -> Vec<Row> {
    vec![
        row("B03", "MAIN-ROW-OFF", |s| {
            s.speaker(KITCHEN, |kitchen| kitchen.enabled = false)?;
            s.main()
        }),
        row("B03", "MAIN-ROW-OFF-HOVER", |s| {
            s.speaker(KITCHEN, |kitchen| kitchen.enabled = false)?;
            hover(&s.app.main, "Kitchen")?;
            s.main()
        }),
        row("B03", "MAIN-ROW-WILLPLAY", |s| {
            s.edit(|state| state.running = false);
            s.main()
        }),
        row("B03", "MAIN-ROW-LOWVOL", |s| {
            s.speaker(DESK, |desk| {
                desk.enabled = true;
                desk.device_volume = Some(Hint::Low { percent: 18 });
            })?;
            s.main()
        }),
        row("B03", "MAIN-ROW-LOWVOL-HEADPHONES", |s| {
            s.speaker(HEADPHONES, |headphones| {
                headphones.enabled = true;
                headphones.device_volume = Some(Hint::Low { percent: 10 });
            })?;
            s.main()
        }),
        row("B03", "MAIN-ROW-MUTED-DEVICE", |s| {
            s.speaker(DESK, |desk| {
                desk.enabled = true;
                desk.device_volume = Some(Hint::Muted);
            })?;
            s.main()
        }),
        row("B03", "MAIN-ROW-THISPC", |s| {
            s.speaker(SPEAKERS, |speakers| speakers.enabled = true)?;
            s.main()
        }),
        row("B03", "MAIN-ROW-THISPC-OFF", |s| {
            s.speaker(SPEAKERS, |speakers| speakers.enabled = false)?;
            s.main()
        }),
        row("B03", "MAIN-ROW-CONNECTING", |s| {
            s.speaker(KITCHEN, |kitchen| {
                kitchen.waiting = Some(Waiting::ForDevice)
            })?;
            s.main()
        }),
        row("B03", "MAIN-ROW-NEEDSCODE", |s| {
            s.speaker("airplay:living", |living| {
                living.availability = ui_model::Availability::NeedsCode;
            })?;
            s.main()
        }),
        row("B03", "MAIN-ROW-CODE-EVERYTIME", |s| {
            s.speaker(BEDROOM, |bedroom| bedroom.code_every_time = true)?;
            s.main()
        }),
        row("B03", "MAIN-ROW-CANTPLAY", |s| {
            s.speaker(KITCHEN, |kitchen| {
                kitchen.availability = ui_model::Availability::Unsupported;
            })?;
            s.main()
        }),
        row("B03", "MAIN-ROW-PROBLEM", |s| {
            s.speaker(KITCHEN, |kitchen| {
                kitchen.waiting = Some(Waiting::NotOnNetwork);
            })?;
            s.main()
        }),
        row("B03", "MAIN-ROW-DEVICE-TURNED-DOWN", |s| {
            s.speaker(DESK, |desk| {
                desk.enabled = true;
                desk.device_volume = Some(Hint::MutedAndLow { percent: 5 });
            })?;
            s.main()
        }),
        row("B03", "MAIN-ROW-RENAME", |s| {
            s.app.start_rename(KITCHEN, RenamePlace::Row);
            s.main()
        }),
        row("B03", "MAIN-ROW-CAL-RUNNING", |s| {
            calibrating(s, Phase::Running(Step::Measuring(Which::Chosen)))
        }),
        row("B03", "MAIN-ROW-CAL-DONE", |s| {
            calibrating(
                s,
                Phase::Done {
                    offset_ms: 132.0,
                    clamped: false,
                },
            )
        }),
        row("B03", "MAIN-ROW-CAL-FAILED", |s| {
            calibrating(s, Phase::Failed(Failure::SourceSilent))
        }),
    ]
}

fn b04() -> Vec<Row> {
    vec![
        row("B04", "MAIN-ROW-MENU", |s| {
            options(s, "Kitchen")?;
            s.main()
        }),
        row("B04", "MAIN-ROW-MOVE-ZONE-MENU", |s| {
            s.load(MockBackend::zones_preview());
            options(s, "Headset")?;
            click(&s.app.main, "Move to Zone")?;
            s.main()
        }),
        row("B04", "MAIN-SOURCE-MENU", |s| {
            click(&s.app.main, "Audio Source")?;
            s.main()
        }),
        row("B04", "MAIN-SOURCE-MENU-LONG", |s| {
            s.edit(many_apps);
            several_apps(&s.app.main)?;
            s.main()
        }),
        row("B04", "MAIN-SOURCE-APPS", |s| {
            several_apps(&s.app.main)?;
            s.main()
        }),
        row("B04", "MAIN-SOURCE-APPS-LONG", |s| {
            s.edit(many_apps);
            several_apps(&s.app.main)?;
            s.main()
        }),
        row("B04", "MAIN-SOURCE-APPS-EMPTY", |s| {
            s.edit(|state| state.mix_apps.clear());
            several_apps(&s.app.main)?;
            s.main()
        }),
        row("B04", "MAIN-GEAR-MENU", |s| {
            // As the Mac's release build, whose app menu has Check for Updates….
            s.app.main.set_updater_available(true);
            click(&s.app.main, "Options")?;
            s.main()
        }),
        row("B04", "LEVEL-POPUP", |s| {
            s.edit(|state| state.source_id = "input:line".into());
            let picker = ElementHandle::find_by_element_type_name(&s.app.main, "PairPicker")
                .next()
                .ok_or("no channel pair picker")?;
            click_element(s.app.main.window(), &picker);
            s.main()
        }),
        row("B04", "TRAY-SOURCE-MENU", |s| {
            let win = s.flyout()?;
            click(&s.app.flyout, "Audio Source")?;
            Ok(win)
        }),
        row("B04", "TRAY-SOURCE-APPS", |s| {
            let win = s.flyout()?;
            several_apps(&s.app.flyout)?;
            Ok(win)
        }),
        row("B04", "TRAY-SOURCE-APPS-LONG", |s| {
            s.edit(many_apps);
            let win = s.flyout()?;
            several_apps(&s.app.flyout)?;
            Ok(win)
        }),
    ]
}

fn b05() -> Vec<Row> {
    vec![
        row("B05", "MAIN-DELAY-OPEN", |s| {
            s.speaker(DESK, |desk| desk.enabled = true)?;
            s.app.toggle_sync(DESK);
            s.main()
        }),
        row("B05", "MAIN-DELAY-ROWS", |s| {
            s.speaker(DESK, |desk| desk.enabled = true)?;
            s.speaker(HEADSET, |headset| headset.enabled = true)?;
            s.main()
        }),
        row("B05", "MAIN-DELAY-EDIT", |s| {
            s.speaker(DESK, |desk| desk.enabled = true)?;
            s.app.toggle_sync(DESK);
            s.app.edit_delay(DESK, "150");
            s.main()
        }),
        row("B05", "MAIN-AIRPLAY-DELAY-FOLDED", |s| {
            s.edit(|state| state.airplay_delay_ms = None);
            s.main()
        }),
        row("B05", "MAIN-AIRPLAY-DELAY-OPEN", |s| s.main()),
        row("B05", "MAIN-SYSVOL-FOLDED", |s| s.main()),
        row("B05", "MAIN-SYSVOL-OPEN", |s| {
            s.app.toggle_sync(SPEAKERS);
            s.main()
        }),
        row("B05", "MAIN-SYSVOL-PANEL", |s| {
            s.app.toggle_sync(SPEAKERS);
            s.main()
        }),
    ]
}

fn b06() -> Vec<Row> {
    vec![
        row("B06", "CAL-SHEET", |s| {
            calibrate_all(s, |state| state.calibrate_all.shown = true)?;
            s.main()
        }),
        row("B06", "CAL-SHEET-RUNNING", |s| {
            calibrate_all(s, |state| {
                state.calibrate_all.phase = AllPhase::Running(AllProgress::Measuring {
                    name: "Desk Speaker".into(),
                    reference: false,
                    take: 1,
                    searching: false,
                });
            })?;
            s.main()
        }),
        row("B06", "CAL-SHEET-RESULT", |s| {
            calibrate_all(s, |state| {
                state.calibrate_all.sheet = Some(results(SHEET_NAMES));
            })?;
            s.main()
        }),
        row("B06", "CAL-SHEET-FAILED", |s| {
            calibrate_all(s, |state| {
                let mut sheet = results(SHEET_NAMES);
                for row in sheet.rows.iter_mut().skip(1) {
                    row.state = SheetRowState::NotMeasured;
                }
                for row in &mut sheet.rows {
                    row.can_measure_again = false;
                }
                sheet.airplay = None;
                sheet.failure = Some(Failure::SourceSilent);
                sheet.can_apply = false;
                state.calibrate_all.sheet = Some(sheet);
            })?;
            s.main()
        }),
        row("B06", "CAL-SHEET-LEFTOUT", |s| left_out(s, false, false)),
        row("B06", "CAL-SHEET-INCLUDED", |s| left_out(s, true, false)),
        row("B06", "CAL-SHEET-ONLY-LEFTOUT", |s| {
            left_out(s, false, true)
        }),
        row("B06", "MAIN-CALIBRATE-SHEET-NARROW", |s| {
            let win = s.main_at(MAIN_SMALLEST)?;
            calibrate_all(s, |state| {
                state.calibrate_all.sheet = Some(results(SHEET_NAMES));
            })?;
            Ok(win)
        }),
    ]
}

fn b07() -> Vec<Row> {
    vec![
        row("B07", "MAIN-ZONE-CARD", |s| {
            s.load(MockBackend::zones_preview());
            s.main()
        }),
        row("B07", "MAIN-ZONE-COLLAPSED", |s| {
            s.load(MockBackend::zones_preview());
            s.app.set_zone_expanded("zone:upstairs", false, false);
            s.main()
        }),
        row("B07", "MAIN-ZONE-MENU", |s| {
            s.load(MockBackend::zones_preview());
            options(s, "Upstairs")?;
            s.main()
        }),
        row("B07", "MAIN-ZONE-RENAME", |s| {
            s.load(MockBackend::zones_preview());
            s.app.start_rename("zone:upstairs", RenamePlace::Zone);
            s.main()
        }),
        row("B07", "MAIN-ZONE-PLACEHOLDER", |s| {
            s.load(MockBackend::zones_preview());
            let at = center(&find(&s.app.main, "Upstairs")?);
            scroll(s.app.main.window(), at, -1200.0);
            s.main()
        }),
        row("B07", "MAIN-PRESETS", |s| {
            s.load(MockBackend::presets_preview(PresetsPreview::List));
            s.main()
        }),
        row("B07", "MAIN-PRESET-LIST", |s| {
            s.load(MockBackend::presets_preview(PresetsPreview::List));
            s.app.main.set_presets_open(true);
            s.main()
        }),
        row("B07", "MAIN-PRESET-ROW-MENU", |s| {
            s.load(MockBackend::presets_preview(PresetsPreview::List));
            s.app.main.set_presets_open(true);
            render(s.app.main.window());
            options(s, "Evening")?;
            s.main()
        }),
        row("B07", "MAIN-PRESET-RENAME", |s| {
            s.load(MockBackend::presets_preview(PresetsPreview::List));
            s.app.main.set_presets_open(true);
            let first = s.state.borrow().presets.first().map(|p| p.id.clone());
            s.app
                .main
                .set_preset_renaming(first.ok_or("no example preset")?.into());
            s.main()
        }),
        row("B07", "MAIN-PRESETS-EMPTY", |s| {
            s.load(MockBackend::presets_preview(PresetsPreview::Empty));
            s.app.main.set_presets_open(true);
            s.main()
        }),
        row("B07", "MAIN-ADD-ADDRESS", |s| {
            s.app.ask_cast_address();
            s.main()
        }),
        row("B07", "MAIN-ADD-ADDRESS-ERROR", |s| {
            s.app.ask_cast_address();
            render(s.app.main.window());
            s.app.cast_sheet().invoke_confirmed("speaker".into());
            s.main()
        }),
        row("B07", "MAIN-CAST-SEARCHING", |s| {
            s.edit(|state| state.cast_list = ui_model::CastList::Searching);
            s.main()
        }),
        row("B07", "MAIN-CAST-NONEFOUND", |s| {
            s.edit(|state| state.cast_list = ui_model::CastList::NoneFound);
            s.main()
        }),
        row("B07", "MAIN-CAST-FAILED", |s| {
            s.edit(|state| state.cast_list = ui_model::CastList::Failed);
            s.main()
        }),
    ]
}

fn b08() -> Vec<Row> {
    vec![
        row("B08", "TRAY-IDLE", |s| {
            s.edit(|state| state.running = false);
            s.flyout()
        }),
        row("B08", "TRAY-HEADER-OFF", |s| {
            s.edit(|state| state.running = false);
            s.flyout()
        }),
        row("B08", "TRAY-PLAYING", |s| {
            s.play(track(true, true));
            s.flyout()
        }),
        row("B08", "TRAY-EMPTY", |s| {
            s.edit(|state| state.speakers.clear());
            s.flyout()
        }),
        row("B08", "TRAY-LONGNAME", |s| {
            s.speaker(KITCHEN, rename)?;
            s.play(long_track());
            s.flyout()
        }),
        row("B08", "TRAY-HOVER", |s| {
            let win = s.flyout()?;
            hover(&s.app.flyout, "Kitchen")?;
            Ok(win)
        }),
        row("B08", "TRAY-ROW-OFF", |s| {
            s.speaker(KITCHEN, |kitchen| kitchen.enabled = false)?;
            s.flyout()
        }),
        row("B08", "TRAY-ROW-OFF-HOVER", |s| {
            s.speaker(KITCHEN, |kitchen| kitchen.enabled = false)?;
            let win = s.flyout()?;
            hover(&s.app.flyout, "Kitchen")?;
            Ok(win)
        }),
        row("B08", "TRAY-ROW-LOWVOL", |s| {
            s.speaker(DESK, |desk| {
                desk.enabled = true;
                desk.device_volume = Some(Hint::Low { percent: 18 });
            })?;
            s.flyout()
        }),
        row("B08", "TRAY-ROW-THISPC", |s| s.flyout()),
        row("B08", "TRAY-ROW-HINT-TALL", |s| {
            s.speaker(SPEAKERS, |speakers| {
                rename(speakers);
                speakers.device_volume = Some(Hint::MutedAndLow { percent: 8 });
            })?;
            s.flyout()
        }),
        row("B08", "TRAY-TRANSPORT", |s| {
            let mut playing = track(true, true);
            playing.modes.shuffle = Some(true);
            playing.modes.repeat = Some(Repeat::All);
            s.play(playing);
            s.flyout()
        }),
        row("B08", "TRAY-AMBIENT-REST", |s| {
            s.play(track(false, true));
            s.flyout()
        }),
        row("B08", "TRAY-MASTER-MUTED", |s| {
            s.app.toggle_master_mute();
            s.flyout()
        }),
        row("B08", "TRAY-ZONE-HEADER", |s| {
            s.load(MockBackend::zones_preview());
            s.flyout()
        }),
        row("B08", "TRAY-PRESET-PICKER", |s| {
            s.load(MockBackend::presets_preview(PresetsPreview::List));
            let win = s.flyout()?;
            click(&s.app.flyout, "Presets")?;
            Ok(win)
        }),
        row("B08", "TRAY-NOTICE", |s| {
            notices(
                s,
                vec![NoticeMessage::SourceQuit {
                    source: music_player(),
                }],
            );
            s.flyout()
        }),
    ]
}

fn b09() -> Vec<Row> {
    vec![
        row("B09", "NOTICE-SOURCE", |s| {
            notice(
                s,
                NoticeMessage::SourceQuit {
                    source: music_player(),
                },
            )
        }),
        row("B09", "NOTICE-START", |s| {
            notice(s, NoticeMessage::NoSpeaker)
        }),
        row("B09", "NOTICE-CAPTURE", |s| {
            notice(s, NoticeMessage::CaptureLost)
        }),
        row("B09", "NOTICE-LOGIN", |s| {
            notice(s, NoticeMessage::LaunchAtLoginFailed)
        }),
        row("B09", "NOTICE-AIRPLAY-SENDER", |s| {
            notice(
                s,
                NoticeMessage::AirPlayCantPlay {
                    speaker: "Kitchen".into(),
                    problem: ui_model::AirPlayProblem::NotResponding,
                },
            )
        }),
        row("B09", "NOTICE-AIRPLAY-PTP", |s| {
            notice(s, NoticeMessage::AirPlayPtpPortsTaken)
        }),
        row("B09", "NOTICE-PAIRINGS", |s| {
            notice(s, NoticeMessage::PairingsNotSaved)
        }),
        row("B09", "NOTICE-PAIRINGS-IGNORED", |s| {
            notice(
                s,
                NoticeMessage::PairingsKeptAside {
                    file: "pairings.json.unreadable".into(),
                },
            )
        }),
        row("B09", "NOTICE-ZONES", |s| {
            notice(s, NoticeMessage::GroupsBecameZones)
        }),
        row("B09", "NOTICE-PRESETS", |s| {
            notice(
                s,
                NoticeMessage::PresetLeftOff {
                    preset: "Whole House".into(),
                    left_off: vec![("Patio".into(), PresetLeftOff::NotFound)],
                    nothing_changed: false,
                },
            )
        }),
        row("B09", "NOTICE-CAST-SENDER", |s| {
            notice(
                s,
                NoticeMessage::CastCantPlay {
                    speaker: "Porch".into(),
                    problem: ui_model::CastProblem::Busy,
                },
            )
        }),
        row("B09", "NOTICE-AIRPLAY-AHEAD", |s| {
            notice(
                s,
                NoticeMessage::AirPlayAheadOfCast {
                    speaker: "Porch".into(),
                    tenths: 4,
                },
            )
        }),
        row("B09", "NOTICE-PERMISSION", |s| {
            notice(
                s,
                NoticeMessage::MicrophoneBlocked {
                    source: "Line In".into(),
                },
            )
        }),
        row("B09", "NOTICE-STACK", |s| {
            notices(
                s,
                vec![
                    NoticeMessage::SourceQuit {
                        source: music_player(),
                    },
                    NoticeMessage::CaptureLost,
                    NoticeMessage::AirPlayPtpPortsTaken,
                    NoticeMessage::GroupsBecameZones,
                ],
            );
            s.main()
        }),
        row("B09", "NOTICE-LONG", |s| {
            notice(
                s,
                NoticeMessage::AirPlayCantPlay {
                    speaker: LONG.into(),
                    problem: ui_model::AirPlayProblem::AirPlay2NotYet,
                },
            )
        }),
    ]
}

fn b10() -> Vec<Row> {
    vec![
        row("B10", "EQ-WINDOW", |s| {
            let win = s.equalizer(EQUALIZER)?;
            s.app.equalizer().invoke_take_focus();
            s.app.note_focus_input(FocusInput::Tab);
            for _ in 0..4 {
                key(leaked(s.app.equalizer()), Key::Tab);
            }
            Ok(win)
        }),
        row("B10", "EQ-CURVE-MENU", |s| {
            let win = s.equalizer(EQUALIZER)?;
            click(&s.app.equalizer(), "Curve")?;
            Ok(win)
        }),
        row("B10", "EQ-SLIDER-DRAG", |s| {
            let win = s.equalizer(EQUALIZER)?;
            let at = center(&find(&s.app.equalizer(), "32 Hz")?);
            let window = leaked(s.app.equalizer());
            let button = PointerEventButton::Left;
            window.dispatch_event(WindowEvent::PointerMoved { position: at });
            window.dispatch_event(WindowEvent::PointerPressed {
                position: at,
                button,
            });
            let position = LogicalPosition::new(at.x, at.y - 30.0);
            window.dispatch_event(WindowEvent::PointerMoved { position });
            settle(window);
            Ok(win)
        }),
        row("B10", "EQ-SMALLEST", |s| s.equalizer(EQUALIZER_SMALLEST)),
    ]
}

fn running_remote(s: &Scene) -> Result<Win, String> {
    let win = s.remote()?;
    let remote = &s.app.remote_window();
    let local = "http://den.local:8770/";
    remote.set_remote_on(true);
    remote.set_listening(true);
    remote.set_local_address(local.into());
    remote.set_ip_address("http://192.0.2.10:8770/".into());
    remote.set_code("4821".into());
    remote.set_code_spoken("4 8 2 1".into());
    remote.set_devices(1);
    let (side, pixels) = crate::remote::qr_pixels(local, 4).ok_or("no QR code")?;
    let side = u32::try_from(side).map_err(|error| error.to_string())?;
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side, side);
    for (pixel, [r, g, b]) in buffer.make_mut_slice().iter_mut().zip(pixels) {
        *pixel = slint::Rgb8Pixel { r, g, b };
    }
    remote.set_qr(slint::Image::from_rgb8(buffer));
    remote.set_has_qr(true);
    Ok(win)
}

fn b11() -> Vec<Row> {
    vec![
        row("B11", "ABOUT-PANEL", |s| {
            let about = s.app.about();
            about.set_version("0.9.3".into());
            about.show().map_err(|error| error.to_string())?;
            // A headless dialog has no size of its own.
            Ok(Win(Shown::About, (360.0, 340.0)))
        }),
        row("B11", "MAIN-SETTINGS", |s| s.settings()),
        row("B11", "SETTINGS-UPDATES", |s| {
            s.app.main.set_updater_available(true);
            s.app.main.set_check_updates_automatically(true);
            s.settings()
        }),
        row("B11", "SETTINGS-SWITCH-OFF", |s| {
            let main = &s.app.main;
            main.set_updater_available(true);
            main.set_check_updates_automatically(false);
            main.set_launch_at_login(false);
            main.set_resume_after_sleep(false);
            main.set_switch_source_on_quit(false);
            main.set_volume_keys_available(true);
            main.set_volume_keys(false);
            s.settings()
        }),
        row("B11", "GUIDE-WINDOW", |s| s.guide()),
        row("B11", "GUIDE-SCROLLED", |s| {
            let win = s.guide()?;
            let at = LogicalPosition::new(GUIDE.0 / 2.0, GUIDE.1 / 2.0);
            scroll(leaked(s.app.help()), at, -600.0);
            Ok(win)
        }),
        row("B11", "WEBREMOTE-OFF", |s| s.remote()),
        row("B11", "WEBREMOTE-RUNNING", running_remote),
        row("B11", "WEBREMOTE-ANDROID", |s| {
            let win = running_remote(s)?;
            s.app.remote_window().set_steps_for_iphone(false);
            Ok(win)
        }),
        row("B11", "WEBREMOTE-FAILED", |s| {
            let win = s.remote()?;
            let remote = &s.app.remote_window();
            remote.set_remote_on(true);
            remote.set_listening(false);
            remote.set_problem(s.app.strings().get_remote_failed());
            Ok(win)
        }),
    ]
}

/// The main checkout's `.build/layar`, from any worktree.
fn photo_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AUDIOSYNC_LAYAR_DIR") {
        return PathBuf::from(dir);
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let common = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(manifest)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()));
    let checkout = common
        .as_deref()
        .and_then(Path::parent)
        .map_or_else(|| manifest.join(".."), Path::to_path_buf);
    checkout.join(".build").join("layar")
}

#[test]
#[ignore = "writes the LAYAR photos: run with --ignored, AUDIOSYNC_LAYAR_BATCH picks batches"]
fn layar_photos_of_the_headless_rows() {
    let dir = photo_dir();
    std::fs::create_dir_all(&dir).expect("the photo folder is made");
    let batches = std::env::var("AUDIOSYNC_LAYAR_BATCH").unwrap_or_default();
    let wanted = |batch: &str| {
        batches.trim().is_empty()
            || batches
                .split(',')
                .any(|asked| asked.trim().eq_ignore_ascii_case(batch))
    };
    let rows = [
        b01(),
        b02(),
        b03(),
        b04(),
        b05(),
        b06(),
        b07(),
        b08(),
        b09(),
        b10(),
        b11(),
    ];
    let mut taken = 0;
    let mut missed = Vec::new();
    for row in rows.into_iter().flatten().filter(|row| wanted(row.batch)) {
        for (theme, appearance) in [
            ("terang", ui_model::Appearance::Light),
            ("gelap", ui_model::Appearance::Dark),
        ] {
            let scene = Scene::new(appearance);
            match (row.setup)(&scene).and_then(|win| scene.photo(win)) {
                Ok((file, elements)) => {
                    let path = dir.join(format!("{}-{theme}.png", row.id));
                    std::fs::write(path, file).expect("the photo is written");
                    let path = dir.join(format!("{}-{theme}.json", row.id));
                    std::fs::write(path, elements).expect("the element list is written");
                    taken += 1;
                }
                Err(why) => missed.push(format!("{} {theme}: {why}", row.id)),
            }
        }
    }
    for (batch, id, why) in NOT_HEADLESS {
        if wanted(batch) {
            missed.push(format!("{id}: {why}"));
        }
    }
    std::fs::write(dir.join("not-set-up.txt"), missed.join("\n") + "\n")
        .expect("the list of rows not set up is written");
    println!("{taken} photos in {}", dir.display());
    if !missed.is_empty() {
        println!("Not set up headless:");
        for line in &missed {
            println!("  {line}");
        }
    }
}

/// An uncompressed RGBA PNG (stored deflate blocks): the Windows build has
/// no image crate, and view.rs keeps its own encoder private to its tests.
fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut raw = Vec::with_capacity(rgba.len() + height as usize);
    for line in rgba.chunks(width as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks = raw.len().div_ceil(0xFFFF);
    for (index, block) in raw.chunks(0xFFFF).enumerate() {
        zlib.push(u8::from(index + 1 == blocks));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut header = [0u8; 13];
    header[..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8..10].copy_from_slice(&[8, 6]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// One frame of a 60 Hz display on Slint's held clock.
const FRAME: Duration = Duration::from_micros(16_667);

/// Draws `frames` frames of the main window 1/60 s apart on the held clock,
/// saving each as `<dir>/<name>/NN.png`, and returns a line per frame: its
/// time, the gap since the one before, and how long the UI thread took to
/// lay it out and draw it.
fn record(scene: &Scene, dir: &Path, name: &str, frames: u32) -> Vec<String> {
    let folder = dir.join(name);
    std::fs::create_dir_all(&folder).expect("the frame folder is made");
    let window = scene.app.main.window();
    let mut lines = vec![format!("{name}: frame, time ms, gap ms, UI thread ms")];
    for index in 0..frames {
        if index > 0 {
            super::tests::advance_clock(FRAME);
        }
        let started = std::time::Instant::now();
        slint::platform::update_timers_and_animations();
        let shot = window.take_snapshot().expect("the frame is drawn");
        let took = started.elapsed();
        let path = folder.join(format!("{index:02}.png"));
        std::fs::write(path, png(shot.width(), shot.height(), shot.as_bytes()))
            .expect("the frame is written");
        let gap = if index == 0 {
            0.0
        } else {
            FRAME.as_secs_f64() * 1000.0
        };
        lines.push(format!(
            "{index:02}  {:7.1}  {gap:5.1}  {:6.1}",
            f64::from(index) * FRAME.as_secs_f64() * 1000.0,
            took.as_secs_f64() * 1000.0
        ));
    }
    lines
}

#[test]
#[ignore = "writes motion frames: run with --ignored, AUDIOSYNC_LAYAR_DIR picks the folder"]
fn motion_frames_of_a_song_change_and_a_speaker_switched_off() {
    let dir = photo_dir().join("motion");
    let scene = Scene::new(ui_model::Appearance::Light);
    scene.app.main.global::<Motion>().set_on(true);
    scene
        .speaker(SPEAKERS, |speaker| speaker.enabled = true)
        .expect("the example speakers");
    scene.play(track(true, true));
    settle(scene.app.main.window());
    scene.app.main.invoke_now_playing_transport(2);
    let mut lines = record(&scene, &dir, "next-press", 12);
    let mut next = track(true, true);
    next.title = "Another title".into();
    next.artist = "Another artist".into();
    scene.media.read(Some(next));
    scene.app.main.invoke_now_playing_read();
    lines.extend(record(&scene, &dir, "song-change", 40));
    scene
        .speaker(SPEAKERS, |speaker| speaker.enabled = false)
        .expect("the example speakers");
    lines.extend(record(&scene, &dir, "speaker-off", 20));
    std::fs::write(dir.join("frames.txt"), lines.join("\n") + "\n")
        .expect("the frame list is written");
    println!("{}", lines.join("\n"));
}
