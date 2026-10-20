//! Connects the windows to a [`Backend`]: fills them from its snapshots and
//! turns every user action into a backend call, then refreshes. A timer polls
//! the backend twice a second, so what happens on its own (a device unplugged,
//! a source that quit) shows too.
//!
//! The models are updated in place, row by row, so a slider being dragged or a
//! field being typed in keeps its focus while the rest of the window updates.
//! A list whose length changes is rebuilt; when that removes the control that
//! had the focus in the tray flyout, the flyout takes the focus back so Escape
//! still closes it, and in the main window the focus goes to the next notice,
//! the same control of the row rebuilt, a neighbouring row's options, or the
//! streaming switch.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::time::Duration;

use audiosync_core::eq::{self, EqSettings};
use audiosync_core::policy::device_volume::Hint;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::backdrop::Frame;
use crate::debug_log::DebugLog;
use crate::guide::{self, Guide, GuideStep, SettingsPage};
use crate::ui_model::{
    self, AppState, Backend, MainWindowShows, NoticeMessage, PinPrompt, SourceKind, SpeakerState,
    ViewSettings, WindowFrame,
};
use crate::{
    AboutDialog, AirPlayProblem, Availability, DeviceVolumeHint, EqualizerWindow, GroupItem,
    GroupNameDialog, GuidePage, GuideStepItem, GuideWindow, MainWindow, NoticeItem, NoticeKind,
    Palette, PinDialog, PinPhase, RowPart, RowState, SpeakerItem, SpeakerKind, Strings, TrayFlyout,
};

/// The digits the PIN field keeps of what was typed: four at most.
fn pin_digits(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).take(4).collect()
}

/// What the group name dialog is open for.
#[derive(Clone, Debug, PartialEq)]
enum DialogPurpose {
    NewGroup,
    Rename(String),
    /// Only a message (no name field), e.g. why a group can't be made yet.
    Message,
}

/// UI state the backend doesn't keep: which sync controls are open, what is
/// being typed in their fields, and whether virtual outputs are listed.
#[derive(Default)]
struct UiState {
    sync_open: HashSet<String>,
    /// What is typed in each speaker's delay field, by speaker ID.
    delay_drafts: HashMap<String, TypedDelay>,
    airplay_delay_draft: TypedDelay,
    /// View > Show Virtual Devices; off at launch, like the Mac's default.
    show_virtual: bool,
    listed_virtual: ListedVirtual,
}

/// The virtual outputs the speaker lists have shown this session. With Show
/// Virtual Devices off, a virtual output is listed while it is on; once
/// listed, it stays listed for the rest of the session (until the app
/// restarts or Show Virtual Devices changes), so turning it off doesn't
/// remove its row from under the keyboard focus, and the lists never change
/// shape later on their own.
#[derive(Default)]
struct ListedVirtual {
    ids: HashSet<String>,
}

impl ListedVirtual {
    /// Remembers the virtual outputs listed now.
    fn note(&mut self, speakers: &[ui_model::Speaker], show_virtual: bool) {
        for speaker in speakers {
            if speaker.kind == ui_model::SpeakerKind::Virtual && speaker.is_listed(show_virtual) {
                self.ids.insert(speaker.id.clone());
            }
        }
    }

    /// Whether the speaker lists show the speaker.
    fn lists(&self, speaker: &ui_model::Speaker, show_virtual: bool) -> bool {
        speaker.is_listed(show_virtual) || self.ids.contains(&speaker.id)
    }

    /// Show Virtual Devices changed: only its rule decides again.
    fn forget(&mut self) {
        self.ids.clear();
    }
}

/// What is being typed in a delay field (a speaker's or the AirPlay delay).
/// Typing only keeps the text: applying "2" and "22" on the way to "220"
/// would re-time the output at each of them, and the AirPlay delay's lower
/// bound (2000 ms under the sender's latency) would clamp them. Enter or
/// leaving the field applies it; Escape drops it.
#[derive(Default)]
struct TypedDelay {
    /// The text, and the value the field showed when it was typed: the
    /// field follows a new value (a new sender latency) instead.
    draft: Option<(String, Option<f64>)>,
}

impl TypedDelay {
    fn edit(&mut self, text: &str, shown: Option<f64>) {
        self.draft = Some((text.to_owned(), shown));
    }

    /// The text the field shows instead of `shown`, the applied value.
    fn text(&self, shown: Option<f64>) -> Option<&str> {
        self.draft
            .as_ref()
            .filter(|(_, typed_at)| *typed_at == shown)
            .map(|(text, _)| text.as_str())
    }

    /// Ends the typing: the number to apply, if the typed text is one. The
    /// field then shows the applied value.
    fn commit(&mut self, shown: Option<f64>) -> Option<f64> {
        let number = self.text(shown).and_then(ui_model::parse_ms);
        self.draft = None;
        number
    }

    /// Drops the typed text (Escape, or a step replaced it): the field shows
    /// the applied value again, and leaving it applies nothing.
    fn cancel(&mut self) {
        self.draft = None;
    }
}

/// Whether a delay field holds a draft: `typed`, the text it shows instead of
/// the applied value, isn't `applied` as the field shows it. A draft applies
/// only on Enter or leaving the field, so the field marks it.
fn is_draft(typed: Option<&str>, applied: &str) -> bool {
    typed.is_some_and(|text| text != applied)
}

/// Whether Connected Speakers lists nothing only because its outputs are
/// virtual ones that View > Show Virtual Devices hides: its note says so.
fn only_virtual_outputs_hidden(
    connected: &[&ui_model::Speaker],
    listed: impl Fn(&ui_model::Speaker) -> bool,
) -> bool {
    !connected.is_empty() && connected.iter().all(|s| !listed(s))
}

struct Models {
    sources: Rc<VecModel<SharedString>>,
    this_pc: Rc<VecModel<SpeakerItem>>,
    connected: Rc<VecModel<SpeakerItem>>,
    all_speakers: Rc<VecModel<SpeakerItem>>,
    groups: Rc<VecModel<GroupItem>>,
    notices: Rc<VecModel<NoticeItem>>,
    flyout_notices: Rc<VecModel<NoticeItem>>,
    gains: Rc<VecModel<f32>>,
    presets: Rc<VecModel<SharedString>>,
}

/// How many notices the main window lists, newest first.
const MAIN_NOTICE_LIMIT: usize = 4;
/// The flyout lists only the newest notice and a button for the rest.
const FLYOUT_NOTICE_LIMIT: usize = 1;

/// How long a notice's announcement stays in its window's live region at
/// most (it goes sooner with its notice). Cleared, nothing stray is left for
/// Narrator's scan mode, and the same words can be announced again later.
const ANNOUNCEMENT_CLEARS_AFTER: Duration = Duration::from_secs(5);

/// The most UTF-16 units a tray icon's tooltip holds (128 with the
/// terminating null).
const TOOLTIP_MAX_UNITS: usize = 127;

/// A listed notice, as the announcer compares them.
#[derive(Debug)]
struct Listed {
    id: u64,
    subject: String,
    title: SharedString,
}

/// What the live regions should do after a refresh.
#[derive(Debug, PartialEq)]
enum Announcement {
    /// Say this notice title.
    Say(SharedString),
    /// The notice last said is gone: empty them.
    Clear,
    /// Leave them as they are.
    Keep,
}

/// Picks the notice Narrator is told about: the newest one posted since the
/// last refresh, unless it only repeats a listed notice (same subject and
/// title, as an AirPlay speaker's retries do). The notices there at launch
/// are not news.
#[derive(Debug, Default)]
struct NoticeAnnouncer {
    /// The newest notice id seen so far; `None` until the first refresh.
    seen: Option<u64>,
    /// The subject and title of each notice listed at the last refresh.
    listed: Vec<(String, SharedString)>,
    /// The subject of the notice last said, until it leaves the list.
    said: Option<String>,
}

impl NoticeAnnouncer {
    /// What to announce for `notices`. Ids only grow, so a notice with a
    /// higher id than any seen was posted since the last refresh.
    fn next(&mut self, notices: &[Listed]) -> Announcement {
        let repeats = |n: &Listed| {
            self.listed
                .iter()
                .any(|(subject, title)| *subject == n.subject && *title == n.title)
        };
        let fresh = self.seen.and_then(|seen| {
            notices
                .iter()
                .filter(|n| n.id > seen && !repeats(n))
                .max_by_key(|n| n.id)
        });
        let top = notices.iter().map(|n| n.id).max().unwrap_or(0);
        self.seen = Some(self.seen.map_or(top, |seen| seen.max(top)));
        self.listed = notices
            .iter()
            .map(|n| (n.subject.clone(), n.title.clone()))
            .collect();
        if let Some(notice) = fresh {
            self.said = Some(notice.subject.clone());
            return Announcement::Say(notice.title.clone());
        }
        match &self.said {
            Some(said) if !notices.iter().any(|n| n.subject == *said) => {
                self.said = None;
                Announcement::Clear
            }
            _ => Announcement::Keep,
        }
    }
}

/// How often the backend is polled for what happened on its own.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often, 20 ms apart, the saved main window size waits for the native
/// window before it gives up (about two seconds).
const NATIVE_WINDOW_TRIES: u8 = 100;

/// Roughly what the main window's outer frame adds to its inner size, in
/// logical pixels: the borders, the title bar and the menu bar. Used to fit
/// a saved size to a monitor, where a little extra room does no harm.
const MAIN_WINDOW_CHROME: (f64, f64) = (16.0, 64.0);

/// The parts of the tray flyout that a refresh can remove, with the controls
/// in them: the listed notice, the button for more notices, the group rows
/// and the speaker rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FlyoutParts {
    notices: bool,
    more_notices: bool,
    groups: bool,
    speakers: bool,
}

impl FlyoutParts {
    /// Whether a part is set in both.
    fn overlaps(self, other: FlyoutParts) -> bool {
        (self.notices && other.notices)
            || (self.more_notices && other.more_notices)
            || (self.groups && other.groups)
            || (self.speakers && other.speakers)
    }
}

/// What the tray flyout shows: how many notices there are and how many of
/// them it lists, and how many groups and speakers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FlyoutCounts {
    notices: usize,
    listed_notices: usize,
    groups: usize,
    speakers: usize,
}

impl FlyoutCounts {
    /// Whether the flyout shows the button for the notices it doesn't list.
    fn shows_more_notices(self) -> bool {
        self.notices > self.listed_notices
    }

    /// The parts that a refresh to `next` removes. A list whose length
    /// changes is rebuilt ([`sync_model`]), so all its rows go, even the
    /// ones whose items come back.
    fn removed_by(self, next: FlyoutCounts) -> FlyoutParts {
        FlyoutParts {
            notices: self.listed_notices != next.listed_notices,
            more_notices: self.shows_more_notices() && !next.shows_more_notices(),
            groups: self.groups != next.groups,
            speakers: self.speakers != next.speakers,
        }
    }
}

/// What has the keyboard focus in the main window's lists, as their rows
/// report it. A row that a refresh removes never reports losing it.
#[derive(Debug, Default)]
struct MainFocus {
    /// A notice's button, by notice id.
    notice: Option<i32>,
    /// The notices' Dismiss All button.
    dismiss_all: bool,
    /// A speaker row's control: the speaker id and the row's part.
    speaker: Option<(SharedString, RowPart)>,
    /// A group row's control: the group id and the row's part.
    group: Option<(SharedString, RowPart)>,
}

/// Notes that the notice `id` gained or lost the focus.
fn note_focus<T: PartialEq>(slot: &mut Option<T>, id: T, focused: bool) {
    if focused {
        *slot = Some(id);
    } else if slot.as_ref() == Some(&id) {
        // Another row may have reported gaining it first.
        *slot = None;
    }
}

/// Notes that the focus moved to `part` of the row `id`, or left the row
/// (`RowPart::None`).
fn note_row_focus(slot: &mut Option<(SharedString, RowPart)>, id: SharedString, part: RowPart) {
    if part != RowPart::None {
        *slot = Some((id, part));
    } else if slot.as_ref().is_some_and(|(held, _)| *held == id) {
        // Another row may have reported gaining it first.
        *slot = None;
    }
}

/// Where the main window's keyboard focus goes after a refresh rebuilt
/// the list that had it.
#[derive(Clone, Debug, PartialEq)]
enum MainFocusTarget {
    /// The notice at this index.
    Notice(usize),
    /// A speaker or group row, by id, on this part (the row falls back to a
    /// harmless part when it hasn't that one).
    Speaker(SharedString, RowPart),
    Group(SharedString, RowPart),
    StreamingSwitch,
}

/// The row next to `old[at]`, which `new` no longer lists: the next row
/// that `new` still lists, else the previous one, else the one in its
/// place; none when `new` is empty.
fn neighbour<T>(
    old: &[T],
    new: &[T],
    at: usize,
    id: impl Fn(&T) -> &SharedString,
) -> Option<SharedString> {
    let kept = |row: &&T| new.iter().any(|n| id(n) == id(row));
    old[at + 1..]
        .iter()
        .find(kept)
        .or_else(|| old[..at].iter().rev().find(kept))
        .or_else(|| new.get(at.min(new.len().saturating_sub(1))))
        .map(|row| id(row).clone())
}

/// The options of the first row `new` lists, This PC's, then Connected
/// Speakers', then the groups' (a row's list is empty, so its own list adds
/// nothing); the streaming switch when every list is empty.
fn first_row(new: &MainLists) -> MainFocusTarget {
    let speaker = new.this_pc.first().or_else(|| new.connected.first());
    match (speaker, new.groups.first()) {
        (Some(speaker), _) => MainFocusTarget::Speaker(speaker.id.clone(), RowPart::Options),
        (None, Some(group)) => MainFocusTarget::Group(group.id.clone(), RowPart::Options),
        (None, None) => MainFocusTarget::StreamingSwitch,
    }
}

impl MainFocus {
    /// Where the focus goes when the lists change from `old` to `new`, if a
    /// rebuilt list (its length changed, [`sync_model`]) had it.
    fn target(&self, old: &MainLists, new: &MainLists) -> Option<MainFocusTarget> {
        let notice_ids = |lists: &MainLists| lists.notices.iter().map(|n| n.id).collect::<Vec<_>>();
        if (self.notice.is_some() || self.dismiss_all) && old.notices.len() != new.notices.len() {
            let (old_ids, new_ids) = (notice_ids(old), notice_ids(new));
            let index = match self.notice {
                // The same notice if it stayed, else the one in its place.
                Some(id) => new_ids
                    .iter()
                    .position(|&n| n == id)
                    .or_else(|| old_ids.iter().position(|&n| n == id))
                    .unwrap_or(0),
                // Dismiss All sits under the last notice.
                None => old_ids.len(),
            };
            return Some(match new_ids.len() {
                0 => MainFocusTarget::StreamingSwitch,
                len => MainFocusTarget::Notice(index.min(len - 1)),
            });
        }
        // A row that went hands the focus to a neighbour's options, where
        // Space or Enter opens a menu rather than toggling a speaker or
        // streaming; once its list is empty, to the first row of the other
        // lists ([`first_row`]), and the streaming switch takes it only when
        // no row is left.
        if let Some((id, part)) = &self.speaker {
            for (old_rows, new_rows) in [
                (&old.this_pc, &new.this_pc),
                (&old.connected, &new.connected),
            ] {
                let Some(at) = old_rows.iter().position(|s| s.id == *id) else {
                    continue;
                };
                if old_rows.len() == new_rows.len() {
                    break;
                }
                let listed = new
                    .this_pc
                    .iter()
                    .chain(&new.connected)
                    .any(|s| s.id == *id);
                return Some(if listed {
                    MainFocusTarget::Speaker(id.clone(), *part)
                } else {
                    neighbour(old_rows, new_rows, at, |s| &s.id).map_or_else(
                        || first_row(new),
                        |next| MainFocusTarget::Speaker(next, RowPart::Options),
                    )
                });
            }
        }
        if let Some((id, part)) = &self.group
            && old.groups.len() != new.groups.len()
            && let Some(at) = old.groups.iter().position(|g| g.id == *id)
        {
            return Some(if new.groups.iter().any(|g| g.id == *id) {
                MainFocusTarget::Group(id.clone(), *part)
            } else {
                neighbour(&old.groups, &new.groups, at, |g| &g.id).map_or_else(
                    || first_row(new),
                    |next| MainFocusTarget::Group(next, RowPart::Options),
                )
            });
        }
        None
    }
}

/// The main window's lists that a refresh can rebuild.
#[derive(Debug, Default)]
struct MainLists {
    notices: Vec<NoticeItem>,
    this_pc: Vec<SpeakerItem>,
    connected: Vec<SpeakerItem>,
    groups: Vec<GroupItem>,
}

/// Runs after every refresh with the new state.
type RefreshHook = Box<dyn Fn(&AppState)>;

/// The application's windows and the backend behind them.
pub struct App {
    backend: RefCell<Box<dyn Backend>>,
    pub main: MainWindow,
    pub flyout: TrayFlyout,
    pub equalizer: EqualizerWindow,
    group_dialog: GroupNameDialog,
    /// The PIN prompt of an Apple TV pairing.
    pin_dialog: PinDialog,
    /// The speaker the PIN dialog is open for, while it is.
    pin_for: RefCell<Option<String>>,
    /// The PIN dialog shows a made-up pairing (the PIN preview), which no
    /// refresh closes.
    pin_preview: Cell<bool>,
    about: AboutDialog,
    guide: GuideWindow,
    /// The paragraphs the guide shows.
    guide_steps: Rc<VecModel<GuideStepItem>>,
    /// The guide is open for the double-audio notice and follows it (not
    /// for a made-up preview notice).
    guide_follows_notice: Cell<bool>,
    log: DebugLog,
    ui: RefCell<UiState>,
    /// What has the keyboard focus in the main window's lists.
    main_focus: RefCell<MainFocus>,
    dialog_purpose: RefCell<DialogPurpose>,
    models: Models,
    /// With a tray icon, closing the main window keeps the app running there.
    tray_available: Cell<bool>,
    /// Dark (true) or light for every window instead of the Windows setting:
    /// the theme preview, `None` normally.
    theme_override: Cell<Option<bool>>,
    /// Whether each window got its Windows 11 frame, as last logged.
    frames: RefCell<HashMap<&'static str, bool>>,
    /// Called after every refresh, e.g. to update the tray icon and tooltip.
    on_refresh: RefCell<Option<RefreshHook>>,
    /// Which new notice Narrator is told about.
    notice_announcer: RefCell<NoticeAnnouncer>,
    /// Clears the announcement a while after it was made.
    announcement_timer: Timer,
    /// Polls the backend; it stops when the app is dropped.
    poll_timer: Timer,
}

/// Runs `action` on the app if it still exists.
fn with_app(app: &Weak<App>, action: impl FnOnce(&App)) {
    if let Some(app) = app.upgrade() {
        action(&app);
    }
}

/// Sets the main window's size as soon as its native window exists, checking
/// every 20 ms up to `tries` times, then maximizes it when `maximized` (a
/// size set on a maximized window would restore it to normal).
fn set_size_once_native(
    main: slint::Weak<MainWindow>,
    size: slint::LogicalSize,
    maximized: bool,
    tries: u8,
) {
    use slint::winit_030::WinitWindowAccessor;

    Timer::single_shot(Duration::from_millis(20), move || {
        let Some(window_owner) = main.upgrade() else {
            return;
        };
        let window = window_owner.window();
        if window.with_winit_window(|_| ()).is_some() {
            window.set_size(size);
            if maximized {
                window.set_maximized(true);
            }
        } else if tries > 1 {
            set_size_once_native(main, size, maximized, tries - 1);
        }
    });
}

/// The saved main window `frame` as it opens on the monitor with
/// `work_area` (physical pixels) and `scale`: its inner size (logical
/// pixels) and outer position (physical pixels), fitted inside the work
/// area. `keep_place` when the saved place is on that monitor; else the
/// window opens centered on it.
fn fitted_main_frame(
    frame: WindowFrame,
    work_area: crate::placement::Bounds,
    scale: f64,
    keep_place: bool,
) -> WindowFrame {
    let outer = crate::placement::Bounds {
        x: f64::from(frame.x),
        y: f64::from(frame.y),
        width: (frame.width + MAIN_WINDOW_CHROME.0) * scale,
        height: (frame.height + MAIN_WINDOW_CHROME.1) * scale,
    };
    let fitted = crate::placement::fit_window(outer, work_area, keep_place);
    if fitted == outer {
        return frame;
    }
    let (min_width, min_height) = crate::settings::MIN_WINDOW;
    let inner = |outer: f64, chrome: f64, saved: f64, min: f64| {
        (outer / scale - chrome).round().clamp(min, saved.max(min))
    };
    WindowFrame {
        x: fitted.x.round() as i32,
        y: fitted.y.round() as i32,
        width: inner(fitted.width, MAIN_WINDOW_CHROME.0, frame.width, min_width),
        height: inner(
            fitted.height,
            MAIN_WINDOW_CHROME.1,
            frame.height,
            min_height,
        ),
        maximized: frame.maximized,
    }
}

impl App {
    /// The windows over `backend`; what they do themselves goes to `log`.
    pub fn new(backend: Box<dyn Backend>, log: DebugLog) -> Result<Rc<App>, slint::PlatformError> {
        let models = Models {
            sources: Rc::new(VecModel::default()),
            this_pc: Rc::new(VecModel::default()),
            connected: Rc::new(VecModel::default()),
            all_speakers: Rc::new(VecModel::default()),
            groups: Rc::new(VecModel::default()),
            notices: Rc::new(VecModel::default()),
            flyout_notices: Rc::new(VecModel::default()),
            gains: Rc::new(VecModel::default()),
            presets: Rc::new(VecModel::default()),
        };
        let app = Rc::new(App {
            backend: RefCell::new(backend),
            main: MainWindow::new()?,
            flyout: TrayFlyout::new()?,
            equalizer: EqualizerWindow::new()?,
            group_dialog: GroupNameDialog::new()?,
            pin_dialog: PinDialog::new()?,
            pin_for: RefCell::new(None),
            pin_preview: Cell::new(false),
            about: AboutDialog::new()?,
            guide: GuideWindow::new()?,
            guide_steps: Rc::new(VecModel::default()),
            guide_follows_notice: Cell::new(false),
            log,
            ui: RefCell::default(),
            main_focus: RefCell::default(),
            dialog_purpose: RefCell::new(DialogPurpose::Message),
            models,
            tray_available: Cell::new(false),
            theme_override: Cell::new(None),
            frames: RefCell::default(),
            on_refresh: RefCell::new(None),
            notice_announcer: RefCell::default(),
            announcement_timer: Timer::default(),
            poll_timer: Timer::default(),
        });
        app.attach_models();
        app.connect_main();
        app.connect_flyout();
        app.connect_equalizer();
        app.connect_dialogs();
        app.connect_guide();
        app.restore_view();
        app.refresh();
        let weak = Rc::downgrade(&app);
        app.poll_timer
            .start(TimerMode::Repeated, POLL_INTERVAL, move || {
                with_app(&weak, App::poll)
            });
        Ok(app)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_tray_available(&self, available: bool) {
        self.tray_available.set(available);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_on_refresh(&self, callback: impl Fn(&AppState) + 'static) {
        *self.on_refresh.borrow_mut() = Some(Box::new(callback));
    }

    pub fn strings(&self) -> Strings<'_> {
        self.main.global::<Strings>()
    }

    pub fn snapshot(&self) -> AppState {
        self.backend.borrow().snapshot()
    }

    /// Changes the backend, then shows the result everywhere.
    pub fn update(&self, change: impl FnOnce(&mut dyn Backend)) {
        change(self.backend.borrow_mut().as_mut());
        self.refresh();
    }

    /// Lets the backend take in what happened on its own, and shows it.
    fn poll(&self) {
        let changed = self.backend.borrow_mut().poll();
        if changed {
            self.refresh();
        }
    }

    /// Shows the main window and brings it to the front.
    pub fn show_main(&self) {
        if let Err(error) = self.main.show() {
            self.show_window_error(&error);
            return;
        }
        self.apply_frame(self.main.window(), "main window", Frame::Titled);
        crate::platform::bring_to_front(self.main.window());
    }

    /// Shows every window light or dark, whatever Windows uses, to check both
    /// appearances without changing the Windows setting (the theme preview).
    pub fn preview_theme(&self, dark: bool) {
        use slint::language::ColorScheme;
        let scheme = if dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        self.main.global::<Palette>().set_color_scheme(scheme);
        self.flyout.global::<Palette>().set_color_scheme(scheme);
        self.equalizer.global::<Palette>().set_color_scheme(scheme);
        self.group_dialog
            .global::<Palette>()
            .set_color_scheme(scheme);
        self.pin_dialog.global::<Palette>().set_color_scheme(scheme);
        self.about.global::<Palette>().set_color_scheme(scheme);
        self.guide.global::<Palette>().set_color_scheme(scheme);
        self.theme_override.set(Some(dark));
    }

    /// Gives a window that is showing its Windows 11 frame (`backdrop`), and
    /// logs whether DWM took it. `true` when it did.
    pub fn apply_frame(&self, window: &slint::Window, name: &'static str, frame: Frame) -> bool {
        if let Some(dark) = self.theme_override.get() {
            use slint::winit_030::WinitWindowAccessor;
            use slint::winit_030::winit::window::Theme;
            let theme = if dark { Theme::Dark } else { Theme::Light };
            window.with_winit_window(|window| window.set_theme(Some(theme)));
        }
        // Before the event loop runs there is no native window yet; the
        // window's first focus (see `follow_frame`) tries again.
        let Some(applied) = crate::backdrop::apply(window, frame) else {
            return false;
        };
        if self.frames.borrow_mut().insert(name, applied) != Some(applied) {
            self.log.log(|| {
                let what = match frame {
                    Frame::Titled => "Mica title bar",
                    Frame::Flyout => "rounded corners",
                };
                let result = if applied { "on" } else { "not available" };
                format!("{name}: {what} {result}")
            });
        }
        applied
    }

    /// Gives a titled window (every window but the flyout) its frame each
    /// time it gains the focus, which covers a native window created after
    /// `show` returned.
    fn follow_frame(self: &Rc<Self>, window: fn(&App) -> &slint::Window, name: &'static str) {
        use slint::winit_030::winit::event::WindowEvent;
        use slint::winit_030::{EventResult, WinitWindowAccessor};

        let weak = Rc::downgrade(self);
        window(self).on_winit_window_event(move |_, event| {
            if let WindowEvent::Focused(true) = event {
                with_app(&weak, |app| {
                    app.apply_frame(window(app), name, Frame::Titled);
                });
            }
            EventResult::Propagate
        });
    }

    fn attach_models(&self) {
        let m = &self.models;
        self.main.set_sources(ModelRc::from(m.sources.clone()));
        self.main.set_this_pc(ModelRc::from(m.this_pc.clone()));
        self.main.set_connected(ModelRc::from(m.connected.clone()));
        self.main.set_groups(ModelRc::from(m.groups.clone()));
        self.main.set_notices(ModelRc::from(m.notices.clone()));
        self.flyout.set_sources(ModelRc::from(m.sources.clone()));
        self.flyout
            .set_speakers(ModelRc::from(m.all_speakers.clone()));
        self.flyout.set_groups(ModelRc::from(m.groups.clone()));
        self.flyout
            .set_notices(ModelRc::from(m.flyout_notices.clone()));
        self.equalizer.set_gains(ModelRc::from(m.gains.clone()));
        self.equalizer
            .set_preset_names(ModelRc::from(m.presets.clone()));
        let band_hz: Vec<f32> = eq::BAND_CENTERS_HZ.iter().map(|&hz| hz as f32).collect();
        self.equalizer
            .set_band_hz(ModelRc::new(VecModel::from(band_hz)));
        self.equalizer.set_max_gain(eq::MAX_GAIN_DB as f32);
        self.guide
            .set_steps(ModelRc::from(self.guide_steps.clone()));
        self.about.set_version(env!("CARGO_PKG_VERSION").into());
        self.about.set_core_version(audiosync_core::VERSION.into());
    }

    /// Reads a snapshot and shows it in every window.
    pub fn refresh(&self) {
        let state = self.snapshot();
        let strings = self.strings();
        {
            let mut ui = self.ui.borrow_mut();
            let show_virtual = ui.show_virtual;
            ui.listed_virtual.note(&state.speakers, show_virtual);
            // A speaker that went takes what was typed for it along: back,
            // it shows its saved delay.
            ui.delay_drafts
                .retain(|id, _| state.speakers.iter().any(|s| s.id == *id));
        }
        let ui = self.ui.borrow();
        let item = |speaker: &ui_model::Speaker, show_kind: bool| {
            let draft = ui
                .delay_drafts
                .get(&speaker.id)
                .and_then(|typed| typed.text(speaker.delay_ms));
            speaker_item(
                &state,
                speaker,
                show_kind,
                ui.sync_open.contains(&speaker.id),
                draft,
            )
        };
        let (this_pc, connected) = state.layout();
        let lists = |s: &ui_model::Speaker| ui.listed_virtual.lists(s, ui.show_virtual);
        self.main
            .set_connected_virtual_hidden(only_virtual_outputs_hidden(&connected, lists));
        self.main.set_airplay_none_found(state.airplay_none_found);
        let listed = |s: &&ui_model::Speaker| lists(s);
        let this_pc: Vec<SpeakerItem> = this_pc
            .into_iter()
            .filter(listed)
            .map(|s| item(s, false))
            .collect();
        let connected: Vec<SpeakerItem> = connected
            .into_iter()
            .filter(listed)
            .map(|s| item(s, true))
            .collect();
        let all: Vec<SpeakerItem> = this_pc.iter().chain(&connected).cloned().collect();
        let groups: Vec<GroupItem> = state.groups.iter().map(|g| group_item(&state, g)).collect();
        let notices: Vec<NoticeItem> = state
            .notices
            .iter()
            .map(|n| notice_item(&strings, n))
            .collect();
        let listed: Vec<Listed> = state
            .notices
            .iter()
            .zip(&notices)
            .map(|(notice, item)| Listed {
                id: notice.id,
                subject: notice.subject.clone(),
                title: item.title.clone(),
            })
            .collect();
        let announcement = self.notice_announcer.borrow_mut().next(&listed);
        match announcement {
            Announcement::Say(title) => self.announce(strings.invoke_notice_announcement(title)),
            Announcement::Clear => self.clear_announcement(),
            Announcement::Keep => {}
        }
        let sources: Vec<SharedString> = state
            .sources
            .iter()
            .map(|s| match s.kind {
                // The backend's name for it is English; the UI's is translated.
                SourceKind::System => strings.get_source_system(),
                SourceKind::App | SourceKind::Input => s.name.as_str().into(),
            })
            .collect();
        let source_index = state
            .sources
            .iter()
            .position(|s| s.id == state.source_id)
            .map_or(-1, |i| i as i32);
        let status = strings.invoke_status(state.running, count(state.active_speakers()));
        let flyout_notices = newest_notices(&notices, FLYOUT_NOTICE_LIMIT);

        let m = &self.models;
        let flyout_shown = FlyoutCounts {
            notices: usize::try_from(self.flyout.get_notice_count()).unwrap_or(0),
            listed_notices: m.flyout_notices.row_count(),
            groups: m.groups.row_count(),
            speakers: m.all_speakers.row_count(),
        };
        let flyout_removed = flyout_shown.removed_by(FlyoutCounts {
            notices: notices.len(),
            listed_notices: flyout_notices.len(),
            groups: groups.len(),
            speakers: all.len(),
        });
        let main_lists = MainLists {
            notices: newest_notices(&notices, MAIN_NOTICE_LIMIT),
            this_pc,
            connected,
            groups,
        };
        let main_focus = self.main_focus_target(&main_lists);
        let MainLists {
            notices: main_notices,
            this_pc,
            connected,
            groups,
        } = main_lists;
        sync_model(&m.sources, sources);
        sync_model(&m.this_pc, this_pc);
        sync_model(&m.connected, connected);
        sync_model(&m.all_speakers, all);
        sync_model(&m.groups, groups);
        sync_model(&m.notices, main_notices);
        sync_model(&m.flyout_notices, flyout_notices);
        self.flyout.set_notice_count(count(notices.len()));
        self.restore_flyout_focus(flyout_removed);
        if main_focus == Some(MainFocusTarget::StreamingSwitch) {
            self.main.invoke_focus_streaming_switch();
        }
        self.refresh_guide(&state);
        self.show_pin_prompt(state.pin_prompt.as_ref());

        for window in [&self.main as &dyn WindowState, &self.flyout] {
            window.show_state(&state, &status, source_index);
        }
        self.main.set_equalizer_on(state.equalizer.enabled);
        self.main.set_show_virtual_devices(ui.show_virtual);
        self.main
            .set_airplay_delay_shown(state.airplay_delay_ms.is_some());
        let airplay_delay = format_ms(state.airplay_delay_ms.unwrap_or(0.0));
        let typed = ui.airplay_delay_draft.text(state.airplay_delay_ms);
        self.main
            .set_airplay_delay_pending(is_draft(typed, &airplay_delay));
        self.main
            .set_airplay_delay_text(typed.map_or(airplay_delay, str::to_owned).into());

        self.show_equalizer_state(&state.equalizer);
        drop(ui);
        if let Some(callback) = self.on_refresh.borrow().as_ref() {
            callback(&state);
        }
    }

    /// Has Narrator say `text` without moving the focus, through the live
    /// region of the window it can hear: the flyout while it is open (it
    /// has the focus then), else the main window. Never both, so it is said
    /// once. The live region empties again after a few seconds.
    fn announce(&self, text: SharedString) {
        if self.flyout.window().is_visible() {
            self.main.set_notice_announcement(SharedString::new());
            self.flyout.set_notice_announcement(text);
        } else {
            self.flyout.set_notice_announcement(SharedString::new());
            self.main.set_notice_announcement(text);
        }
        let (main, flyout) = (self.main.as_weak(), self.flyout.as_weak());
        self.announcement_timer.start(
            TimerMode::SingleShot,
            ANNOUNCEMENT_CLEARS_AFTER,
            move || clear_announcement(&main, &flyout),
        );
    }

    /// Empties both live regions now.
    fn clear_announcement(&self) {
        self.announcement_timer.stop();
        clear_announcement(&self.main.as_weak(), &self.flyout.as_weak());
    }

    /// Gives the keyboard focus back to the open flyout when a refresh
    /// removed the control that had it (a notice dismissed with the
    /// keyboard, a speaker unplugged): Slint then leaves nothing focused,
    /// and Escape would not close the flyout until Tab focused something.
    fn restore_flyout_focus(&self, removed: FlyoutParts) {
        let flyout = &self.flyout;
        let focused = FlyoutParts {
            notices: flyout.get_notice_focused(),
            more_notices: flyout.get_more_notices_focused(),
            groups: flyout.get_group_focused(),
            speakers: flyout.get_speaker_focused(),
        };
        if focused.overlaps(removed) && flyout.window().is_visible() {
            flyout.invoke_take_focus();
        }
    }

    /// Where the main window's keyboard focus goes when this refresh, to
    /// `new`, rebuilds the list that has it: Slint then leaves nothing
    /// focused. Asks the rebuilt row or notice to take it as it is built
    /// (so before the models change); the streaming switch takes it after.
    fn main_focus_target(&self, new: &MainLists) -> Option<MainFocusTarget> {
        let m = &self.models;
        let old = MainLists {
            notices: m.notices.iter().collect(),
            this_pc: m.this_pc.iter().collect(),
            connected: m.connected.iter().collect(),
            groups: m.groups.iter().collect(),
        };
        let mut focus = self.main_focus.borrow_mut();
        let target = focus.target(&old, new)?;
        let main = &self.main;
        if !main.window().is_visible() {
            *focus = MainFocus::default();
            return None;
        }
        // What has it next. The row that takes it may report that before
        // its own focus tracking starts, and so never report it.
        *focus = match &target {
            MainFocusTarget::Notice(index) => MainFocus {
                notice: new.notices.get(*index).map(|n| n.id),
                ..MainFocus::default()
            },
            MainFocusTarget::Speaker(id, part) => MainFocus {
                speaker: Some((id.clone(), *part)),
                ..MainFocus::default()
            },
            MainFocusTarget::Group(id, part) => MainFocus {
                group: Some((id.clone(), *part)),
                ..MainFocus::default()
            },
            MainFocusTarget::StreamingSwitch => MainFocus::default(),
        };
        drop(focus);
        main.set_focus_notice_index(-1);
        main.set_focus_speaker_id(SharedString::new());
        main.set_focus_group_id(SharedString::new());
        match &target {
            MainFocusTarget::Notice(index) => main.set_focus_notice_index(count(*index)),
            MainFocusTarget::Speaker(id, part) => {
                main.set_focus_speaker_part(*part);
                main.set_focus_speaker_id(id.clone());
            }
            MainFocusTarget::Group(id, part) => {
                main.set_focus_group_part(*part);
                main.set_focus_group_id(id.clone());
            }
            MainFocusTarget::StreamingSwitch => {}
        }
        Some(target)
    }

    fn show_equalizer_state(&self, settings: &EqSettings) {
        let strings = self.strings();
        let mut presets: Vec<SharedString> = eq::PRESETS
            .iter()
            .map(|p| strings.invoke_equalizer_preset(p.id.into()))
            .collect();
        let preset_index = match ui_model::matching_preset_index(settings) {
            Some(index) => index,
            None => {
                presets.push(strings.get_equalizer_custom());
                presets.len() - 1
            }
        };
        sync_model(&self.models.presets, presets);
        sync_model(
            &self.models.gains,
            settings.gains_db.iter().map(|&g| g as f32).collect(),
        );
        self.equalizer.set_enabled(settings.enabled);
        self.equalizer.set_preamp(settings.preamp_db as f32);
        self.equalizer.set_preset_index(preset_index as i32);
    }

    fn connect_main(self: &Rc<Self>) {
        self.follow_frame(|app| app.main.window(), "main window");
        let main = &self.main;
        let weak = Rc::downgrade(self);

        let w = weak.clone();
        main.on_power_toggled(move |on| with_app(&w, |app| app.update(|b| b.set_running(on))));
        let w = weak.clone();
        main.on_source_selected(move |index| with_app(&w, |app| app.select_source(index)));
        let w = weak.clone();
        main.on_master_volume_changed(move |volume| {
            with_app(&w, |app| app.update(|b| b.set_master_volume(volume.into())))
        });
        let w = weak.clone();
        main.on_equalizer_requested(move || with_app(&w, App::show_equalizer));
        let w = weak.clone();
        main.on_speaker_toggled(move |id| with_app(&w, |app| app.toggle_speaker(&id)));
        let w = weak.clone();
        main.on_speaker_volume_changed(move |id, volume| {
            with_app(&w, |app| {
                app.update(|b| b.set_speaker_volume(&id, volume.into()))
            })
        });
        let w = weak.clone();
        main.on_sync_toggled(move |id| with_app(&w, |app| app.toggle_sync(&id)));
        let w = weak.clone();
        main.on_delay_stepped(move |id, delta| with_app(&w, |app| app.step_delay(&id, delta)));
        let w = weak.clone();
        main.on_delay_edited(move |id, text| with_app(&w, |app| app.edit_delay(&id, &text)));
        let w = weak.clone();
        main.on_delay_committed(move |id| with_app(&w, |app| app.enter_delay(&id)));
        let w = weak.clone();
        main.on_delay_cancelled(move |_| with_app(&w, |app| app.end_delay_typing(false)));
        let w = weak.clone();
        main.on_airplay_delay_stepped(move |delta| {
            with_app(&w, |app| app.step_airplay_delay(delta))
        });
        let w = weak.clone();
        main.on_airplay_delay_edited(move |text| with_app(&w, |app| app.edit_airplay_delay(&text)));
        let w = weak.clone();
        main.on_airplay_delay_committed(move || with_app(&w, App::commit_airplay_delay));
        let w = weak.clone();
        main.on_airplay_delay_cancelled(move || with_app(&w, |app| app.end_delay_typing(false)));
        let w = weak.clone();
        main.on_blank_clicked(move || with_app(&w, |app| app.end_delay_typing(true)));
        let w = weak.clone();
        main.on_auto_connect_toggled(move |id, on| {
            with_app(&w, |app| app.update(|b| b.set_auto_connect(&id, on)))
        });
        let w = weak.clone();
        main.on_forget_pairing_requested(move |id| {
            with_app(&w, |app| app.update(|b| b.forget_pairing(&id)))
        });
        let w = weak.clone();
        main.on_device_volume_fix_requested(move |id| {
            with_app(&w, |app| app.update(|b| b.fix_device_volume(&id)))
        });
        let w = weak.clone();
        main.on_group_toggled(move |id| with_app(&w, |app| app.update(|b| b.toggle_group(&id))));
        let w = weak.clone();
        main.on_group_rename_requested(move |id| with_app(&w, |app| app.ask_rename(&id)));
        let w = weak.clone();
        main.on_group_deleted(move |id| with_app(&w, |app| app.update(|b| b.delete_group(&id))));
        let w = weak.clone();
        main.on_new_group_requested(move || with_app(&w, App::ask_new_group));
        let w = weak.clone();
        main.on_refresh_devices(move || with_app(&w, |app| app.update(|b| b.refresh_devices())));
        let w = weak.clone();
        main.on_show_virtual_devices_toggled(move |on| {
            with_app(&w, |app| app.show_virtual_devices(on))
        });
        let w = weak.clone();
        main.on_notice_dismissed(move |id| {
            with_app(&w, |app| app.update(|b| b.dismiss_notice(id as u64)))
        });
        let w = weak.clone();
        main.on_notices_dismissed_all(move || {
            with_app(&w, |app| app.update(|b| b.dismiss_all_notices()))
        });
        let w = weak.clone();
        main.on_notice_guide_requested(move |id| with_app(&w, |app| app.show_guide(id)));
        let w = weak.clone();
        main.on_notice_retry_requested(move |_| {
            with_app(&w, |app| app.update(|b| b.retry_airplay()))
        });
        let w = weak.clone();
        main.on_notice_focus_changed(move |id, focused| {
            with_app(&w, |app| {
                note_focus(&mut app.main_focus.borrow_mut().notice, id, focused)
            })
        });
        let w = weak.clone();
        main.on_dismiss_all_focus_changed(move |focused| {
            with_app(&w, |app| app.main_focus.borrow_mut().dismiss_all = focused)
        });
        let w = weak.clone();
        main.on_speaker_focus_changed(move |id, part| {
            with_app(&w, |app| {
                note_row_focus(&mut app.main_focus.borrow_mut().speaker, id, part)
            })
        });
        let w = weak.clone();
        main.on_group_focus_changed(move |id, part| {
            with_app(&w, |app| {
                note_row_focus(&mut app.main_focus.borrow_mut().group, id, part)
            })
        });
        let w = weak.clone();
        main.on_close_requested(move || with_app(&w, App::close_main));
        main.on_exit_requested(|| {
            let _ = slint::quit_event_loop();
        });
        let w = weak.clone();
        main.on_about_requested(move || {
            with_app(&w, |app| {
                if let Err(error) = app.about.show() {
                    app.show_window_error(&error);
                    return;
                }
                app.about.invoke_take_focus();
            })
        });
        let w = weak;
        main.window().on_close_requested(move || {
            with_app(&w, App::close_main);
            slint::CloseRequestResponse::HideWindow
        });

        let about = self.about.as_weak();
        self.about.on_closed(move || {
            if let Some(about) = about.upgrade() {
                // A window that fails to hide stays open; the user can still close it.
                let _ = about.hide();
            }
        });
    }

    fn connect_flyout(self: &Rc<Self>) {
        let flyout = &self.flyout;
        let weak = Rc::downgrade(self);

        let w = weak.clone();
        flyout.on_power_toggled(move |on| with_app(&w, |app| app.update(|b| b.set_running(on))));
        let w = weak.clone();
        flyout.on_source_selected(move |index| with_app(&w, |app| app.select_source(index)));
        let w = weak.clone();
        flyout.on_master_volume_changed(move |volume| {
            with_app(&w, |app| app.update(|b| b.set_master_volume(volume.into())))
        });
        let w = weak.clone();
        flyout.on_speaker_toggled(move |id| with_app(&w, |app| app.toggle_speaker(&id)));
        let w = weak.clone();
        flyout.on_group_toggled(move |id| with_app(&w, |app| app.update(|b| b.toggle_group(&id))));
        let w = weak.clone();
        flyout.on_notice_dismissed(move |id| {
            with_app(&w, |app| app.update(|b| b.dismiss_notice(id as u64)))
        });
        let w = weak.clone();
        flyout.on_notices_dismissed_all(move || {
            with_app(&w, |app| app.update(|b| b.dismiss_all_notices()))
        });
        let w = weak.clone();
        flyout.on_notice_guide_requested(move |id| {
            with_app(&w, |app| {
                app.hide_flyout();
                app.show_guide(id);
            })
        });
        let w = weak.clone();
        flyout.on_notice_retry_requested(move |_| {
            with_app(&w, |app| app.update(|b| b.retry_airplay()))
        });
        let w = weak.clone();
        flyout.on_open_app(move || {
            with_app(&w, |app| {
                app.hide_flyout();
                app.show_main();
            })
        });
        flyout.on_exit_app(|| {
            let _ = slint::quit_event_loop();
        });
        let w = weak;
        flyout.on_close_requested(move || with_app(&w, App::hide_flyout));
    }

    fn connect_equalizer(self: &Rc<Self>) {
        self.follow_frame(|app| app.equalizer.window(), "equalizer");
        let eq_window = &self.equalizer;
        let weak = Rc::downgrade(self);

        let w = weak.clone();
        eq_window.on_enabled_toggled(move |on| {
            with_app(&w, |app| app.change_equalizer(|s| s.enabled = on))
        });
        let w = weak.clone();
        eq_window.on_preamp_changed(move |db| {
            with_app(&w, |app| app.change_equalizer(|s| s.preamp_db = db.into()))
        });
        let w = weak.clone();
        eq_window.on_gain_changed(move |band, db| {
            with_app(&w, |app| {
                app.change_equalizer(|s| {
                    if let Some(gain) = usize::try_from(band)
                        .ok()
                        .and_then(|b| s.gains_db.get_mut(b))
                    {
                        *gain = db.into();
                    }
                })
            })
        });
        let w = weak.clone();
        eq_window.on_preset_selected(move |index| {
            with_app(&w, |app| {
                if let Some(preset) = usize::try_from(index).ok().and_then(|i| eq::PRESETS.get(i)) {
                    // Like the Mac: picking a preset turns the equalizer on.
                    app.change_equalizer(|s| {
                        s.apply_preset(preset);
                        s.enabled = true;
                    });
                }
            })
        });
        let w = weak.clone();
        eq_window.on_reset(move || {
            with_app(&w, |app| {
                app.change_equalizer(|s| {
                    s.apply_preset(&eq::PRESETS[0]);
                    s.preamp_db = 0.0;
                })
            })
        });
        let w = weak;
        eq_window.on_close_requested(move || {
            with_app(&w, |app| {
                let _ = app.equalizer.hide();
            })
        });
    }

    fn connect_dialogs(self: &Rc<Self>) {
        self.follow_frame(|app| app.group_dialog.window(), "group name dialog");
        self.follow_frame(|app| app.about.window(), "about dialog");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.group_dialog
            .on_confirmed(move |name| with_app(&w, |app| app.confirm_dialog(&name)));
        let w = weak.clone();
        self.group_dialog.on_cancelled(move || {
            with_app(&w, |app| {
                let _ = app.group_dialog.hide();
            })
        });

        self.follow_frame(|app| app.pin_dialog.window(), "PIN dialog");
        let dialog = self.pin_dialog.as_weak();
        self.pin_dialog.on_pin_edited(move |text| {
            if let Some(dialog) = dialog.upgrade() {
                let digits = pin_digits(&text);
                if digits != text.as_str() {
                    dialog.set_pin(digits.into());
                }
            }
        });
        let w = weak.clone();
        self.pin_dialog
            .on_confirmed(move |pin| with_app(&w, |app| app.confirm_pin(&pin)));
        let w = weak.clone();
        self.pin_dialog
            .on_cancelled(move || with_app(&w, App::cancel_pin));
        let w = weak;
        self.pin_dialog.window().on_close_requested(move || {
            with_app(&w, App::cancel_pin);
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// Opens, updates or closes the PIN dialog for the pairing that asks for
    /// a PIN. It opens when the Apple TV first shows a PIN, stays open while
    /// that pairing runs (a wrong PIN clears the field for the new one), and
    /// closes when it ends.
    fn show_pin_prompt(&self, prompt: Option<&PinPrompt>) {
        if self.pin_preview.get() {
            return;
        }
        let dialog = &self.pin_dialog;
        let Some(prompt) = prompt else {
            if self.pin_for.borrow_mut().take().is_some() {
                let _ = dialog.hide();
                self.log.log(|| "PIN dialog: closed".to_owned());
            }
            return;
        };
        let opening = self.pin_for.borrow().as_deref() != Some(prompt.speaker_id.as_str());
        let phase = pin_phase(prompt.phase);
        let new_code = (prompt.wrong_pin && !dialog.get_wrong_pin())
            || (phase == PinPhase::Ready && dialog.get_phase() != PinPhase::Ready);
        dialog.set_speaker(prompt.name.as_str().into());
        dialog.set_phase(phase);
        dialog.set_wrong_pin(prompt.wrong_pin);
        if opening {
            *self.pin_for.borrow_mut() = Some(prompt.speaker_id.clone());
            dialog.set_pin(SharedString::new());
            self.open_pin_dialog();
            self.log
                .log(|| format!("PIN dialog: opened for {}", prompt.name));
        } else if new_code {
            if prompt.wrong_pin {
                dialog.set_pin(SharedString::new());
            }
            dialog.invoke_prepare();
        }
    }

    fn open_pin_dialog(&self) {
        if let Err(error) = self.pin_dialog.show() {
            self.show_window_error(&error);
            return;
        }
        self.pin_dialog.invoke_prepare();
        crate::platform::bring_to_front(self.pin_dialog.window());
    }

    /// Pair: the PIN goes to the pairing; the dialog stays open until it
    /// ends.
    fn confirm_pin(&self, pin: &str) {
        if self.pin_preview.get() {
            self.close_pin_preview();
            return;
        }
        let id = self.pin_for.borrow().clone();
        if let Some(id) = id {
            self.update(|b| b.submit_pin(&id, pin));
        }
    }

    /// Cancel, Escape or the close button: the pairing stops.
    fn cancel_pin(&self) {
        if self.pin_preview.get() {
            self.close_pin_preview();
            return;
        }
        let id = self.pin_for.borrow_mut().take();
        let _ = self.pin_dialog.hide();
        if let Some(id) = id {
            self.update(|b| b.cancel_pairing(&id));
        }
    }

    fn close_pin_preview(&self) {
        self.pin_preview.set(false);
        let _ = self.pin_dialog.hide();
    }

    /// Opens the PIN dialog for a made-up Apple TV, to look at it without
    /// one: the PIN preview, debug only. `wrong`: as after a wrong PIN.
    pub fn preview_pin(&self, wrong: bool) {
        self.pin_preview.set(true);
        let dialog = &self.pin_dialog;
        dialog.set_speaker(self.strings().get_pin_preview_speaker());
        dialog.set_phase(PinPhase::Ready);
        dialog.set_wrong_pin(wrong);
        dialog.set_pin(SharedString::new());
        self.open_pin_dialog();
    }

    fn connect_guide(self: &Rc<Self>) {
        self.follow_frame(|app| app.guide.window(), "guide");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.guide
            .on_page_requested(move |page| with_app(&w, |app| app.open_guide_page(page)));
        let w = weak.clone();
        self.guide
            .on_close_requested(move || with_app(&w, App::close_guide));
        let w = weak;
        self.guide.window().on_close_requested(move || {
            with_app(&w, App::close_guide);
            slint::CloseRequestResponse::HideWindow
        });
    }

    fn close_guide(&self) {
        self.guide_follows_notice.set(false);
        let _ = self.guide.hide();
    }

    /// Opens the Settings page a guide button asks for; only the pages the
    /// guide names, nothing else. When the shell doesn't take it, the guide
    /// says where to find the page.
    fn open_guide_page(&self, page: GuidePage) {
        let Some(page) = settings_page(page) else {
            return;
        };
        let opened = crate::platform::open_settings(page.uri());
        if !opened {
            self.log
                .log(|| format!("guide: the shell did not open {}", page.uri()));
        }
        self.guide.set_settings_failed(!opened);
    }

    /// Keeps an open guide in step with the double-audio notice it was
    /// opened from: updated while that notice changes, closed once it is
    /// gone (the app quit, streaming stopped, the routing was fixed).
    fn refresh_guide(&self, state: &AppState) {
        if !self.guide_follows_notice.get() {
            return;
        }
        match guide::current(&state.notices) {
            Some(guide) => self.fill_guide(&guide),
            None => {
                self.log
                    .log(|| "guide: closed, its notice is gone".to_owned());
                self.close_guide();
            }
        }
    }

    /// Shows `guide` in the guide window, row by row so a focused button
    /// keeps its focus.
    fn fill_guide(&self, guide: &Guide) {
        let strings = self.strings();
        self.guide.set_intro(guide_intro(&strings, guide));
        let items: Vec<GuideStepItem> = guide
            .steps
            .iter()
            .map(|step| GuideStepItem {
                text: guide_step_text(&strings, step),
                page: guide_page(step.page()),
            })
            .collect();
        sync_model(&self.guide_steps, items);
    }

    /// Opens the guide of notice `id`, if it is still there and has one.
    fn show_guide(&self, id: i32) {
        let state = self.snapshot();
        let Some(notice) = state
            .notices
            .iter()
            .find(|notice| i32::try_from(notice.id).ok() == Some(id))
        else {
            return;
        };
        if self.show_guide_for(&notice.message) {
            self.guide_follows_notice.set(true);
        }
    }

    /// Opens the guide for a notice saying `message`, if it has one, and
    /// brings it to the front. Returns whether it opened. A guide opened
    /// this way stays as it is; one opened from a notice follows it.
    pub fn show_guide_for(&self, message: &NoticeMessage) -> bool {
        let Some(guide) = guide::guide(message) else {
            return false;
        };
        self.guide_follows_notice.set(false);
        self.fill_guide(&guide);
        self.guide.set_settings_failed(false);
        if let Err(error) = self.guide.show() {
            self.show_window_error(&error);
            return false;
        }
        self.guide.invoke_take_focus();
        crate::platform::bring_to_front(self.guide.window());
        true
    }

    fn select_source(&self, index: i32) {
        let state = self.snapshot();
        if let Some(source) = usize::try_from(index)
            .ok()
            .and_then(|i| state.sources.get(i))
        {
            self.update(|b| b.select_source(&source.id));
        }
    }

    fn toggle_speaker(&self, id: &str) {
        let state = self.snapshot();
        if let Some(speaker) = state.speaker(id) {
            let on = state.speaker_state(speaker).click_turns_on();
            self.update(|b| b.set_speaker_enabled(id, on));
        }
    }

    /// Opens or closes a speaker's sync controls. Closing applies what was
    /// typed in its field; other rows' controls stay open with their typing.
    fn toggle_sync(&self, id: &str) {
        let closed = {
            let mut ui = self.ui.borrow_mut();
            let closed = ui.sync_open.remove(id);
            if !closed {
                ui.sync_open.insert(id.to_string());
                ui.delay_drafts.remove(id);
            }
            closed
        };
        if closed {
            self.commit_delay(id);
        } else {
            self.refresh();
        }
    }

    fn show_virtual_devices(&self, on: bool) {
        {
            let mut ui = self.ui.borrow_mut();
            if ui.show_virtual != on {
                ui.listed_virtual.forget();
            }
            ui.show_virtual = on;
        }
        self.remember_view();
        self.refresh();
    }

    /// How the main window shows now, with its place and size.
    fn main_window_shows(&self) -> MainWindowShows {
        let window = self.main.window();
        if !window.is_visible() || window.is_minimized() {
            return MainWindowShows::Not;
        }
        let position = window.position();
        let size = window.size().to_logical(window.scale_factor());
        let frame = WindowFrame {
            x: position.x,
            y: position.y,
            width: f64::from(size.width),
            height: f64::from(size.height),
            maximized: false,
        };
        if window.is_maximized() {
            MainWindowShows::Maximized(frame)
        } else {
            MainWindowShows::Normal(frame)
        }
    }

    /// Hands the windows' own settings to the backend, which saves them with
    /// the rest: View > Show Virtual Devices, and the main window's frame
    /// (see [`ui_model::frame_to_keep`]).
    fn remember_view(&self) {
        let mut backend = self.backend.borrow_mut();
        let saved = backend.view_settings();
        let settings = ViewSettings {
            show_virtual_devices: self.ui.borrow().show_virtual,
            window: ui_model::frame_to_keep(self.main_window_shows(), saved.window),
        };
        if settings != saved {
            backend.set_view_settings(settings);
        }
    }

    /// Shows the windows with the settings saved last time: View > Show
    /// Virtual Devices, and the main window's size and place, fitted to the
    /// monitor it opens on (the one at its saved place, else the primary
    /// one, where it opens centered), maximized when it was.
    fn restore_view(&self) {
        let saved = self.backend.borrow().view_settings();
        self.ui.borrow_mut().show_virtual = saved.show_virtual_devices;
        let Some(saved_frame) = saved.window else {
            return;
        };
        let on_a_monitor = crate::platform::is_on_a_monitor(saved_frame.x, saved_frame.y);
        // The primary monitor holds the point (0, 0).
        let (x, y) = if on_a_monitor {
            (saved_frame.x, saved_frame.y)
        } else {
            (0, 0)
        };
        let monitor = crate::platform::monitor_at(f64::from(x), f64::from(y));
        let frame = monitor.map_or(saved_frame, |(work_area, scale)| {
            fitted_main_frame(saved_frame, work_area, scale, on_a_monitor)
        });
        let window = self.main.window();
        let size = slint::LogicalSize::new(frame.width as f32, frame.height as f32);
        window.set_size(size);
        // The native menu bar is attached when the native window is created
        // and takes its height out of a size set before that, so the window
        // would shrink by the menu bar at every restart: set the size again
        // once the native window exists (and maximize it after that).
        set_size_once_native(
            self.main.as_weak(),
            size,
            frame.maximized,
            NATIVE_WINDOW_TRIES,
        );
        if on_a_monitor || monitor.is_some() {
            window.set_position(slint::PhysicalPosition::new(frame.x, frame.y));
        }
        let maximized = if frame.maximized { ", maximized" } else { "" };
        if frame == saved_frame {
            self.log.log(|| {
                format!(
                    "main window: {} x {} as last time{maximized}",
                    frame.width, frame.height
                )
            });
        } else {
            self.log.log(|| {
                format!(
                    "main window: {} x {}, fitted to the monitor (saved {} x {}){maximized}",
                    frame.width, frame.height, saved_frame.width, saved_frame.height
                )
            });
        }
    }

    /// Saves the settings now, with the main window's frame: the app is
    /// about to quit.
    pub fn save_settings(&self) {
        // Quit drops a typed delay that wasn't applied, so a focus loss as
        // the window goes applies nothing either.
        {
            let mut ui = self.ui.borrow_mut();
            ui.delay_drafts.clear();
            ui.airplay_delay_draft.cancel();
        }
        self.remember_view();
        self.backend.borrow_mut().save_settings();
    }

    fn step_delay(&self, id: &str, delta: i32) {
        let state = self.snapshot();
        let Some(current) = state.speaker(id).and_then(|s| s.delay_ms) else {
            return;
        };
        self.ui.borrow_mut().delay_drafts.remove(id);
        self.update(|b| b.set_speaker_delay(id, current + f64::from(delta)));
    }

    /// Keeps what is typed in a speaker's delay field ("+150", "−250");
    /// nothing applies until Enter, its delay button or a click outside
    /// every control (see [`TypedDelay`]).
    fn edit_delay(&self, id: &str, text: &str) {
        let shown = self.snapshot().speaker(id).and_then(|s| s.delay_ms);
        self.ui
            .borrow_mut()
            .delay_drafts
            .entry(id.to_string())
            .or_default()
            .edit(text, shown);
        self.refresh();
    }

    /// Enter in a speaker's delay field: applies what was typed and closes
    /// that speaker's sync controls; other rows' stay open with their typing.
    fn enter_delay(&self, id: &str) {
        self.ui.borrow_mut().sync_open.remove(id);
        self.commit_delay(id);
        self.focus_delay_button(id);
    }

    /// A click on the window outside every control (`apply`), or Escape in
    /// a delay field: every typed delay applies, or none does, and every
    /// sync control closes. Unreadable text applies nothing.
    fn end_delay_typing(&self, apply: bool) {
        let idle = {
            let ui = self.ui.borrow();
            ui.sync_open.is_empty()
                && ui.delay_drafts.is_empty()
                && ui.airplay_delay_draft.draft.is_none()
        };
        if idle {
            return;
        }
        let focused = match &self.main_focus.borrow().speaker {
            Some((id, RowPart::Sync)) => Some(id.clone()),
            _ => None,
        };
        if apply {
            let typed: Vec<String> = self.ui.borrow().delay_drafts.keys().cloned().collect();
            for id in typed {
                self.commit_delay(&id);
            }
            self.commit_airplay_delay();
        }
        {
            let mut ui = self.ui.borrow_mut();
            ui.sync_open.clear();
            ui.delay_drafts.clear();
            ui.airplay_delay_draft.cancel();
        }
        self.refresh();
        if let Some(id) = focused {
            self.focus_delay_button(&id);
        }
    }

    /// The speaker's sync controls closed under the keyboard focus: its
    /// delay button takes it, so Narrator and Tab carry on from there.
    fn focus_delay_button(&self, id: &str) {
        self.main.set_focus_speaker_part(RowPart::Delay);
        self.main.set_focus_speaker_id(id.into());
    }

    /// Applies the typed offset (Enter, the delay button, a click outside
    /// every control). Unreadable text applies nothing and the field shows
    /// the applied value again; out of range, it shows the value that was
    /// applied.
    fn commit_delay(&self, id: &str) {
        let shown = self.snapshot().speaker(id).and_then(|s| s.delay_ms);
        let typed = self.ui.borrow_mut().delay_drafts.remove(id);
        let ms = typed
            .as_ref()
            .and_then(|typed| typed.text(shown))
            .and_then(ui_model::parse_delay_ms);
        match ms {
            Some(ms) => self.update(|b| b.set_speaker_delay(id, ms)),
            None => self.refresh(),
        }
    }

    fn step_airplay_delay(&self, delta: i32) {
        let Some(current) = self.snapshot().airplay_delay_ms else {
            return;
        };
        self.ui.borrow_mut().airplay_delay_draft.cancel();
        self.update(|b| b.set_airplay_delay(current + f64::from(delta)));
    }

    /// Keeps what is typed in the AirPlay delay field; nothing applies until
    /// Enter or a click outside every control (see [`TypedDelay`]).
    fn edit_airplay_delay(&self, text: &str) {
        let shown = self.snapshot().airplay_delay_ms;
        self.ui.borrow_mut().airplay_delay_draft.edit(text, shown);
        self.refresh();
    }

    /// Applies the typed AirPlay delay (Enter, a click outside every
    /// control). Clamped, the field shows the value that was applied.
    fn commit_airplay_delay(&self) {
        let shown = self.snapshot().airplay_delay_ms;
        let typed = self.ui.borrow_mut().airplay_delay_draft.commit(shown);
        match typed {
            Some(ms) => self.update(|b| b.set_airplay_delay(ms)),
            None => self.refresh(),
        }
    }

    fn change_equalizer(&self, change: impl FnOnce(&mut EqSettings)) {
        let mut settings = self.snapshot().equalizer;
        change(&mut settings);
        self.update(|b| b.set_equalizer(settings));
    }

    pub fn show_equalizer(&self) {
        if let Err(error) = self.equalizer.show() {
            self.show_window_error(&error);
            return;
        }
        self.apply_frame(self.equalizer.window(), "equalizer", Frame::Titled);
        self.equalizer.invoke_take_focus();
        crate::platform::bring_to_front(self.equalizer.window());
    }

    fn ask_new_group(&self) {
        let strings = self.strings();
        let dialog = &self.group_dialog;
        if self.snapshot().active_speakers() == 0 {
            *self.dialog_purpose.borrow_mut() = DialogPurpose::Message;
            dialog.set_heading(strings.get_new_group_title());
            dialog.set_message(strings.get_new_group_needs_speakers());
            dialog.set_show_field(false);
            dialog.set_confirm_label(strings.get_ok());
        } else {
            *self.dialog_purpose.borrow_mut() = DialogPurpose::NewGroup;
            dialog.set_heading(strings.get_new_group_title());
            dialog.set_message(strings.get_new_group_message());
            dialog.set_placeholder(strings.get_new_group_placeholder());
            dialog.set_name(SharedString::new());
            dialog.set_show_field(true);
            dialog.set_confirm_label(strings.get_create());
        }
        self.open_dialog();
    }

    fn ask_rename(&self, id: &str) {
        let state = self.snapshot();
        let Some(group) = state.groups.iter().find(|g| g.id == id) else {
            return;
        };
        let strings = self.strings();
        let dialog = &self.group_dialog;
        *self.dialog_purpose.borrow_mut() = DialogPurpose::Rename(id.to_string());
        dialog.set_heading(strings.get_rename_group_title());
        dialog.set_message(strings.invoke_rename_group_message(group.name.as_str().into()));
        dialog.set_placeholder(SharedString::new());
        dialog.set_name(group.name.as_str().into());
        dialog.set_show_field(true);
        dialog.set_confirm_label(strings.get_rename());
        self.open_dialog();
    }

    fn open_dialog(&self) {
        if let Err(error) = self.group_dialog.show() {
            self.show_window_error(&error);
            return;
        }
        self.group_dialog.invoke_prepare();
        crate::platform::bring_to_front(self.group_dialog.window());
    }

    fn confirm_dialog(&self, name: &str) {
        let purpose = self.dialog_purpose.borrow().clone();
        let _ = self.group_dialog.hide();
        match purpose {
            DialogPurpose::NewGroup => {
                let name = if name.trim().is_empty() {
                    let number = self.snapshot().groups.len() + 1;
                    self.strings()
                        .invoke_numbered_group(count(number))
                        .to_string()
                } else {
                    name.to_string()
                };
                // The dialog only offers a name while a speaker is on; if the
                // last one went off meanwhile, nothing is made.
                let _ = self.backend.borrow_mut().new_group(&name);
                self.refresh();
            }
            DialogPurpose::Rename(id) => self.update(|b| b.rename_group(&id, name)),
            DialogPurpose::Message => {}
        }
    }

    /// Closing the main window keeps AudioSync running in the tray, like the
    /// Mac app in the menu bar. Without a tray icon, it quits.
    fn close_main(&self) {
        // With a tray this is a click outside the controls, so what was typed
        // applies; without one it quits, and a quit drops it.
        self.end_delay_typing(self.tray_available.get());
        self.remember_view();
        let _ = self.main.hide();
        if !self.tray_available.get() {
            let _ = slint::quit_event_loop();
        }
    }

    fn show_window_error(&self, error: &slint::PlatformError) {
        let text = self
            .strings()
            .invoke_window_failed(error.to_string().into());
        crate::platform::show_error(&text);
    }

    pub fn hide_flyout(&self) {
        let _ = self.flyout.hide();
    }
}

/// The properties the main window and the flyout share.
trait WindowState {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32);
}

impl WindowState for MainWindow {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32) {
        self.set_running(state.running);
        self.set_status(status.clone());
        self.set_source_index(source_index);
        self.set_master_volume(state.master_volume as f32);
    }
}

impl WindowState for TrayFlyout {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32) {
        self.set_running(state.running);
        self.set_status(status.clone());
        self.set_source_index(source_index);
        self.set_master_volume(state.master_volume as f32);
    }
}

/// The newest `limit` notices: what a window lists. A window that shows
/// fewer than all of them says how many more there are instead.
fn newest_notices<T: Clone>(notices: &[T], limit: usize) -> Vec<T> {
    notices.iter().take(limit).cloned().collect()
}

/// Replaces the rows that changed; the whole list only when its length did.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, items: Vec<T>) {
    if model.row_count() == items.len() {
        for (row, item) in items.into_iter().enumerate() {
            if model.row_data(row).as_ref() != Some(&item) {
                model.set_row_data(row, item);
            }
        }
    } else {
        model.set_vec(items);
    }
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Empties the notice live regions of the windows still there.
fn clear_announcement(main: &slint::Weak<MainWindow>, flyout: &slint::Weak<TrayFlyout>) {
    if let Some(main) = main.upgrade() {
        main.set_notice_announcement(SharedString::new());
    }
    if let Some(flyout) = flyout.upgrade() {
        flyout.set_notice_announcement(SharedString::new());
    }
}

/// The tray icon's tooltip: AudioSync and its status, then the newest
/// notice's title on a second line while there is a notice, so Narrator
/// reads it on the tray icon even with every window closed.
pub fn tray_tooltip(strings: &Strings<'_>, state: &AppState) -> String {
    let speakers = count(state.active_speakers());
    let text = match state.notices.iter().max_by_key(|n| n.id) {
        Some(notice) => strings.invoke_tray_tooltip_notice(
            state.running,
            speakers,
            notice_title(strings, &notice.message),
        ),
        None => strings.invoke_tray_tooltip(state.running, speakers),
    };
    fit_tooltip(&text)
}

/// `text` cut to what a tray tooltip holds, ending in "…" when cut.
fn fit_tooltip(text: &str) -> String {
    if text.encode_utf16().count() <= TOOLTIP_MAX_UNITS {
        return text.to_owned();
    }
    let room = TOOLTIP_MAX_UNITS - '…'.len_utf16();
    let mut fitted = String::new();
    let mut units = 0;
    for c in text.chars() {
        units += c.len_utf16();
        if units > room {
            break;
        }
        fitted.push(c);
    }
    fitted.push('…');
    fitted
}

fn format_ms(ms: f64) -> String {
    format!("{}", ms.round() as i64)
}

/// An offset as its field shows it, with its sign: "+150", "−250", "0".
fn format_offset_ms(ms: f64) -> String {
    match ms.round() as i64 {
        whole if whole > 0 => format!("+{whole}"),
        whole if whole < 0 => format!("\u{2212}{}", whole.unsigned_abs()),
        _ => "0".to_owned(),
    }
}

pub fn row_state(state: SpeakerState) -> RowState {
    match state {
        SpeakerState::Off => RowState::Off,
        SpeakerState::WillPlay => RowState::WillPlay,
        SpeakerState::Playing => RowState::Playing,
        SpeakerState::Waiting(ui_model::Waiting::ForDevice) => RowState::WaitingForDevice,
        SpeakerState::Waiting(ui_model::Waiting::Reconnecting) => RowState::Reconnecting,
    }
}

pub fn speaker_kind(kind: ui_model::SpeakerKind) -> SpeakerKind {
    use ui_model::SpeakerKind as K;
    match kind {
        K::AirPlay => SpeakerKind::Airplay,
        K::BuiltIn => SpeakerKind::BuiltIn,
        K::Headphones => SpeakerKind::Headphones,
        K::Bluetooth => SpeakerKind::Bluetooth,
        K::Usb => SpeakerKind::Usb,
        K::Hdmi => SpeakerKind::Hdmi,
        K::DisplayPort => SpeakerKind::DisplayPort,
        K::Virtual => SpeakerKind::Virtual,
        K::Other => SpeakerKind::Other,
    }
}

fn availability(a: ui_model::Availability) -> Availability {
    match a {
        ui_model::Availability::Open => Availability::Open,
        ui_model::Availability::NeedsCode => Availability::NeedsCode,
        ui_model::Availability::Pairing => Availability::Pairing,
        ui_model::Availability::WaitingToPair => Availability::WaitingToPair,
        ui_model::Availability::PasswordNotYet => Availability::PasswordNotYet,
        ui_model::Availability::PairingNotYet => Availability::PairingNotYet,
        ui_model::Availability::AirPlay2NotYet => Availability::Airplay2NotYet,
        ui_model::Availability::Unsupported => Availability::Unsupported,
    }
}

fn airplay_problem(problem: ui_model::AirPlayProblem) -> AirPlayProblem {
    use ui_model::AirPlayProblem as P;
    match problem {
        P::NotResponding => AirPlayProblem::NotResponding,
        P::ConnectionLost => AirPlayProblem::ConnectionLost,
        P::Busy => AirPlayProblem::Busy,
        P::Refused => AirPlayProblem::Refused,
        P::PasswordNotYet => AirPlayProblem::PasswordNotYet,
        P::NeedsPin => AirPlayProblem::NeedsPin,
        P::PairingFailed => AirPlayProblem::PairingFailed,
        P::PairingNotYet => AirPlayProblem::PairingNotYet,
        P::AirPlay2NotYet => AirPlayProblem::Airplay2NotYet,
        P::Unsupported => AirPlayProblem::Unsupported,
    }
}

/// A device-volume hint as a row shows it, with the whole percent it names
/// (spec S198, S199).
fn device_volume_hint(hint: Option<Hint>) -> (DeviceVolumeHint, i32) {
    let percent = |percent: u32| i32::try_from(percent).unwrap_or(i32::MAX);
    match hint {
        None => (DeviceVolumeHint::None, 0),
        Some(Hint::Low { percent: n }) => (DeviceVolumeHint::Low, percent(n)),
        Some(Hint::Muted) => (DeviceVolumeHint::Muted, 0),
        Some(Hint::MutedAndLow { percent: n }) => (DeviceVolumeHint::MutedLow, percent(n)),
    }
}

fn pin_phase(phase: ui_model::PinPhase) -> PinPhase {
    match phase {
        ui_model::PinPhase::Starting => PinPhase::Starting,
        ui_model::PinPhase::Ready => PinPhase::Ready,
        ui_model::PinPhase::Checking => PinPhase::Checking,
    }
}

/// A speaker as a row shows it. `draft`: what is typed in its delay field.
pub fn speaker_item(
    state: &AppState,
    speaker: &ui_model::Speaker,
    show_kind: bool,
    sync_open: bool,
    draft: Option<&str>,
) -> SpeakerItem {
    let delay = format_offset_ms(speaker.delay_ms.unwrap_or(0.0));
    let (device_volume, device_volume_percent) = device_volume_hint(speaker.device_volume);
    SpeakerItem {
        id: speaker.id.as_str().into(),
        name: speaker.name.as_str().into(),
        kind: speaker_kind(speaker.kind),
        show_kind,
        state: row_state(state.speaker_state(speaker)),
        volume: speaker.volume as f32,
        has_delay: speaker.delay_ms.is_some(),
        delay_ms: speaker.delay_ms.unwrap_or(0.0).round() as i32,
        delay_pending: is_draft(draft, &delay),
        delay_text: draft.map_or(delay, str::to_string).into(),
        reference_name: state.reference_name.as_deref().unwrap_or_default().into(),
        sync_open: sync_open && speaker.delay_ms.is_some(),
        auto_connect: speaker.auto_connect,
        availability: availability(speaker.availability),
        paired: speaker.paired,
        device_volume,
        device_volume_percent,
    }
}

pub fn group_item(state: &AppState, group: &ui_model::Group) -> GroupItem {
    GroupItem {
        id: group.id.as_str().into(),
        name: group.name.as_str().into(),
        state: row_state(state.group_state(group)),
        available: count(state.available_members(group).count()),
        total: count(group.members.len()),
    }
}

pub fn notice_item(strings: &Strings<'_>, notice: &ui_model::Notice) -> NoticeItem {
    NoticeItem {
        id: i32::try_from(notice.id).unwrap_or(i32::MAX),
        message: notice_text(strings, &notice.message),
        title: notice_title(strings, &notice.message),
        time: notice.time.as_str().into(),
        has_guide: guide::guide(&notice.message).is_some(),
        has_retry: notice.message == NoticeMessage::AirPlayStopped,
    }
}

/// Names as one list, "A, B and C": commas between all but the last two,
/// then `and` (the UI's words for "head and last").
fn join_names(names: &[String], and: impl FnOnce(&str, &str) -> String) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [head @ .., last] => and(&head.join(", "), last),
    }
}

/// Output names as one list in the UI's words: what every notice and the
/// guide name outputs with.
fn joined(strings: &Strings<'_>, names: &[String]) -> SharedString {
    join_names(names, |head, last| {
        strings
            .invoke_list_and(head.into(), last.into())
            .to_string()
    })
    .into()
}

/// The guide's first paragraph, in the UI's words.
fn guide_intro(strings: &Strings<'_>, guide: &Guide) -> SharedString {
    let outputs = joined(strings, &guide.outputs);
    match &guide.source {
        Some(source) => strings.invoke_guide_intro_app(source.as_str().into(), outputs),
        None => strings.invoke_guide_intro_system(outputs),
    }
}

/// A guide step in the UI's words.
fn guide_step_text(strings: &Strings<'_>, step: &GuideStep) -> SharedString {
    match step {
        GuideStep::RouteApp { app } => strings.invoke_guide_route_app(app.as_str().into()),
        GuideStep::ChangeDefault { current } => {
            strings.invoke_guide_change_default(current.as_str().into())
        }
        GuideStep::RouteOtherApps { outputs } => {
            strings.invoke_guide_route_other_apps(joined(strings, outputs))
        }
        GuideStep::MuteOutputs { outputs } => {
            strings.invoke_guide_mute_outputs(joined(strings, outputs), count(outputs.len()))
        }
        GuideStep::SharedOutputs { outputs } => {
            strings.invoke_guide_shared_outputs(joined(strings, outputs))
        }
        GuideStep::KeepAppUnmuted { app } => {
            strings.invoke_guide_keep_app_unmuted(app.as_str().into())
        }
    }
}

/// The UI's name of a step's Settings page.
fn guide_page(page: Option<SettingsPage>) -> GuidePage {
    match page {
        None => GuidePage::None,
        Some(SettingsPage::VolumeMixer) => GuidePage::VolumeMixer,
        Some(SettingsPage::Sound) => GuidePage::Sound,
    }
}

/// The Settings page a step's button asks for.
fn settings_page(page: GuidePage) -> Option<SettingsPage> {
    match page {
        GuidePage::None => None,
        GuidePage::VolumeMixer => Some(SettingsPage::VolumeMixer),
        GuidePage::Sound => Some(SettingsPage::Sound),
    }
}

/// A notice in the UI's words.
fn notice_text(strings: &Strings<'_>, message: &NoticeMessage) -> SharedString {
    match message {
        NoticeMessage::NoSpeaker => strings.get_notice_no_speaker(),
        NoticeMessage::SourceQuit { source } => {
            strings.invoke_notice_source_quit(source.as_str().into())
        }
        NoticeMessage::SourceNotRunning { source } => {
            strings.invoke_notice_source_not_running(source.as_str().into())
        }
        NoticeMessage::SourceRefused { source } => {
            strings.invoke_notice_source_refused(source.as_str().into())
        }
        NoticeMessage::CaptureFailed => strings.get_notice_capture_failed(),
        NoticeMessage::CaptureLost => strings.get_notice_capture_lost(),
        NoticeMessage::SpeakerFailed { speaker } => {
            strings.invoke_notice_speaker_failed(speaker.as_str().into())
        }
        NoticeMessage::SpeakerLost { speaker } => {
            strings.invoke_notice_speaker_lost(speaker.as_str().into())
        }
        NoticeMessage::SpeakerBusy { speaker } => {
            strings.invoke_notice_speaker_busy(speaker.as_str().into())
        }
        NoticeMessage::AirPlayRetrying { speaker, problem }
        | NoticeMessage::AirPlayCantPlay { speaker, problem } => {
            strings.invoke_notice_airplay(airplay_problem(*problem), speaker.as_str().into())
        }
        NoticeMessage::AirPlayStopped => strings.get_notice_airplay_stopped(),
        NoticeMessage::SettingsNotSaved => strings.get_notice_settings_not_saved(),
        NoticeMessage::PairingsNotSaved => strings.get_notice_pairings_not_saved(),
        NoticeMessage::SettingsKeptAside { file } => {
            strings.invoke_notice_settings_kept_aside(file.as_str().into())
        }
        NoticeMessage::PairingsKeptAside { file } => {
            strings.invoke_notice_pairings_kept_aside(file.as_str().into())
        }
        NoticeMessage::NoSoundOutput => strings.get_notice_no_sound_output(),
        NoticeMessage::HeardTwice {
            source,
            mute,
            shared,
            ..
        } => {
            let mute = joined(strings, mute);
            let shared = joined(strings, shared);
            match source {
                Some(source) => {
                    strings.invoke_notice_heard_twice_app(source.as_str().into(), mute, shared)
                }
                None => strings.invoke_notice_heard_twice_system(mute, shared),
            }
        }
    }
}

/// A few words that name a notice, for its dismiss button.
fn notice_title(strings: &Strings<'_>, message: &NoticeMessage) -> SharedString {
    let (kind, name) = match message {
        NoticeMessage::NoSpeaker => (NoticeKind::NoSpeaker, ""),
        NoticeMessage::SourceQuit { source } => (NoticeKind::SourceQuit, source.as_str()),
        NoticeMessage::SourceNotRunning { source } => {
            (NoticeKind::SourceNotRunning, source.as_str())
        }
        NoticeMessage::SourceRefused { source } => (NoticeKind::SourceRefused, source.as_str()),
        NoticeMessage::CaptureFailed => (NoticeKind::CaptureFailed, ""),
        NoticeMessage::CaptureLost => (NoticeKind::CaptureLost, ""),
        NoticeMessage::SpeakerFailed { speaker } => (NoticeKind::SpeakerFailed, speaker.as_str()),
        NoticeMessage::SpeakerLost { speaker } => (NoticeKind::SpeakerLost, speaker.as_str()),
        NoticeMessage::SpeakerBusy { speaker } => (NoticeKind::SpeakerBusy, speaker.as_str()),
        NoticeMessage::AirPlayRetrying { speaker, .. } => {
            (NoticeKind::AirplayRetrying, speaker.as_str())
        }
        NoticeMessage::AirPlayCantPlay { speaker, .. } => {
            (NoticeKind::AirplayCantPlay, speaker.as_str())
        }
        NoticeMessage::AirPlayStopped => (NoticeKind::AirplayStopped, ""),
        NoticeMessage::SettingsNotSaved => (NoticeKind::SettingsNotSaved, ""),
        NoticeMessage::PairingsNotSaved => (NoticeKind::PairingsNotSaved, ""),
        NoticeMessage::SettingsKeptAside { .. } => (NoticeKind::SettingsKeptAside, ""),
        NoticeMessage::PairingsKeptAside { .. } => (NoticeKind::PairingsKeptAside, ""),
        NoticeMessage::NoSoundOutput => (NoticeKind::NoSoundOutput, ""),
        NoticeMessage::HeardTwice { .. } => (NoticeKind::HeardTwice, ""),
    };
    strings.invoke_notice_title(kind, name.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::MockBackend;

    use slint::platform::software_renderer::{MinimalSoftwareWindow, Rgb565Pixel};
    use slint::platform::{Key, PointerEventButton, WindowEvent};

    /// A platform without native windows, so a test can build the whole App
    /// and drive it through the windows' callbacks or keys.
    struct Headless;

    thread_local! {
        /// The windows the headless platform made on this thread.
        static HEADLESS_WINDOWS: RefCell<Vec<Weak<MinimalSoftwareWindow>>> =
            RefCell::default();
    }

    impl slint::platform::Platform for Headless {
        fn create_window_adapter(
            &self,
        ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            let window = MinimalSoftwareWindow::new(Default::default());
            HEADLESS_WINDOWS.with(|windows| windows.borrow_mut().push(Rc::downgrade(&window)));
            Ok(window)
        }
    }

    /// Draws a frame of every headless window, as the event loop would after
    /// an event: lists are laid out then, and their new rows built.
    fn draw_frames() {
        slint::platform::update_timers_and_animations();
        let windows: Vec<_> = HEADLESS_WINDOWS
            .with(|windows| windows.borrow().iter().filter_map(Weak::upgrade).collect());
        for window in windows {
            window.set_size(slint::PhysicalSize::new(1000, 720));
            window.draw_if_needed(|renderer| {
                let mut pixels = vec![Rgb565Pixel::default(); 1000 * 720];
                renderer.render(&mut pixels, 1000);
            });
        }
        slint::platform::update_timers_and_animations();
    }

    /// Presses and releases `key` in `window`, then draws a frame.
    fn press(window: &slint::Window, key: impl Into<SharedString>) {
        let text: SharedString = key.into();
        window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window.dispatch_event(WindowEvent::KeyReleased { text });
        draw_frames();
    }

    /// Clicks `window` at `x`, `y` (logical pixels), then draws a frame.
    fn click(window: &slint::Window, x: f32, y: f32) {
        let position = slint::LogicalPosition::new(x, y);
        let button = PointerEventButton::Left;
        window.dispatch_event(WindowEvent::PointerMoved { position });
        window.dispatch_event(WindowEvent::PointerPressed { position, button });
        window.dispatch_event(WindowEvent::PointerReleased { position, button });
        draw_frames();
    }

    /// Presses Tab `times` times in `window`.
    fn tab(window: &slint::Window, times: usize) {
        for _ in 0..times {
            press(window, Key::Tab);
        }
    }

    /// The App over `backend`, with headless windows.
    fn app_over(backend: MockBackend) -> Rc<App> {
        // The platform is set per thread: a thread that already built an
        // App has it, which is fine.
        let _ = slint::platform::set_platform(Box::new(Headless));
        App::new(Box::new(backend), DebugLog::new(None)).expect("the windows build headless")
    }

    #[test]
    fn the_main_window_keeps_a_virtual_output_listed_after_it_is_turned_off() {
        let mut backend = MockBackend::new();
        let mut cable = backend.snapshot().speaker("local:desk").unwrap().clone();
        cable.id = "local:cable".into();
        cable.name = "Cable Input".into();
        cable.kind = ui_model::SpeakerKind::Virtual;
        cable.enabled = true;
        backend.add_speaker(cable);
        backend.set_running(true);
        let app = app_over(backend);
        let listed = || {
            app.models
                .all_speakers
                .iter()
                .any(|s| s.id == "local:cable")
        };
        assert!(!app.ui.borrow().show_virtual, "Show Virtual Devices is off");
        assert!(listed(), "on, the virtual output is listed");

        app.main.invoke_speaker_toggled("local:cable".into());
        let enabled = |app: &App| app.snapshot().speaker("local:cable").unwrap().enabled;
        assert!(!enabled(&app), "the click on its row turned it off");
        assert!(listed(), "turned off, its row stays under the focus");
        app.refresh();
        assert!(listed(), "and a later refresh keeps it");

        app.main.invoke_show_virtual_devices_toggled(true);
        assert!(listed(), "Show Virtual Devices on lists it");
        app.main.invoke_show_virtual_devices_toggled(false);
        assert!(
            !listed(),
            "Show Virtual Devices changed: off and hidden again, it is forgotten"
        );
    }

    #[test]
    fn a_speaker_that_cant_play_right_now_shows_waiting_not_playing() {
        let mut backend = MockBackend::new();
        backend.set_running(true);
        let state = backend.snapshot();
        let mut busy = state.speaker("local:speakers").unwrap().clone();
        busy.id = "local:busy".into();
        busy.waiting = Some(ui_model::Waiting::ForDevice);
        let mut retrying = state.speaker("airplay:kitchen").unwrap().clone();
        retrying.id = "airplay:retrying".into();
        retrying.waiting = Some(ui_model::Waiting::Reconnecting);
        backend.add_speaker(busy);
        backend.add_speaker(retrying);
        let app = app_over(backend);
        let row = |id: &str| {
            app.models
                .all_speakers
                .iter()
                .find(|s| s.id == id)
                .expect("listed")
        };
        assert_eq!(row("local:speakers").state, RowState::Playing);
        assert_eq!(row("local:busy").state, RowState::WaitingForDevice);
        assert_eq!(row("airplay:retrying").state, RowState::Reconnecting);

        // Narrator hears the row's state and what a click does; the note
        // under the name says the same.
        let strings = app.strings();
        let read = |state: RowState| {
            (
                strings.invoke_value(state).to_string(),
                strings.invoke_help(state).to_string(),
                strings.invoke_row_note(state).to_string(),
            )
        };
        let turn_off = strings.get_turn_off().to_string();
        let waiting = "Waiting for device".to_owned();
        assert_eq!(
            read(RowState::WaitingForDevice),
            (waiting.clone(), turn_off.clone(), waiting)
        );
        let reconnecting = "Reconnecting\u{2026}".to_owned();
        assert_eq!(
            read(RowState::Reconnecting),
            (reconnecting.clone(), turn_off.clone(), reconnecting)
        );
        assert_eq!(
            read(RowState::Playing),
            ("On".to_owned(), turn_off, String::new())
        );

        // A click turns a waiting speaker off: it is on.
        app.main.invoke_speaker_toggled("local:busy".into());
        assert!(!app.snapshot().speaker("local:busy").unwrap().enabled);
    }

    /// The delay field's text on the desk's row.
    fn desk_delay_text(app: &App) -> String {
        desk_row(app).delay_text.to_string()
    }

    /// The desk's row.
    fn desk_row(app: &App) -> SpeakerItem {
        app.models
            .all_speakers
            .iter()
            .chain(app.models.this_pc.iter())
            .chain(app.models.connected.iter())
            .find(|s| s.id == "local:desk")
            .expect("the desk has a row")
    }

    #[test]
    fn a_draft_is_typed_text_other_than_the_applied_value() {
        assert!(!is_draft(None, "+120"), "nothing typed");
        assert!(!is_draft(Some("+120"), "+120"), "the applied value typed");
        assert!(is_draft(Some("220"), "+120"));
        assert!(is_draft(Some("120"), "+120"), "the text differs");
        assert!(is_draft(Some("abc"), "+120"), "unreadable text");
        assert!(is_draft(Some(""), "0"), "an emptied field");
    }

    #[test]
    fn a_speakers_delay_field_marks_typing_until_it_applies_or_reverts() {
        let app = app_over(MockBackend::new());
        let pending = || desk_row(&app).delay_pending;
        assert!(!pending(), "the applied value is no draft");

        app.main
            .invoke_delay_edited("local:desk".into(), "220".into());
        assert!(pending(), "typing is a draft");
        app.main.invoke_delay_committed("local:desk".into());
        assert!(!pending(), "Enter clears it");

        app.main
            .invoke_delay_edited("local:desk".into(), "5".into());
        app.main.invoke_delay_cancelled("local:desk".into());
        assert!(!pending(), "Escape clears it");

        app.main
            .invoke_delay_edited("local:desk".into(), "abc".into());
        assert!(pending(), "unreadable text is a draft");
        app.main.invoke_delay_committed("local:desk".into());
        assert!(!pending(), "Enter on unreadable text restores the value");

        app.main
            .invoke_delay_edited("local:desk".into(), "9".into());
        app.main.invoke_delay_stepped("local:desk".into(), 10);
        assert!(!pending(), "a step clears it");

        app.main
            .invoke_delay_edited("local:desk".into(), "+230".into());
        assert!(!pending(), "the applied value typed again is no draft");
    }

    #[test]
    fn a_typed_speaker_delay_applies_only_on_enter() {
        let app = app_over(MockBackend::new());
        let applied = || app.snapshot().speaker("local:desk").unwrap().delay_ms;
        assert_eq!(applied(), Some(120.0), "the example desk has a delay");
        assert_eq!(desk_delay_text(&app), "+120");

        // Typing "220" applies nothing on the way (2, 22, 220).
        for text in ["2", "22", "220"] {
            app.main
                .invoke_delay_edited("local:desk".into(), text.into());
            assert_eq!(desk_delay_text(&app), text, "the field keeps what is typed");
            assert_eq!(applied(), Some(120.0), "typing {text} applies nothing");
        }
        app.main.invoke_delay_committed("local:desk".into());
        assert_eq!(applied(), Some(220.0), "Enter applies what was typed");
        assert_eq!(desk_delay_text(&app), "+220");

        // Escape drops the typing; an Enter after it applies nothing.
        app.main
            .invoke_delay_edited("local:desk".into(), "5".into());
        app.main.invoke_delay_cancelled("local:desk".into());
        assert_eq!(
            desk_delay_text(&app),
            "+220",
            "Escape shows the saved value"
        );
        app.main.invoke_delay_committed("local:desk".into());
        assert_eq!(applied(), Some(220.0), "Enter after Escape applies nothing");

        // Unreadable text reverts to the saved value.
        app.main
            .invoke_delay_edited("local:desk".into(), "abc".into());
        app.main.invoke_delay_committed("local:desk".into());
        assert_eq!(applied(), Some(220.0));
        assert_eq!(desk_delay_text(&app), "+220", "unreadable text reverts");

        // A step applies at once and drops half-typed text.
        app.main
            .invoke_delay_edited("local:desk".into(), "9".into());
        app.main.invoke_delay_stepped("local:desk".into(), 10);
        assert_eq!(applied(), Some(230.0), "a step applies at once");
        assert_eq!(desk_delay_text(&app), "+230", "a step drops the typed text");
        app.main.invoke_delay_committed("local:desk".into());
        assert_eq!(applied(), Some(230.0), "the dropped text stays dropped");
    }

    /// A speaker's applied delay.
    fn delay_of(app: &App, id: &str) -> Option<f64> {
        app.snapshot().speaker(id).unwrap().delay_ms
    }

    /// The speaker's row.
    fn row_of(app: &App, id: &str) -> SpeakerItem {
        app.models
            .this_pc
            .iter()
            .chain(app.models.connected.iter())
            .find(|s| s.id == id)
            .expect("the speaker has a row")
    }

    /// Opens the desk's and the headset's sync controls, types 400 into the
    /// desk's field and `headset` into the headset's.
    fn type_into_two_rows(app: &App, headset: &str) {
        for id in ["local:desk", "local:bt"] {
            app.main.invoke_sync_toggled(id.into());
        }
        app.main
            .invoke_delay_edited("local:desk".into(), "400".into());
        app.main
            .invoke_delay_edited("local:bt".into(), headset.into());
        assert_eq!(
            delay_of(app, "local:desk"),
            Some(120.0),
            "typing applies nothing"
        );
    }

    #[test]
    fn the_delay_button_applies_its_rows_typed_delay_and_closes_only_its_controls() {
        let app = app_over(MockBackend::new());
        type_into_two_rows(&app, "-200");

        app.main.invoke_sync_toggled("local:desk".into());
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(400.0),
            "closing applied it"
        );
        assert_eq!(desk_delay_text(&app), "+400");
        let ui = app.ui.borrow();
        assert!(!ui.sync_open.contains("local:desk"));
        assert!(ui.sync_open.contains("local:bt"), "the headset's stay open");
        assert!(ui.delay_drafts.contains_key("local:bt"), "with its draft");
        drop(ui);
        assert_eq!(delay_of(&app, "local:bt"), Some(-250.0));
    }

    #[test]
    fn enter_applies_its_rows_typed_delay_and_closes_only_its_controls() {
        let app = app_over(MockBackend::new());
        type_into_two_rows(&app, "-200");
        app.main.invoke_delay_committed("local:desk".into());
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(400.0),
            "Enter applied it"
        );
        let ui = app.ui.borrow();
        assert!(!ui.sync_open.contains("local:desk"), "its controls closed");
        assert!(ui.sync_open.contains("local:bt"), "the headset's stay open");
        assert!(ui.delay_drafts.contains_key("local:bt"), "with its draft");
        drop(ui);
        assert_eq!(delay_of(&app, "local:bt"), Some(-250.0));
    }

    #[test]
    fn a_click_on_a_control_does_only_what_the_control_does() {
        let app = app_over(MockBackend::new());
        draw_frames();
        type_into_two_rows(&app, "-200");
        let running = app.snapshot().running;
        // The streaming switch, at the right end of the header.
        click(app.main.window(), 920.0, 78.0);
        assert_ne!(
            app.snapshot().running,
            running,
            "the switch toggled streaming"
        );
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(120.0),
            "and applied no typed delay"
        );
        let ui = app.ui.borrow();
        assert_eq!(ui.sync_open.len(), 2, "the sync controls stay open");
        assert_eq!(ui.delay_drafts.len(), 2, "with their typing");
    }

    #[test]
    fn a_click_outside_every_control_applies_each_typed_delay_and_closes_every_sync_control() {
        // Between the wordmark and the streaming switch, under the left
        // column's cards, then on the blank part of a row in each speaker
        // section, which the row's own TouchArea takes.
        for (x, y) in [
            (500.0, 60.0),
            (100.0, 700.0),
            (420.0, 190.0),
            (420.0, 470.0),
        ] {
            let app = app_over(MockBackend::new());
            draw_frames();
            type_into_two_rows(&app, "abc");
            app.main.invoke_airplay_delay_edited("2100".into());
            click(app.main.window(), x, y);
            assert_eq!(delay_of(&app, "local:desk"), Some(400.0), "at {x}, {y}");
            assert_eq!(app.snapshot().airplay_delay_ms, Some(2100.0));
            assert_eq!(
                delay_of(&app, "local:bt"),
                Some(-250.0),
                "unreadable text applies nothing"
            );
            assert!(
                !row_of(&app, "local:bt").delay_pending,
                "and shows the delay in use"
            );
            let ui = app.ui.borrow();
            assert!(ui.sync_open.is_empty(), "every sync control closed");
            assert!(ui.delay_drafts.is_empty() && ui.airplay_delay_draft.draft.is_none());
        }
    }

    #[test]
    fn escape_drops_every_typed_delay_and_closes_every_sync_control() {
        let app = app_over(MockBackend::new());
        type_into_two_rows(&app, "-200");
        app.main.invoke_airplay_delay_edited("2100".into());
        app.main.invoke_delay_cancelled("local:bt".into());
        assert_eq!(delay_of(&app, "local:desk"), Some(120.0));
        assert_eq!(delay_of(&app, "local:bt"), Some(-250.0));
        assert_eq!(app.snapshot().airplay_delay_ms, Some(1750.0));
        assert_eq!(
            desk_delay_text(&app),
            "+120",
            "the field shows the delay in use"
        );
        let ui = app.ui.borrow();
        assert!(ui.sync_open.is_empty(), "every sync control closed");
        assert!(ui.delay_drafts.is_empty() && ui.airplay_delay_draft.draft.is_none());
    }

    #[test]
    fn leaving_the_delay_field_applies_nothing_and_enter_and_escape_hand_the_focus_back() {
        let app = app_over(MockBackend::new());
        let window = app.main.window();
        app.main.show().unwrap();
        let focus_desk_field = || {
            app.main.invoke_sync_toggled("local:desk".into());
            draw_frames();
            app.main.set_focus_speaker_part(RowPart::Sync);
            app.main.set_focus_speaker_id("local:desk".into());
            draw_frames();
        };
        let focus = || app.main_focus.borrow().speaker.clone();
        focus_desk_field();
        assert_eq!(
            focus(),
            Some(("local:desk".into(), RowPart::Sync)),
            "the field has it"
        );

        press(window, "4");
        press(window, "0");
        tab(window, 1);
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(120.0),
            "leaving the field applies nothing"
        );
        assert!(desk_row(&app).delay_pending, "the draft stays marked");

        press(window, Key::Escape);
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(120.0),
            "Escape applied nothing"
        );
        assert!(!app.ui.borrow().sync_open.contains("local:desk"));
        assert_eq!(
            focus(),
            Some(("local:desk".into(), RowPart::Delay)),
            "its delay button has it"
        );

        focus_desk_field();
        press(window, "9");
        press(window, Key::Return);
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(1209.0),
            "Enter applied it"
        );
        assert!(!app.ui.borrow().sync_open.contains("local:desk"));
        assert_eq!(
            focus(),
            Some(("local:desk".into(), RowPart::Delay)),
            "its delay button has it"
        );
    }

    #[test]
    fn quitting_drops_every_typed_delay_and_keeps_the_applied_ones() {
        let app = app_over(MockBackend::new());
        app.main
            .invoke_delay_edited("local:desk".into(), "400".into());
        app.main.invoke_airplay_delay_edited("2100".into());

        app.save_settings();
        assert!(app.ui.borrow().delay_drafts.is_empty(), "no draft is left");
        assert!(app.ui.borrow().airplay_delay_draft.draft.is_none());
        // A focus loss as the window goes applies nothing either.
        app.main.invoke_delay_committed("local:desk".into());
        app.main.invoke_airplay_delay_committed();
        let state = app.snapshot();
        assert_eq!(state.speaker("local:desk").unwrap().delay_ms, Some(120.0));
        assert_eq!(state.airplay_delay_ms, Some(1750.0));
    }

    #[test]
    fn quitting_with_unreadable_typing_keeps_the_applied_delay() {
        let app = app_over(MockBackend::new());
        app.main
            .invoke_delay_edited("local:desk".into(), "abc".into());
        app.main.invoke_airplay_delay_edited("".into());
        app.save_settings();
        app.main.invoke_delay_committed("local:desk".into());
        app.main.invoke_airplay_delay_committed();
        let state = app.snapshot();
        assert_eq!(state.speaker("local:desk").unwrap().delay_ms, Some(120.0));
        assert_eq!(state.airplay_delay_ms, Some(1750.0));
    }

    #[test]
    fn a_removed_speakers_typed_delay_is_dropped() {
        let app = app_over(MockBackend::new());
        app.main
            .invoke_delay_edited("local:desk".into(), "99".into());
        assert_eq!(desk_delay_text(&app), "99");

        // The desk is unplugged: its draft goes with it.
        let mut unplugged = MockBackend::new();
        unplugged.retain_speakers(|s| s.id != "local:desk");
        *app.backend.borrow_mut() = Box::new(unplugged);
        app.refresh();
        assert!(
            !app.ui.borrow().delay_drafts.contains_key("local:desk"),
            "the removed speaker's draft is gone"
        );

        // Plugged back in, it shows its saved delay.
        *app.backend.borrow_mut() = Box::new(MockBackend::new());
        app.refresh();
        assert_eq!(desk_delay_text(&app), "+120");
    }

    #[test]
    fn escape_in_the_airplay_delay_field_leaves_the_applied_delay() {
        let app = app_over(MockBackend::new());
        let applied = app.snapshot().airplay_delay_ms;
        assert_eq!(applied, Some(1750.0), "the example sender has a delay");
        let shown = || app.main.get_airplay_delay_text().to_string();
        assert_eq!(shown(), format_ms(1750.0));

        assert!(!app.main.get_airplay_delay_pending());
        app.main.invoke_airplay_delay_edited("9999".into());
        assert_eq!(shown(), "9999", "the field keeps what is typed");
        assert!(app.main.get_airplay_delay_pending(), "typing is a draft");
        assert_eq!(
            app.snapshot().airplay_delay_ms,
            applied,
            "typing applies nothing"
        );

        app.main.invoke_airplay_delay_cancelled();
        assert_eq!(shown(), format_ms(1750.0), "Escape shows the applied value");
        assert!(
            !app.main.get_airplay_delay_pending(),
            "Escape clears the draft"
        );

        app.main.invoke_airplay_delay_committed();
        assert_eq!(
            app.snapshot().airplay_delay_ms,
            applied,
            "leaving the field after Escape applies nothing"
        );
        assert_eq!(shown(), format_ms(1750.0));
    }

    #[test]
    fn the_main_window_fits_a_small_laptop_screen() {
        // A 1366 x 768 screen at 150 %, with the taskbar, leaves about
        // 911 x 464 logical pixels inside the window's frame.
        let (min_width, min_height) = crate::settings::MIN_WINDOW;
        assert!(
            min_width <= 911.0 && min_height <= 464.0,
            "{min_width} x {min_height}"
        );
        let main = include_str!("../ui/main.slint");
        // By line, so a checkout with CRLF line ends (Windows CI) reads the same.
        let has_line = |wanted: String| main.lines().any(|line| line.trim_end() == wanted);
        assert!(has_line(format!("    min-width: {min_width}px;")));
        assert!(has_line(format!("    min-height: {min_height}px;")));
        // The left column scrolls when it is taller than the window, so
        // its cards never push the window past that height.
        assert!(main.contains("left-column := ScrollView {"));
    }

    #[test]
    fn a_saved_main_window_is_fitted_to_the_monitor_it_opens_on() {
        let laptop = crate::placement::Bounds {
            x: 0.0,
            y: 0.0,
            width: 1366.0,
            height: 728.0,
        };
        let saved = WindowFrame {
            x: 300,
            y: 200,
            width: 2500.0,
            height: 1400.0,
            maximized: true,
        };
        let fitted = fitted_main_frame(saved, laptop, 1.0, true);
        assert_eq!(
            fitted,
            WindowFrame {
                x: 0,
                y: 0,
                width: 1366.0 - MAIN_WINDOW_CHROME.0,
                height: 728.0 - MAIN_WINDOW_CHROME.1,
                maximized: true,
            }
        );
        // At 150 %, the work area holds fewer logical pixels.
        let full_hd = crate::placement::Bounds {
            width: 1920.0,
            height: 1040.0,
            ..laptop
        };
        let fitted = fitted_main_frame(saved, full_hd, 1.5, true);
        assert_eq!(
            (fitted.width, fitted.height),
            (
                1280.0 - MAIN_WINDOW_CHROME.0,
                (1040.0 / 1.5 - MAIN_WINDOW_CHROME.1).round()
            )
        );
        // Never below the window's smallest size, never above the saved one.
        let tiny = crate::placement::Bounds {
            width: 800.0,
            height: 500.0,
            ..laptop
        };
        let fitted = fitted_main_frame(saved, tiny, 1.0, true);
        assert_eq!((fitted.width, fitted.height), crate::settings::MIN_WINDOW);
        // A window that fits is left exactly as saved.
        let small = WindowFrame {
            x: 120,
            y: 40,
            width: 1000.5,
            height: 600.0,
            maximized: false,
        };
        assert_eq!(fitted_main_frame(small, laptop, 1.0, true), small);
    }

    #[test]
    fn rows_show_the_speaker_state_kind_and_delay() {
        let mut backend = MockBackend::new();
        backend.set_running(true);
        let state = backend.snapshot();
        let desk = state.speaker("local:desk").unwrap();
        let row = speaker_item(&state, desk, true, true, None);
        assert_eq!(row.state, RowState::Off);
        assert_eq!(row.kind, SpeakerKind::Usb);
        assert!(row.has_delay && row.sync_open);
        assert_eq!((row.delay_ms, row.delay_text.as_str()), (120, "+120"));

        let kitchen = state.speaker("airplay:kitchen").unwrap();
        let row = speaker_item(&state, kitchen, true, true, None);
        assert_eq!(row.state, RowState::Playing);
        assert!(
            !row.has_delay && !row.sync_open,
            "AirPlay rows have no sync controls"
        );
    }

    #[test]
    fn the_reference_row_has_no_delay_and_the_others_show_a_signed_offset() {
        let state = MockBackend::new().snapshot();
        let row = |id: &str| speaker_item(&state, state.speaker(id).unwrap(), true, true, None);
        let reference = row("local:speakers");
        assert!(
            !reference.has_delay && !reference.sync_open,
            "the reference has no delay field"
        );
        let desk = row("local:desk");
        assert_eq!((desk.delay_ms, desk.delay_text.as_str()), (120, "+120"));
        assert_eq!(desk.reference_name.as_str(), "Speakers");
        let headset = row("local:bt");
        assert_eq!(
            (headset.delay_ms, headset.delay_text.as_str()),
            (-250, "−250")
        );
        let headphones = row("local:headphones");
        assert_eq!(
            (headphones.delay_ms, headphones.delay_text.as_str()),
            (0, "0")
        );
    }

    #[test]
    fn a_typed_delay_stays_in_the_field() {
        let state = MockBackend::new().snapshot();
        let desk = state.speaker("local:desk").unwrap();
        let row = speaker_item(&state, desk, true, true, Some("12"));
        assert_eq!(row.delay_text.as_str(), "12");
        assert_eq!(row.delay_ms, 120);
    }

    #[test]
    fn a_typed_airplay_delay_applies_only_when_committed() {
        // With a sender latency above 2 s the delay can't go under 2100 ms,
        // so applying "2", "25" and "250" on the way to "2500" would clamp
        // each of them.
        let shown = Some(2100.0);
        let mut field = TypedDelay::default();
        for text in ["2", "25", "250", "2500"] {
            field.edit(text, shown);
            assert_eq!(field.text(shown), Some(text), "the field keeps the text");
        }
        assert_eq!(field.commit(shown), Some(2500.0), "Enter applies it");
        assert_eq!(field.text(shown), None, "then the field shows the value");
        assert_eq!(
            field.commit(shown),
            None,
            "leaving the field applies nothing more"
        );

        field.edit("abc", shown);
        assert_eq!(field.commit(shown), None, "not a number: nothing applies");
        field.edit("3000", shown);
        assert_eq!(field.text(Some(2400.0)), None);
        assert_eq!(
            field.commit(Some(2400.0)),
            None,
            "a new sender latency replaced the typed text"
        );
    }

    #[test]
    fn escape_drops_the_typed_airplay_delay() {
        let shown = Some(2100.0);
        let mut field = TypedDelay::default();
        field.edit("9999", shown);
        field.cancel();
        assert_eq!(field.text(shown), None, "the field shows the value again");
        assert_eq!(
            field.commit(shown),
            None,
            "leaving the field after Escape applies nothing"
        );
    }

    #[test]
    fn a_listed_virtual_output_stays_listed_after_it_is_turned_off() {
        let state = MockBackend::new().snapshot();
        let mut cable = state.speaker("local:desk").unwrap().clone();
        cable.kind = ui_model::SpeakerKind::Virtual;
        cable.enabled = true;
        let mut listed = ListedVirtual::default();
        listed.note(std::slice::from_ref(&cable), false);
        assert!(listed.lists(&cable, false), "on, it is listed");

        cable.enabled = false;
        listed.note(std::slice::from_ref(&cable), false);
        assert!(
            listed.lists(&cable, false),
            "turned off, its row stays under the focus"
        );

        listed.forget();
        assert!(
            !listed.lists(&cable, false),
            "until Show Virtual Devices changes"
        );
        let mut other = cable.clone();
        other.id = "local:other".into();
        assert!(
            !listed.lists(&other, false),
            "a virtual output that was never listed stays hidden"
        );
    }

    #[test]
    fn connected_speakers_say_when_only_virtual_outputs_are_hidden() {
        let state = MockBackend::new().snapshot();
        let mut cable = state.speaker("local:desk").unwrap().clone();
        cable.kind = ui_model::SpeakerKind::Virtual;
        cable.enabled = false;
        let hiding = |s: &ui_model::Speaker| s.is_listed(false);
        let showing = |s: &ui_model::Speaker| s.is_listed(true);
        assert!(only_virtual_outputs_hidden(&[&cable], hiding));
        assert!(
            !only_virtual_outputs_hidden(&[&cable], showing),
            "shown, it is listed"
        );
        assert!(!only_virtual_outputs_hidden(&[], hiding), "nothing at all");
        let desk = state.speaker("local:desk").unwrap();
        assert!(
            !only_virtual_outputs_hidden(&[&cable, desk], hiding),
            "another speaker is listed"
        );
    }

    #[test]
    fn groups_count_their_present_members() {
        let state = MockBackend::new().snapshot();
        let row = group_item(&state, &state.groups[0]);
        assert_eq!((row.available, row.total), (2, 2));
        assert_eq!(row.state, RowState::Off);
    }

    #[test]
    fn the_flyout_lists_only_the_newest_notice() {
        let mut backend = MockBackend::new();
        backend.post("source", NoticeMessage::CaptureLost, "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        let notices = backend.snapshot().notices;
        let flyout = newest_notices(&notices, FLYOUT_NOTICE_LIMIT);
        let messages: Vec<_> = flyout.iter().map(|n| n.message.clone()).collect();
        // One row: the stack draws no Dismiss All row under it, and the
        // flyout's button opens the main window for the other notice.
        assert_eq!(messages, [NoticeMessage::NoSpeaker]);
        assert_eq!(newest_notices(&notices, MAIN_NOTICE_LIMIT).len(), 2);
    }

    #[test]
    fn the_pin_field_keeps_four_digits_at_most() {
        assert_eq!(pin_digits("4821"), "4821");
        assert_eq!(pin_digits("48a2-1"), "4821");
        assert_eq!(pin_digits("482199"), "4821");
        assert_eq!(pin_digits(" ١٢ "), "", "ASCII digits only");
        assert_eq!(pin_digits(""), "");
    }

    #[test]
    fn output_names_read_as_a_list() {
        let and = |head: &str, last: &str| format!("{head} and {last}");
        let names =
            |list: &[&str]| -> Vec<String> { list.iter().map(|name| (*name).to_owned()).collect() };
        assert_eq!(join_names(&names(&[]), and), "");
        assert_eq!(join_names(&names(&["TV"]), and), "TV");
        assert_eq!(
            join_names(&names(&["TV", "Speakers"]), and),
            "TV and Speakers"
        );
        assert_eq!(
            join_names(&names(&["TV", "Speakers", "Monitor"]), and),
            "TV, Speakers and Monitor"
        );
        // Every notice and the guide join names through it.
        let view = include_str!("view.rs");
        let comma_joins = view.matches(concat!(".join(", "\", \"", ")")).count();
        assert_eq!(comma_joins, 1, "only join_names joins names by commas");
    }

    #[test]
    fn guide_buttons_ask_for_the_page_their_step_names() {
        for page in [
            None,
            Some(SettingsPage::VolumeMixer),
            Some(SettingsPage::Sound),
        ] {
            assert_eq!(settings_page(guide_page(page)), page);
        }
        let message = guide::preview("app").unwrap();
        let pages: Vec<GuidePage> = guide::guide(&message)
            .unwrap()
            .steps
            .iter()
            .map(|step| guide_page(step.page()))
            .collect();
        assert_eq!(
            pages,
            [GuidePage::VolumeMixer, GuidePage::Sound, GuidePage::None]
        );
    }

    #[test]
    fn models_update_in_place_when_the_length_is_unchanged() {
        let model = VecModel::from(vec![1, 2, 3]);
        sync_model(&model, vec![1, 5, 3]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5, 3]);
        sync_model(&model, vec![4]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [4]);
    }

    /// The flyout with two notices (it lists the newest), one group and
    /// three speakers.
    const FLYOUT: FlyoutCounts = FlyoutCounts {
        notices: 2,
        listed_notices: 1,
        groups: 1,
        speakers: 3,
    };

    #[test]
    fn a_refresh_removes_the_flyout_lists_whose_length_changes() {
        assert_eq!(FLYOUT.removed_by(FLYOUT), FlyoutParts::default());
        let unplugged = FlyoutCounts {
            speakers: 2,
            ..FLYOUT
        };
        assert_eq!(
            FLYOUT.removed_by(unplugged),
            FlyoutParts {
                speakers: true,
                ..FlyoutParts::default()
            }
        );
        let group_added = FlyoutCounts {
            groups: 2,
            ..FLYOUT
        };
        assert!(FLYOUT.removed_by(group_added).groups, "rows are rebuilt");
    }

    #[test]
    fn dismissing_notices_removes_the_more_button_then_the_last_notice() {
        // The newest notice is dismissed: the next one takes its row, and
        // the button for the rest goes.
        let one_left = FlyoutCounts {
            notices: 1,
            ..FLYOUT
        };
        assert_eq!(
            FLYOUT.removed_by(one_left),
            FlyoutParts {
                more_notices: true,
                ..FlyoutParts::default()
            }
        );
        let none_left = FlyoutCounts {
            notices: 0,
            listed_notices: 0,
            ..FLYOUT
        };
        assert_eq!(
            one_left.removed_by(none_left),
            FlyoutParts {
                notices: true,
                ..FlyoutParts::default()
            }
        );
        assert!(!none_left.removed_by(FLYOUT).more_notices, "it came back");
    }

    #[test]
    fn the_flyout_takes_the_focus_back_only_from_a_removed_part() {
        let notice = FlyoutParts {
            notices: true,
            ..FlyoutParts::default()
        };
        let speakers = FlyoutParts {
            speakers: true,
            ..FlyoutParts::default()
        };
        assert!(notice.overlaps(notice));
        assert!(
            !speakers.overlaps(FlyoutParts {
                notices: true,
                more_notices: true,
                groups: true,
                speakers: false,
            }),
            "a focused speaker row keeps its focus when other parts change"
        );
        assert!(!FlyoutParts::default().overlaps(notice), "nothing focused");
    }

    #[test]
    fn escape_on_the_pin_dialog_buttons_cancels() {
        let app = app_over(MockBackend::new());
        app.preview_pin(false);
        let window = app.pin_dialog.window();
        assert!(window.is_visible());
        // From the field to Cancel: Pair waits for four digits.
        tab(window, 1);
        press(window, "5");
        assert_eq!(app.pin_dialog.get_pin(), "", "the field lost the focus");
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape on Cancel closed the dialog");
        assert!(!app.pin_preview.get(), "through its Cancel path");
    }

    #[test]
    fn escape_on_the_group_dialog_buttons_cancels() {
        let app = app_over(MockBackend::new());
        let groups = app.snapshot().groups.len();
        app.ask_new_group();
        let window = app.group_dialog.window();
        assert!(window.is_visible());
        assert!(app.group_dialog.get_show_field(), "a speaker is on");
        // From the name field to Create.
        tab(window, 1);
        press(window, "x");
        assert_eq!(app.group_dialog.get_name(), "", "the field lost the focus");
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape on Create closed the dialog");
        assert_eq!(app.snapshot().groups.len(), groups, "and made no group");
    }

    #[test]
    fn escape_closes_the_about_dialog() {
        let app = app_over(MockBackend::new());
        let window = app.about.window();
        app.main.invoke_about_requested();
        assert!(window.is_visible());
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape right after it opened");

        app.main.invoke_about_requested();
        assert!(window.is_visible());
        tab(window, 1);
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape on a focused button");
    }

    #[test]
    fn a_notices_dismiss_button_names_it_in_a_few_words() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let dismiss_label = |message: NoticeMessage| {
            let notice = ui_model::Notice {
                id: 1,
                subject: "test".into(),
                message,
                time: "09.40".into(),
            };
            let item = notice_item(&strings, &notice);
            strings.invoke_dismiss_notice(item.title).to_string()
        };
        assert_eq!(
            dismiss_label(NoticeMessage::HeardTwice {
                source: Some("Music Player".into()),
                mute: vec!["Speakers".into()],
                shared: vec!["Headphones".into()],
                default_output: None,
            }),
            "Dismiss: double audio"
        );
        assert_eq!(
            dismiss_label(NoticeMessage::AirPlayRetrying {
                speaker: "Kitchen".into(),
                problem: ui_model::AirPlayProblem::ConnectionLost,
            }),
            "Dismiss: Kitchen reconnecting"
        );
        // The names say what happened: the speaker was turned off, and the
        // unreadable file was kept.
        assert_eq!(
            dismiss_label(NoticeMessage::SpeakerFailed {
                speaker: "Kitchen".into(),
            }),
            "Dismiss: Kitchen didn't start, turned off"
        );
        assert_eq!(
            dismiss_label(NoticeMessage::SettingsKeptAside { file: "f".into() }),
            "Dismiss: settings unreadable, copy kept"
        );
        assert_eq!(
            dismiss_label(NoticeMessage::PairingsKeptAside { file: "f".into() }),
            "Dismiss: pairings unreadable, copy kept"
        );
        // Every kind gets a name of a few words, never the whole message.
        let messages = [
            NoticeMessage::NoSpeaker,
            NoticeMessage::SourceQuit { source: "A".into() },
            NoticeMessage::SourceNotRunning { source: "A".into() },
            NoticeMessage::SourceRefused { source: "A".into() },
            NoticeMessage::CaptureFailed,
            NoticeMessage::CaptureLost,
            NoticeMessage::SpeakerFailed {
                speaker: "A".into(),
            },
            NoticeMessage::SpeakerLost {
                speaker: "A".into(),
            },
            NoticeMessage::SpeakerBusy {
                speaker: "A".into(),
            },
            NoticeMessage::AirPlayCantPlay {
                speaker: "A".into(),
                problem: ui_model::AirPlayProblem::Refused,
            },
            NoticeMessage::AirPlayStopped,
            NoticeMessage::SettingsNotSaved,
            NoticeMessage::PairingsNotSaved,
            NoticeMessage::SettingsKeptAside { file: "f".into() },
            NoticeMessage::PairingsKeptAside { file: "f".into() },
            NoticeMessage::NoSoundOutput,
        ];
        for message in messages {
            let label = dismiss_label(message.clone());
            let words = label.split_whitespace().count();
            assert!(
                (3..=6).contains(&words),
                "{message:?}: \"{label}\" is not a short name"
            );
        }
    }

    /// The App over `backend`, with its main window shown and drawn once.
    fn main_window_over(backend: MockBackend) -> Rc<App> {
        let app = app_over(backend);
        app.main.show().expect("the main window shows headless");
        draw_frames();
        app
    }

    #[test]
    fn dismissing_a_notice_with_the_keyboard_focuses_the_next_then_the_switch() {
        let mut backend = MockBackend::new();
        backend.post("source", NoticeMessage::CaptureLost, "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        let app = main_window_over(backend);
        let window = app.main.window();
        // The streaming switch, then the newest notice's dismiss button.
        tab(window, 2);
        press(window, " ");
        assert_eq!(app.snapshot().notices.len(), 1, "Space dismissed it");
        press(window, " ");
        assert_eq!(
            app.snapshot().notices.len(),
            0,
            "the focus moved to the next notice, which Space dismissed"
        );
        assert!(!app.snapshot().running);
        press(window, " ");
        assert!(
            app.snapshot().running,
            "with no notice left, the streaming switch has the focus"
        );
    }

    /// The example speakers plus a virtual output that is off, so it is
    /// listed only while Show Virtual Devices is on.
    fn main_window_with_listed_cable() -> Rc<App> {
        let mut backend = MockBackend::new();
        let mut cable = backend.snapshot().speaker("local:desk").unwrap().clone();
        cable.id = "local:cable".into();
        cable.name = "Cable Input".into();
        cable.kind = ui_model::SpeakerKind::Virtual;
        cable.enabled = false;
        backend.add_speaker(cable);
        let app = app_over(backend);
        app.main.invoke_show_virtual_devices_toggled(true);
        app.main.show().expect("the main window shows headless");
        draw_frames();
        app
    }

    fn enabled(app: &App, id: &str) -> bool {
        app.snapshot().speaker(id).unwrap().enabled
    }

    #[test]
    fn a_focused_speaker_row_that_goes_hands_the_focus_to_the_next_rows_options() {
        let app = main_window_with_listed_cable();
        let window = app.main.window();
        // Switch, source, master volume, Equalizer, then the rows: the
        // group's, This PC's, then the connected speakers' by name.
        tab(window, 16);
        press(window, " ");
        assert!(enabled(&app, "local:cable"), "the cable row has the focus");
        press(window, " ");
        assert!(!enabled(&app, "local:cable"));

        // Off and no longer shown, its row goes: the focus moves to the
        // options of the row after it, the desk speaker's, not to a control
        // whose Space acts on every speaker.
        app.main.invoke_show_virtual_devices_toggled(false);
        draw_frames();
        app.update(|b| b.set_running(false));
        draw_frames();
        let headset = enabled(&app, "local:bt");
        tab(window, 1);
        press(window, " ");
        assert_ne!(
            enabled(&app, "local:bt"),
            headset,
            "the next stop after the desk row's options is the headset's toggle"
        );
    }

    #[test]
    fn typing_in_a_delay_field_keeps_the_focus_there_when_its_list_is_rebuilt() {
        let app = main_window_with_listed_cable();
        let window = app.main.window();
        app.main.invoke_sync_toggled("local:desk".into());
        draw_frames();
        // The cable row's toggle, delay and options, then the desk row's
        // toggle, delay, options, two steps and its field.
        tab(window, 24);
        press(window, "5");
        let draft = || {
            let shown = app.snapshot().speaker("local:desk").unwrap().delay_ms;
            app.ui
                .borrow()
                .delay_drafts
                .get("local:desk")
                .and_then(|typed| typed.text(shown).map(str::to_owned))
        };
        assert_eq!(
            draft().as_deref(),
            Some("5"),
            "the desk's field has the focus"
        );

        // The cable row goes, so Connected Speakers is rebuilt.
        app.main.invoke_show_virtual_devices_toggled(false);
        draw_frames();
        press(window, "0");
        assert_eq!(
            draft().as_deref(),
            Some("50"),
            "the rebuilt row's field has the focus, the typed text and the cursor at its end"
        );
        let applied = || app.snapshot().speaker("local:desk").unwrap().delay_ms;
        assert_eq!(applied(), Some(120.0), "typing applied nothing");
        press(window, Key::Return);
        assert!(!enabled(&app, "local:desk"), "Enter left the speaker off");
        assert_eq!(applied(), Some(50.0), "Enter applied the typed delay");
        assert!(
            !app.ui.borrow().sync_open.contains("local:desk"),
            "and closed the controls"
        );
        assert_eq!(
            app.main_focus.borrow().speaker,
            Some(("local:desk".into(), RowPart::Delay)),
            "the desk's delay button has the focus"
        );
    }

    #[test]
    fn a_focused_speaker_row_keeps_the_focus_when_its_list_is_rebuilt() {
        let app = main_window_with_listed_cable();
        let window = app.main.window();
        tab(window, 14);
        let bedroom = enabled(&app, "airplay:bedroom");
        press(window, " ");
        assert_ne!(
            enabled(&app, "airplay:bedroom"),
            bedroom,
            "it has the focus"
        );
        press(window, " ");
        assert_eq!(enabled(&app, "airplay:bedroom"), bedroom);

        // The cable row goes, so Connected Speakers is rebuilt.
        app.main.invoke_show_virtual_devices_toggled(false);
        draw_frames();
        press(window, " ");
        assert_ne!(
            enabled(&app, "airplay:bedroom"),
            bedroom,
            "the rebuilt row has the focus back"
        );
    }

    #[test]
    fn the_main_window_focus_goes_where_the_removed_control_was() {
        let notices = |ids: &[i32]| -> Vec<NoticeItem> {
            ids.iter()
                .map(|&id| NoticeItem {
                    id,
                    ..NoticeItem::default()
                })
                .collect()
        };
        let groups = |ids: &[&str]| -> Vec<GroupItem> {
            ids.iter()
                .map(|&id| GroupItem {
                    id: id.into(),
                    ..GroupItem::default()
                })
                .collect()
        };
        let lists = |n: &[i32], g: &[&str]| MainLists {
            notices: notices(n),
            groups: groups(g),
            ..MainLists::default()
        };
        let on_notice = |id| MainFocus {
            notice: Some(id),
            ..MainFocus::default()
        };
        assert_eq!(
            on_notice(2).target(&lists(&[3, 2, 1], &[]), &lists(&[3, 1], &[])),
            Some(MainFocusTarget::Notice(1)),
            "the notice that took the dismissed one's place"
        );
        assert_eq!(
            on_notice(1).target(&lists(&[2, 1], &[]), &lists(&[3, 2, 1], &[])),
            Some(MainFocusTarget::Notice(2)),
            "a new notice rebuilt the stack: the same notice"
        );
        assert_eq!(
            on_notice(1).target(&lists(&[2, 1], &[]), &lists(&[2], &[])),
            Some(MainFocusTarget::Notice(0)),
            "the last one went: the one above"
        );
        assert_eq!(
            on_notice(1).target(&lists(&[2, 1], &[]), &lists(&[2, 1], &[])),
            None,
            "updated in place, the focus stays"
        );
        let dismiss_all = MainFocus {
            dismiss_all: true,
            ..MainFocus::default()
        };
        assert_eq!(
            dismiss_all.target(&lists(&[2, 1], &[]), &lists(&[], &[])),
            Some(MainFocusTarget::StreamingSwitch)
        );

        let on_group = |id: &str, part| MainFocus {
            group: Some((id.into(), part)),
            ..MainFocus::default()
        };
        assert_eq!(
            on_group("a", RowPart::Options).target(&lists(&[], &["a", "b"]), &lists(&[], &["a"])),
            Some(MainFocusTarget::Group("a".into(), RowPart::Options)),
            "the same group, on the same part"
        );
        assert_eq!(
            on_group("b", RowPart::Toggle).target(&lists(&[], &["a", "b"]), &lists(&[], &["a"])),
            Some(MainFocusTarget::Group("a".into(), RowPart::Options)),
            "a deleted group: the options of the one before it"
        );
        assert_eq!(
            on_group("a", RowPart::Toggle).target(&lists(&[], &["a"]), &lists(&[], &[])),
            Some(MainFocusTarget::StreamingSwitch),
            "the last group went"
        );
        assert_eq!(
            MainFocus::default().target(&lists(&[1], &["a"]), &lists(&[], &[])),
            None,
            "nothing in the lists had the focus"
        );
    }

    #[test]
    fn a_removed_speaker_row_hands_the_focus_to_a_neighbours_options() {
        let speakers = |ids: &[&str]| -> Vec<SpeakerItem> {
            ids.iter()
                .map(|&id| SpeakerItem {
                    id: id.into(),
                    ..SpeakerItem::default()
                })
                .collect()
        };
        let lists = |this_pc: &[&str], connected: &[&str]| MainLists {
            this_pc: speakers(this_pc),
            connected: speakers(connected),
            ..MainLists::default()
        };
        let on = |id: &str, part| MainFocus {
            speaker: Some((id.into(), part)),
            ..MainFocus::default()
        };
        let options = |id: &str| Some(MainFocusTarget::Speaker(id.into(), RowPart::Options));
        let before = lists(&["pc"], &["a", "b", "c"]);
        assert_eq!(
            on("b", RowPart::Toggle).target(&before, &lists(&["pc"], &["a", "c"])),
            options("c"),
            "the next row"
        );
        assert_eq!(
            on("c", RowPart::Volume).target(&before, &lists(&["pc"], &["a", "b"])),
            options("b"),
            "the last row went: the one before it"
        );
        assert_eq!(
            on("b", RowPart::Toggle).target(&before, &lists(&["pc"], &["a", "b", "c", "d"])),
            Some(MainFocusTarget::Speaker("b".into(), RowPart::Toggle)),
            "a rebuilt row keeps the focus on its part"
        );
        assert_eq!(
            on("b", RowPart::Sync).target(&before, &lists(&["pc"], &["a", "c"])),
            options("c"),
            "the sync controls went with their row"
        );
        assert_eq!(
            on("b", RowPart::Sync).target(&before, &lists(&["pc", "x"], &["a", "b", "c"])),
            None,
            "only the other list changed"
        );
        assert_eq!(
            on("pc", RowPart::Toggle).target(&before, &lists(&[], &["a", "b", "c"])),
            options("a"),
            "its list is empty now: the other list's first row"
        );
        assert_eq!(
            on("a", RowPart::Toggle).target(&lists(&["pc"], &["a"]), &lists(&["pc"], &[])),
            options("pc"),
            "the same from the other list"
        );
        let with_group = |this_pc: &[&str], connected: &[&str]| MainLists {
            groups: vec![GroupItem {
                id: "g".into(),
                ..GroupItem::default()
            }],
            ..lists(this_pc, connected)
        };
        assert_eq!(
            on("pc", RowPart::Volume).target(&with_group(&["pc"], &[]), &with_group(&[], &[])),
            Some(MainFocusTarget::Group("g".into(), RowPart::Options)),
            "no speaker row left: the first group's options"
        );
        assert_eq!(
            on("pc", RowPart::Toggle).target(&lists(&["pc"], &[]), &lists(&[], &[])),
            Some(MainFocusTarget::StreamingSwitch),
            "no row left at all"
        );
    }

    #[test]
    fn the_last_row_of_a_list_hands_the_focus_to_the_other_lists_first_row() {
        // This PC's two outputs, and under Connected Speakers only a
        // virtual output that is off, listed while Show Virtual Devices is
        // on.
        let mut backend = MockBackend::new();
        backend.retain_speakers(|s| s.kind.is_this_pc());
        backend.delete_group("group:1");
        let mut cable = backend
            .snapshot()
            .speaker("local:headphones")
            .unwrap()
            .clone();
        cable.id = "local:cable".into();
        cable.name = "Cable Input".into();
        cable.kind = ui_model::SpeakerKind::Virtual;
        cable.enabled = false;
        backend.add_speaker(cable);
        let app = app_over(backend);
        app.main.invoke_show_virtual_devices_toggled(true);
        app.main.show().expect("the main window shows headless");
        draw_frames();
        let window = app.main.window();
        // As in the example's order without the group's row and Bedroom's:
        // the cable row's toggle.
        tab(window, 12);
        press(window, " ");
        assert!(enabled(&app, "local:cable"), "the cable row has the focus");
        press(window, " ");
        assert!(!enabled(&app, "local:cable"));

        // Off and no longer shown, Connected Speakers is empty: the focus
        // goes to This PC's first row's options, so the next Tab stop is
        // the headphones' toggle, not the source after the streaming switch.
        app.main.invoke_show_virtual_devices_toggled(false);
        draw_frames();
        let headphones = enabled(&app, "local:headphones");
        tab(window, 1);
        press(window, " ");
        assert_ne!(
            enabled(&app, "local:headphones"),
            headphones,
            "the next stop after the speakers' options is the headphones' toggle"
        );
    }

    /// What the announcer makes of `notices`, given as (id, subject, title).
    fn announce_for(announcer: &mut NoticeAnnouncer, notices: &[(u64, &str, &str)]) -> String {
        let listed: Vec<Listed> = notices
            .iter()
            .map(|&(id, subject, title)| Listed {
                id,
                subject: subject.into(),
                title: title.into(),
            })
            .collect();
        match announcer.next(&listed) {
            Announcement::Say(title) => format!("say {title}"),
            Announcement::Clear => "clear".into(),
            Announcement::Keep => "keep".into(),
        }
    }

    /// Only a notice posted after launch is news, once: the newest one when
    /// several arrive together, and again after the list was emptied.
    #[test]
    fn the_announcer_picks_each_new_notice_once() {
        let mut a = NoticeAnnouncer::default();
        let launch = [(2, "b", "B"), (1, "a", "A")];
        assert_eq!(announce_for(&mut a, &launch), "keep", "there at launch");
        assert_eq!(announce_for(&mut a, &launch), "keep");
        let three = [(3, "c", "C"), (2, "b", "B"), (1, "a", "A")];
        assert_eq!(announce_for(&mut a, &three), "say C", "a new one");
        assert_eq!(announce_for(&mut a, &three), "keep", "said once");
        let two = [(5, "e", "E"), (4, "d", "D"), (3, "c", "C")];
        assert_eq!(announce_for(&mut a, &two), "say E", "the newest of two");
        assert_eq!(announce_for(&mut a, &[]), "clear", "all dismissed");
        assert_eq!(announce_for(&mut a, &[(6, "f", "F")]), "say F");
    }

    /// A notice posted again for the same subject with the same title (an
    /// AirPlay speaker that keeps retrying) is not news; a new title for
    /// that subject is, and so is the same notice after it was dismissed.
    #[test]
    fn a_repost_of_the_same_notice_is_not_announced_again() {
        let mut a = NoticeAnnouncer::default();
        assert_eq!(announce_for(&mut a, &[]), "keep");
        let retrying = |id| [(id, "airplay:kitchen", "Kitchen reconnecting")];
        assert_eq!(
            announce_for(&mut a, &retrying(1)),
            "say Kitchen reconnecting"
        );
        assert_eq!(announce_for(&mut a, &retrying(2)), "keep", "a retry");
        assert_eq!(announce_for(&mut a, &retrying(3)), "keep", "another");
        let cant = |id| [(id, "airplay:kitchen", "Kitchen can't play")];
        assert_eq!(announce_for(&mut a, &cant(4)), "say Kitchen can't play");
        assert_eq!(announce_for(&mut a, &[]), "clear", "dismissed");
        assert_eq!(announce_for(&mut a, &cant(5)), "say Kitchen can't play");
    }

    /// The live region empties as soon as the notice it names is gone, so
    /// Narrator's scan mode finds no stray announcement; other notices
    /// leaving keep it.
    #[test]
    fn the_announcement_clears_when_its_notice_leaves() {
        let mut a = NoticeAnnouncer::default();
        assert_eq!(announce_for(&mut a, &[(1, "a", "A")]), "keep");
        let both = [(2, "b", "B"), (1, "a", "A")];
        assert_eq!(announce_for(&mut a, &both), "say B");
        assert_eq!(announce_for(&mut a, &[(2, "b", "B")]), "keep", "A left");
        assert_eq!(announce_for(&mut a, &[]), "clear", "B left");
        assert_eq!(announce_for(&mut a, &[]), "keep");
    }

    /// A new notice is announced by its title in the window Narrator can
    /// hear: the open flyout, else the main window; never in both. It goes
    /// once the notice is dismissed.
    #[test]
    fn a_new_notice_is_announced_in_the_flyout_or_the_main_window() {
        let mut backend = MockBackend::new();
        backend.post("source", NoticeMessage::CaptureLost, "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        let app = app_over(backend);
        assert_eq!(app.main.get_notice_announcement(), "", "not at launch");
        assert_eq!(app.flyout.get_notice_announcement(), "");

        let strings = app.strings();
        let title = notice_title(&strings, &NoticeMessage::NoSpeaker);
        let said = strings.invoke_notice_announcement(title);
        // As if only the first notice had been there at launch.
        let only_the_first = || NoticeAnnouncer {
            seen: Some(1),
            ..NoticeAnnouncer::default()
        };
        *app.notice_announcer.borrow_mut() = only_the_first();
        app.refresh();
        assert_eq!(app.main.get_notice_announcement(), said);
        assert_eq!(app.flyout.get_notice_announcement(), "");

        app.clear_announcement();
        assert_eq!(app.main.get_notice_announcement(), "");
        app.flyout.show().expect("the flyout shows headless");
        *app.notice_announcer.borrow_mut() = only_the_first();
        app.refresh();
        assert_eq!(app.flyout.get_notice_announcement(), said);
        assert_eq!(app.main.get_notice_announcement(), "", "not twice");

        app.update(|backend| backend.dismiss_all_notices());
        assert_eq!(app.flyout.get_notice_announcement(), "", "dismissed");
    }

    /// A character outside the Basic Multilingual Plane (two UTF-16 units)
    /// across the cut is left out whole, never split.
    #[test]
    fn the_tooltip_cut_never_splits_a_surrogate_pair() {
        // The emoji takes units 126 and 127 (1-based); 126 fit before "…".
        let text = format!("{}\u{1F50A}{}", "a".repeat(125), "b".repeat(10));
        let cut = fit_tooltip(&text);
        assert_eq!(cut, format!("{}…", "a".repeat(125)));
        assert_eq!(cut.encode_utf16().count(), 126);
        // One unit earlier, it fits whole.
        let text = format!("{}\u{1F50A}{}", "a".repeat(124), "b".repeat(10));
        let cut = fit_tooltip(&text);
        assert_eq!(cut, format!("{}\u{1F50A}…", "a".repeat(124)));
        assert_eq!(cut.encode_utf16().count(), 127);
    }

    /// The tray icon's tooltip names the newest notice on a second line while
    /// there is one, and fits the 127 characters Windows shows.
    #[test]
    fn the_tray_tooltip_follows_the_newest_notice() {
        let mut backend = MockBackend::new();
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let plain = strings.invoke_tray_tooltip(false, 0).to_string();
        assert_eq!(tray_tooltip(&strings, &backend.snapshot()), plain);

        backend.post("source", NoticeMessage::CaptureLost, "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        let title = notice_title(&strings, &NoticeMessage::NoSpeaker);
        assert_eq!(
            tray_tooltip(&strings, &backend.snapshot()),
            format!("{plain}\nNotice: {title}")
        );

        backend.dismiss_all_notices();
        assert_eq!(tray_tooltip(&strings, &backend.snapshot()), plain);

        backend.post(
            "speaker",
            NoticeMessage::SpeakerFailed {
                speaker: "Kitchen ".repeat(30),
            },
            "09.42",
        );
        let long = tray_tooltip(&strings, &backend.snapshot());
        assert!(long.encode_utf16().count() <= 127, "{long}");
        assert!(long.ends_with('…'), "{long}");
        assert!(long.starts_with(&format!("{plain}\nNotice: Kitchen")));

        // The tray shows this tooltip, not the plain one.
        let tray = include_str!("tray.rs");
        assert!(!tray.contains("invoke_tray_tooltip("));
        assert_eq!(tray.matches("view::tray_tooltip(").count(), 2);
    }
}
