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
use audiosync_core::policy::zones::Zones;
use i_slint_backend_testing::{ElementHandle, ElementQuery};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize};

use super::tests::{advance_clock, app_over, hold_clock, leaked, press};
use super::{App, RenamePlace};
use crate::audio::calibration::{Failure, Phase, Step, Which};
use crate::now_playing_card::fake::FakeControls;
use crate::now_playing_card::{CardArtwork, CardTrack, PlayModes, Repeat};
use crate::ui_model::{
    self, AllPhase, AllProgress, AppState, Backend, CalibrationState, CastAddressCheck,
    CastAddressError, LeftOutRow, MicKind, MixApp, MockBackend, Notice, NoticeMessage, PresetError,
    PresetLeftOff, PresetsPreview, SheetRow, SheetState, SourceName, Speaker, SpeakerKind, Waiting,
    ZoneError,
};
use crate::window_rules::FocusInput;
use crate::{Motion, RowPart};

type Size = (f32, f32);

/// main.slint's preferred size and its smallest (min-width, min-height).
const MAIN: Size = (696.0, 632.0);
const MAIN_SMALLEST: Size = (696.0, 440.0);
/// equalizer.slint's fixed width; the window's height is its content's.
const EQUALIZER_WIDTH: f32 = 520.0;
/// The Mac's smallest main window, title bar included (its photos: 984 px at 2x).
const MAC_SMALLEST: Size = (696.0, 492.0);
const EQUALIZER_SMALLEST: Size = (520.0, 360.0);
/// help.slint's and remote.slint's preferred sizes.
const GUIDE: Size = (640.0, 600.0);
const REMOTE: Size = (460.0, 640.0);
/// setup.slint:267's width; a headless window has no height of its own, and
/// at 565 the first-run rows end at their 14 px padding (setup.slint:132),
/// as measured on a 1000 px photo whose rows stretched.
const SETUP: Size = (480.0, 565.0);
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
const NOT_HEADLESS: [(&str, &str, &str); 5] = [
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
    (
        "B12",
        "ICON-MENUBAR-TRAY",
        "the notification area icon is drawn by Windows, outside any Slint window",
    ),
    (
        "B12",
        "TRAY-MENU",
        "the tray icon's right-click menu is a native Win32 menu, not a Slint window",
    ),
    (
        "B12",
        "ALERT-PRESET-NAME",
        "Windows asks a preset's name in the Presets list's field (MAIN-PRESET-SAVE-FIELD), never in an alert",
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
    Pin,
    Setup,
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
    fn new_zone(&mut self, name: &str) -> Result<(), ZoneError> {
        let mut state = self.0.borrow_mut();
        let mut zones = Zones::from_stored(std::mem::take(&mut state.zones));
        let id = format!("zone:new-{}", zones.list().len());
        zones.new_zone(&id, name, |n| format!("Zone {n}"));
        state.zones = zones.into_list();
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
    fn set_release_when_quiet(&mut self, _on: bool, _minutes: u32) {}
    // As the adapter's first step: what isn't an address is turned down.
    fn check_cast_address(&mut self, text: &str) -> Result<CastAddressCheck, CastAddressError> {
        crate::audio::cast_address::CastAddress::parse(text).ok_or(CastAddressError::Invalid)?;
        Err(CastAddressError::NotFound)
    }
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
    /// A motion frame: photographed as it stands, without settling.
    still: std::cell::Cell<bool>,
    /// A photo drawn outside Slint, taken in place of the window's.
    drawn: RefCell<Option<(Vec<u8>, String)>>,
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
        // The brand accent the app gives each window as its frame comes up.
        app.use_brand_accent(app.main.window());
        let scene = Scene {
            app,
            state,
            media,
            still: std::cell::Cell::new(false),
            drawn: RefCell::new(None),
        };
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
        // The poll that draws the Now Playing line is stopped.
        self.app.follow_ambient(&Rc::downgrade(&self.app));
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

    fn equalizer(&self, size: Option<Size>) -> Result<Win, String> {
        let equalizer = &self.app.equalizer();
        equalizer.show().map_err(|error| error.to_string())?;
        equalizer.invoke_enabled_toggled(true);
        // As the Mac render's Mock.equalizer: on, preamp -2 dB, the Loudness curve.
        equalizer.invoke_preamp_changed(-2.0);
        let loudness = audiosync_core::eq::PRESETS
            .iter()
            .find(|p| p.id == "loudness");
        for (band, gain) in loudness.expect("Loudness").gains_db.iter().enumerate() {
            equalizer.invoke_gain_changed(band as i32, *gain as f32);
        }
        let size = size.unwrap_or((EQUALIZER_WIDTH, equalizer.get_fit_height()));
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

    /// The Mac's light Settings photos read Match System (the Mac was light),
    /// so the light scene's picker shows it while its colours stay light.
    fn light_on_match_system(&self) {
        if self.app.main.get_appearance() == crate::Appearance::Light {
            self.app
                .settings()
                .set_appearance(crate::Appearance::System);
            render(leaked(self.app.settings()));
        }
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
            Shown::Pin => leaked(self.app.pin_dialog()),
            Shown::Setup => leaked(self.app.setup()),
        }
    }

    fn size(&self, window: &slint::Window, (width, height): Size) {
        // The Mac renderer draws at Retina 2x; the testing backend reads no SLINT_SCALE_FACTOR.
        window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
        window.set_size(LogicalSize::new(width, height));
        render(window);
    }

    /// The photo as a PNG and its elements as the JSON screen_compare.py reads.
    fn photo(&self, Win(shown, size): Win) -> Result<(Vec<u8>, String), String> {
        if let Some(drawn) = self.drawn.borrow_mut().take() {
            return Ok(drawn);
        }
        let window = self.window(shown);
        if !self.still.get() {
            settle(window);
            match shown {
                Shown::Flyout => self.fit_flyout(),
                _ => self.size(window, size),
            }
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
            Shown::Pin => ElementQuery::from_root(&self.app.pin_dialog()).find_all(),
            Shown::Setup => ElementQuery::from_root(&self.app.setup()).find_all(),
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
                    "frame": [at.x, at.y, size.width, size.height],
                    "font": null,
                    "fg": fg,
                    "bg": bg,
                })
            })
        })
        .collect();
    serde_json::json!({ "window": [width, height], "elements": listed }).to_string()
}

/// CAL-SHEET-RESULT: the AirPlay delay's summary under four results takes
/// both lines it wraps to, inside the results list, as the Mac's does
/// (CalibrateAllViews.swift:328-333).
#[test]
fn calibrate_alls_airplay_summary_shows_both_lines_in_the_results() {
    let scene = Scene::new(ui_model::Appearance::Light);
    calibrate_all(&scene, |state| {
        state.calibrate_all.sheet = Some(results(SHEET_NAMES));
    })
    .expect("the example speakers");
    settle(scene.app.main.window());
    let list = ElementHandle::find_by_accessible_label(&scene.app.main, "Results")
        .next()
        .expect("the results list");
    let summary = find(&scene.app.main, "AirPlay delay:").expect("the AirPlay summary");
    let line = find(&scene.app.main, "Calibration finished")
        .expect("the run's outcome, one line")
        .size()
        .height;
    let (height, bottom) = (
        summary.size().height,
        summary.absolute_position().y + summary.size().height,
    );
    let end = list.absolute_position().y + list.size().height;
    assert!(
        height >= 1.8 * line,
        "the summary is {height} high, one line {line}"
    );
    assert!(
        bottom <= end,
        "the summary ends at {bottom}, the list at {end}"
    );
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

/// Lets the timers and animations run for about 900 ms of Slint's held clock.
fn settle(window: &slint::Window) {
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        hold_clock();
        slint::platform::update_timers_and_animations();
        render(window);
    }
    // The header's state line fades one step per 16 ms frame; four frames
    // left it at under half its opacity in the 18 Nov photos.
    for _ in 0..20 {
        advance_clock(Duration::from_millis(16));
        slint::platform::update_timers_and_animations();
    }
    render(window);
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
        };
        // As the adapter reports a speaker's own run to Calibrate All.
        state.calibrate_all.other_running = running;
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
        // As adapter.rs sets it: the reference has no Measure Again.
        can_measure_again: state != SheetRowState::Reference,
        state,
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
                    // The adapter shows the Windows volume hint on a switched-on row only.
                    speaker.device_volume = None;
                }
                state.no_speaker_on = true;
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
        // As the Mac render's empty model: AirPlay has looked and found none.
        row("B02", "MAIN-EMPTY", |s| {
            s.edit(|state| {
                state.speakers.clear();
                state.airplay_none_found = true;
            });
            s.main()
        }),
        row("B02", "MAIN-SMALLEST", |s| s.main_at(MAC_SMALLEST)),
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
                kitchen.waiting = Some(Waiting::Reconnecting)
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
            // The list builds only the rows in view, and Living Room's are the first.
            options(s, "Kitchen")?;
            // A click inside the popup lands at its window's coordinates, outside it.
            find(&s.app.main, "Move to Zone")?.invoke_accessible_default_action();
            settle(s.app.main.window());
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
            // The test API reaches no popup's rows, so the keys unfold Several Apps.
            s.app.main.invoke_open_sources();
            render(s.app.main.window());
            press(s.app.main.window(), slint::platform::Key::End);
            press(s.app.main.window(), slint::platform::Key::Return);
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
        row("B04", "LEVEL-POPUP", level_popup),
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
        row("B05", "MAIN-AIRPLAY-DELAY-OPEN", |s| {
            s.app.toggle_sync(super::AIRPLAY_DELAY_FIELD);
            let at = center(&find(&s.app.main, "Kitchen")?);
            scroll(s.app.main.window(), at, -1200.0);
            s.main()
        }),
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
            options(s, "Living Room")?;
            s.main()
        }),
        row("B07", "MAIN-ZONE-RENAME", |s| {
            s.load(MockBackend::zones_preview());
            s.app.start_rename("zone:upstairs", RenamePlace::Zone);
            s.main()
        }),
        row("B07", "MAIN-ZONE-NEW", |s| {
            s.load(MockBackend::zones_preview());
            s.app.main.invoke_new_zone_requested();
            s.main()
        }),
        row("B07", "MAIN-ZONE-PLACEHOLDER", |s| {
            s.load(MockBackend::zones_preview());
            // Office, the empty zone, is built only once the list scrolls to it.
            let at = center(&find(&s.app.main, "Living Room")?);
            for _ in 0..20 {
                if find(&s.app.main, "Office").is_ok_and(|office| center(&office).y < 480.0) {
                    break;
                }
                scroll(s.app.main.window(), at, -60.0);
                render(s.app.main.window());
            }
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
            let typed = "not a valid address!";
            s.app.cast_sheet().set_address(typed.into());
            s.app.cast_sheet().invoke_confirmed(typed.into());
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
        // As the Mac render's empty panel, whose sender is not up yet: it looks.
        row("B08", "TRAY-EMPTY", |s| {
            s.edit(|state| {
                state.speakers.clear();
                state.airplay_searching = true;
            });
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
            let win = s.equalizer(None)?;
            // The Mac's EQ-WINDOW photo shows the Electronic curve.
            let electronic = audiosync_core::eq::PRESETS
                .iter()
                .find(|p| p.id == "electronic")
                .ok_or("no Electronic curve")?;
            for (band, gain) in electronic.gains_db.iter().enumerate() {
                s.app
                    .equalizer()
                    .invoke_gain_changed(band as i32, *gain as f32);
            }
            // The Mac's EQ-WINDOW photo reads Pre +0.5.
            s.app.equalizer().invoke_preamp_changed(0.5);
            // As the Mac render's: just opened, with no band focused.
            s.app.equalizer().invoke_take_focus();
            Ok(win)
        }),
        row("B10", "EQ-CURVE-MENU", |s| {
            s.equalizer(None)?;
            click(&s.app.equalizer(), "Curve")?;
            // The window grows while the menu is open, as a shown one does.
            let size = (EQUALIZER_WIDTH, s.app.equalizer().get_fit_height());
            s.size(s.app.equalizer().window(), size);
            Ok(Win(Shown::Equalizer, size))
        }),
        row("B10", "EQ-SLIDER-DRAG", |s| {
            let win = s.equalizer(None)?;
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
        row("B10", "EQ-SMALLEST", |s| {
            s.equalizer(Some(EQUALIZER_SMALLEST))
        }),
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
            // A headless dialog has no size of its own; the real window's is 360x351.
            Ok(Win(Shown::About, (360.0, 351.0)))
        }),
        row("B11", "MAIN-SETTINGS", |s| {
            // As the Mac's photos: Volume Keys on, the light one on Match System.
            s.app.main.set_volume_keys_available(true);
            s.app.main.set_volume_keys(true);
            let win = s.settings()?;
            s.light_on_match_system();
            Ok(win)
        }),
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
            main.set_volume_keys_available(true);
            main.set_volume_keys(false);
            let win = s.settings()?;
            s.light_on_match_system();
            Ok(win)
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
/// Runs `window`'s animations `by` past where they stand, then holds them.
fn step(window: &slint::Window, by: Duration) {
    advance_clock(by);
    slint::platform::update_timers_and_animations();
    render(window);
}

/// Tier 1 rows that had no scene before.
fn b12() -> Vec<Row> {
    vec![
        row("B12", "MAIN-ROW-VOLUME-DRAG", |s| {
            s.speaker(KITCHEN, |kitchen| kitchen.enabled = true)?;
            let at = center(&find(&s.app.main, "Kitchen volume")?);
            let window = s.app.main.window();
            let button = PointerEventButton::Left;
            window.dispatch_event(WindowEvent::PointerMoved { position: at });
            window.dispatch_event(WindowEvent::PointerPressed {
                position: at,
                button,
            });
            let position = LogicalPosition::new(at.x - 30.0, at.y);
            window.dispatch_event(WindowEvent::PointerMoved { position });
            settle(window);
            s.main()
        }),
        row("B12", "MAIN-ROWS-TIDY", |s| {
            s.speaker(DESK, |desk| desk.enabled = true)?;
            s.main()
        }),
        row("B12", "MAIN-ZONE-HEADER", |s| {
            s.load(MockBackend::zones_preview());
            // As the Mac's real-app photo: a song playing and a Bluetooth speaker lost.
            s.play(track(true, true));
            notice(
                s,
                NoticeMessage::SpeakerLost {
                    speaker: "Headset".into(),
                },
            )
        }),
        row("B12", "MAIN-PRESET-SAVE-FIELD", |s| {
            s.load(MockBackend::presets_preview(PresetsPreview::List));
            s.app.ask_save_preset();
            s.main()
        }),
        row("B12", "ALERT-DOUBLE", |s| {
            notice(
                s,
                NoticeMessage::HeardTwice {
                    source: None,
                    mute: vec!["Headphones".into()],
                    shared: Vec::new(),
                    default_output: Some("Headphones".into()),
                    silent_output: crate::ui_model::SilentOutput::None,
                },
            )
        }),
        row("B12", "ALERT-PAIR-PIN", |s| {
            s.edit(|state| {
                state.pin_prompt = Some(ui_model::PinPrompt {
                    speaker_id: "airplay:living".into(),
                    name: "Living Room".into(),
                    phase: ui_model::PinPhase::Ready,
                    wrong_pin: false,
                });
            });
            // A headless dialog has no size of its own; dialogs.slint:150 fixes the width.
            Ok(Win(
                Shown::Pin,
                (380.0, s.app.pin_dialog().get_fit_height()),
            ))
        }),
        row("B12", "SETUP-WAITING", |s| {
            s.app.show_setup_at_first_install();
            Ok(Win(Shown::Setup, SETUP))
        }),
        row("B12", "MOTION-TRAY-OPEN", |s| {
            s.app.prepare_flyout_open(true);
            s.app.flyout.show().map_err(|error| error.to_string())?;
            s.fit_flyout();
            s.app.flyout.set_revealed(true);
            // The first rows part-way into their 440 ms rise (flyout.slint:807-808).
            step(s.app.flyout.window(), Duration::from_millis(160));
            s.still.set(true);
            Ok(Win(Shown::Flyout, (FLYOUT_WIDTH, 0.0)))
        }),
        row("B12", "MOTION-SETTINGS-OPEN", |s| {
            s.app.main.global::<Motion>().set_on(true);
            let win = s.settings()?;
            let window = leaked(s.app.settings());
            // The first frame's timer reveals it; then part-way into the 560 ms
            // fade (settings.slint:267-270).
            step(window, super::FLYOUT_FIRST_FRAME);
            step(window, Duration::from_millis(60));
            s.still.set(true);
            Ok(win)
        }),
    ]
}

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
        b12(),
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

/// The volume keys' level pop-up (volume_keys/popup.rs), the Mac's
/// LevelPopup, at a level between its MUTED and FULL rows.
fn level_popup(s: &Scene) -> Result<Win, String> {
    #[cfg(not(windows))]
    return Err(format!(
        "{}: only the Windows keyboard hook takes the volume keys",
        s.app.strings().get_volume_popup_name()
    ));
    #[cfg(windows)]
    {
        let strings = s.app.strings();
        let level = 64.0;
        let (width, height, rgba) = crate::volume_keys::popup::picture(
            level,
            super::volume_waves(level),
            s.app.shows_dark(),
            &strings.get_volume_popup_name(),
            &strings.invoke_volume_popup_announcement(level as i32),
            2.0,
        );
        let window = serde_json::json!({ "window": [width / 2, height / 2], "elements": [] });
        *s.drawn.borrow_mut() = Some((png(width, height, &rgba), window.to_string()));
        Ok(Win(Shown::Main, MAIN))
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

/// The Mac photos run with Google Cast on (render.swift:469), so every main
/// window photo offers Add by Address… as theirs do.
#[test]
fn the_photos_offer_add_by_address_as_the_macs_do() {
    let scene = Scene::new(ui_model::Appearance::Light);
    assert!(scene.app.main.get_cast_by_address());
}

/// NOTICE-SOURCE and MAIN-ROW-LOWVOL: a notice and a low-volume hint push
/// the speaker list past the window's bottom, and the list scrolls to show
/// Add by Address… whole, as the Mac's scroll view does.
#[test]
fn a_list_pushed_down_by_a_notice_scrolls_to_add_by_address() {
    let scene = Scene::new(ui_model::Appearance::Light);
    scene
        .speaker(SPEAKERS, |speaker| {
            speaker.enabled = true;
            speaker.device_volume = Some(Hint::Low { percent: 13 });
        })
        .expect("the example speakers");
    notice(
        &scene,
        NoticeMessage::SourceQuit {
            source: music_player(),
        },
    )
    .expect("the notice shows");
    let window = scene.app.main.window();
    settle(window);
    let list = ElementHandle::find_by_element_id(&scene.app.main, "MainWindow::scroll")
        .next()
        .expect("the speaker list");
    let (at, size) = (list.absolute_position(), list.size());
    let position = LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0);
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y: -3000.0,
    });
    settle(window);
    let button = find(&scene.app.main, "Add by Address").expect("Add by Address…");
    let bottom = button.absolute_position().y + button.size().height;
    assert!(
        bottom <= at.y + size.height,
        "Add by Address… ends at {bottom}, the list at {}",
        at.y + size.height
    );
}

/// New Zone's header, with its name field, scrolls into view: on the Mac
/// the focused field in the scroll view is brought into sight
/// (AppDelegate+Zones.swift, `nameInPlace`).
// covers: S245
#[test]
fn a_new_zones_header_scrolls_into_view_for_its_name() {
    let scene = Scene::new(ui_model::Appearance::Light);
    scene.load(MockBackend::zones_preview());
    let window = scene.app.main.window();
    scene.size(window, MAIN);
    render(window);
    let before: Vec<_> = scene
        .app
        .snapshot()
        .zones
        .into_iter()
        .map(|zone| zone.id)
        .collect();
    scene.app.main.invoke_new_zone_requested();
    settle(window);
    let zone = scene
        .app
        .snapshot()
        .zones
        .into_iter()
        .find(|zone| !before.contains(&zone.id))
        .expect("a new zone");
    let options = find(&scene.app.main, &format!("Options for {}", zone.name))
        .expect("the new zone's header is built");
    let bottom = options.absolute_position().y + options.size().height;
    assert!(
        bottom <= MAIN.1,
        "the header ends at {bottom}, below the window"
    );
}

/// MAIN-PLAYING: a window on Slint's software renderer, the fallback where
/// no GPU draws, shows the Now Playing strands under a playing song.
#[test]
fn the_software_renderer_draws_the_strands_of_a_playing_song() {
    let scene = Scene::new(ui_model::Appearance::Dark);
    scene.play(track(true, true));
    settle(scene.app.main.window());
    scene
        .app
        .main
        .window()
        .take_snapshot()
        .expect("the software renderer draws it");
}
