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

use audiosync_core::calibrate_all::{
    AirPlayMoves, AirPlayShared, Class, EndReason, Measurement, RowState as SheetRowState,
    SharedState,
};
use audiosync_core::channels::InputPair;
use audiosync_core::eq::{self, CurveMatch, EqSettings};
use audiosync_core::meter;
use audiosync_core::policy::device_volume::Hint;
use audiosync_core::policy::presets::new_preset_name;
use audiosync_core::policy::source_quit::WhenSourceQuits;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

mod cast_address;
mod password;

use crate::audio::backend::AirPlayGroupKind;
use crate::audio::calibration::{self, Failure, Missing, Phase, Step, Which};
use crate::backdrop::Frame;
use crate::debug_log::DebugLog;
use crate::guide::{self, Guide, GuideStep, SettingsPage};
use crate::launch_at_login::{self, LaunchAtLogin};
use crate::now_playing_card::{CardSource, MediaControls, NowPlayingCard, Transport};
use crate::ui_model::{
    self, AirPlayWait, AllPhase, AllProgress, AppState, Backend, LeftOutRow, MAX_PRESETS,
    MainWindowShows, NoticeMessage, PinPrompt, PresetError, PresetLeftOff, SheetRow, SheetState,
    SourceKind, SourceName, SpeakerState, SwitchName, ViewSettings, WindowFrame,
};
use crate::{
    AboutDialog, AirPlayProblem, Availability, CalibrationRowItem, CastAddressDialog, CastList,
    CastProblem, CastState, DeviceVolumeHint, EqualizerWindow, GuidePage, GuideStepItem,
    GuideWindow, HiddenItem, LeftOutItem, MainControl, MainWindow, MixAppItem, NameField,
    NoticeItem, NoticeKind, Palette, PasswordDialog, PinDialog, PinPhase, PresetItem, RowPart,
    RowState, SpeakerGroup, SpeakerItem, SpeakerKind, Strings, Tokens, TrayFlyout, UpdateButton,
    ZoneItem, ZoneNameDialog, ZoneTarget, speaker_names,
};

mod help_window;
mod remote_window;

/// The digits the PIN field keeps of what was typed: four at most.
fn pin_digits(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).take(4).collect()
}

/// What the zone name dialog is open for.
#[derive(Clone, Debug, PartialEq)]
enum DialogPurpose {
    /// New Zone from Active Speakers.
    NewZone,
    /// New Zone… in a speaker's menu, by speaker ID.
    NewZoneWith(String),
    /// Only a message (no name field), e.g. why a zone can't be made yet.
    Message,
    /// Save as Preset….
    SavePreset,
    /// Rename… on a preset, by preset ID.
    RenamePreset(String),
    /// Save Curve in the equalizer.
    SaveCurve,
    /// Rename in the equalizer, for the saved curve of this name.
    RenameCurve(String),
}

/// Where a name is edited: a speaker's full row (in its section or under
/// its zone), its In zones row, or a zone's header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RenamePlace {
    #[default]
    Row,
    InZones,
    Zone,
}

/// UI state the backend doesn't keep: which sync controls are open, what is
/// being typed in their fields, and whether virtual outputs are listed.
#[derive(Default)]
struct UiState {
    sync_open: HashSet<String>,
    /// What is typed in each speaker's delay field, by speaker ID.
    delay_drafts: HashMap<String, TypedDelay>,
    airplay_delay_draft: TypedDelay,
    /// The speaker whose name is being edited in its row, and what is typed
    /// there; one at a time (spec S228).
    rename: Option<(String, String)>,
    /// Where the open rename's field shows.
    rename_place: RenamePlace,
    /// The open rename's field has had the focus; only then does the focus
    /// moving to another control apply the name.
    rename_focused: bool,
    /// View > Show Virtual Devices; off at launch, like the Mac's default.
    show_virtual: bool,
    /// View > Appearance as chosen, which the theme preview doesn't change.
    appearance: ui_model::Appearance,
    /// File > Web Remote.
    remote: crate::remote::RemoteSettings,
    /// File > AirPlay 2 (Experimental) as saved, which applies at the next
    /// launch.
    airplay2_experimental: bool,
    /// What the item was at launch: the mode the running sender has.
    airplay2_at_launch: bool,
    /// File > Check for Updates Automatically, as saved.
    check_updates_automatically: bool,
    /// The last update check that read the appcast, Unix seconds.
    last_update_check: Option<u64>,
    /// The version the updater stopped offering at launch (spec S316).
    update_record: Option<ui_model::UpdateRecord>,
    /// Says the item applies at the next launch, while it differs from
    /// `airplay2_at_launch`.
    airplay2_notice: Option<ui_model::Notice>,
    listed_virtual: ListedVirtual,
    /// The zones whose speakers are hidden by their arrow, by ID; a zone
    /// opens expanded.
    collapsed: HashSet<String>,
    /// Whether the source was a mix at the last refresh.
    source_was_mix: bool,
    /// The equalizer curve last picked or saved, so the picker tells it from
    /// a curve with the same gains (spec S270).
    chosen_curve: Option<CurveMatch>,
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
    /// Keeps `text` if the field takes it (`ui_model::delay_field_text`);
    /// otherwise the field goes back to what it showed before.
    fn edit(&mut self, text: &str, shown: Option<f64>) {
        if let Some(text) = ui_model::delay_field_text(text) {
            self.draft = Some((text.to_string(), shown));
        }
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
    /// The chosen input device's channel pairs; empty up to two channels.
    input_pairs: Rc<VecModel<SharedString>>,
    mix_apps: Rc<VecModel<MixAppItem>>,
    this_pc: Rc<VecModel<SpeakerItem>>,
    connected: Rc<VecModel<SpeakerItem>>,
    /// The compact rows of the speakers in a zone, under their own section.
    this_pc_zoned: Rc<VecModel<SpeakerItem>>,
    connected_zoned: Rc<VecModel<SpeakerItem>>,
    /// The flyout's speakers in no zone.
    all_speakers: Rc<VecModel<SpeakerItem>>,
    zones: Rc<VecModel<ZoneItem>>,
    /// Each zone's rows by zone ID, kept between refreshes so an unchanged
    /// zone's item compares equal and its rows aren't rebuilt.
    zone_models: RefCell<HashMap<String, ZoneModels>>,
    notices: Rc<VecModel<NoticeItem>>,
    flyout_notices: Rc<VecModel<NoticeItem>>,
    gains: Rc<VecModel<f32>>,
    presets: Rc<VecModel<SharedString>>,
    /// The speaker presets, in the main window and the flyout.
    speaker_presets: Rc<VecModel<PresetItem>>,
    calibration_rows: Rc<VecModel<CalibrationRowItem>>,
    calibration_left_out: Rc<VecModel<LeftOutItem>>,
}

/// A zone's member rows and its Use … Instead entries.
#[derive(Default)]
struct ZoneModels {
    members: Rc<VecModel<SpeakerItem>>,
    targets: Rc<VecModel<ZoneTarget>>,
}

/// A zone as one refresh builds it, before its rows go into its models.
struct ZoneRowsBuilt {
    item: ZoneItem,
    members: Vec<SpeakerItem>,
    targets: Vec<ZoneTarget>,
}

/// The shape of the zones' member lists: it changes when any list's length
/// does, which rebuilds that list's rows ([`sync_model`]).
fn zone_shape(lengths: impl Iterator<Item = usize>) -> u64 {
    lengths.fold(1, |shape, length| {
        shape.wrapping_mul(31).wrapping_add(length as u64 + 1)
    })
}

/// How many member rows a zone lists under its header: none while its
/// speakers are hidden, so folding a zone counts as removing them.
fn listed_members(expanded: bool, members: usize) -> usize {
    if expanded { members } else { 0 }
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
#[cfg(any(windows, target_os = "linux", test))]
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

/// How often a running calibration is moved on: the signal waits for the
/// microphone's first frames, and each run ends once its recording is in.
const CALIBRATION_TICK: Duration = Duration::from_millis(20);

/// How often Launch at Sign-In is read again from the registry, so a change
/// in Task Manager shows in the menus (Slint's menu bar says nothing before
/// it opens). One small registry read.
const LOGIN_CHECK: Duration = Duration::from_secs(2);

/// The Settings page where Windows turns startup apps on or off.
const STARTUP_APPS_PAGE: &str = "ms-settings:startupapps";

/// The AirPlay 2 (Experimental) notice's id, next to Launch at Sign-In's
/// and as far from the backend's.
const AIRPLAY2_NOTICE_ID: u64 = launch_at_login::NOTICE_ID - 1;

/// The web remote's Public network notice's id, next to those.
const REMOTE_PUBLIC_NOTICE_ID: u64 = AIRPLAY2_NOTICE_ID - 1;

/// The Windows updater's notice's id, next to those.
const UPDATER_NOTICE_ID: u64 = REMOTE_PUBLIC_NOTICE_ID - 1;

/// How often, 20 ms apart, the saved main window size waits for the native
/// window before it gives up (about two seconds).
const NATIVE_WINDOW_TRIES: u8 = 100;

/// Roughly what the main window's outer frame adds to its inner size, in
/// logical pixels: the borders, the title bar and the menu bar. Used to fit
/// a saved size to a monitor, where a little extra room does no harm.
const MAIN_WINDOW_CHROME: (f64, f64) = (16.0, 64.0);

/// The parts of the tray flyout that a refresh can remove, with the controls
/// in them: the listed notice, the button for more notices, the zone rows
/// and the speaker rows (a zone's members among them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FlyoutParts {
    notices: bool,
    more_notices: bool,
    zones: bool,
    speakers: bool,
}

impl FlyoutParts {
    /// Whether a part is set in both.
    fn overlaps(self, other: FlyoutParts) -> bool {
        (self.notices && other.notices)
            || (self.more_notices && other.more_notices)
            || (self.zones && other.zones)
            || (self.speakers && other.speakers)
    }
}

/// What the tray flyout shows: how many notices there are and how many of
/// them it lists, how many zones and speakers in no zone, and the shape of
/// the zones' member lists ([`zone_shape`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FlyoutCounts {
    notices: usize,
    listed_notices: usize,
    zones: usize,
    speakers: usize,
    zone_members: u64,
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
            zones: self.zones != next.zones,
            speakers: self.speakers != next.speakers || self.zone_members != next.zone_members,
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
    /// A zone header's control: the zone id and the header's part.
    zone: Option<(SharedString, RowPart)>,
    /// An In zones row's control: the speaker id and the row's part.
    in_zones: Option<(SharedString, RowPart)>,
    /// Another main-window control: the streaming switch, the source, the
    /// transport, the master volume, the equalizer or calibration card or
    /// the AirPlay delay.
    control: Option<MainControl>,
    /// The speaker Hide Speaker is taking out: its neighbour's switch takes
    /// the focus, since only that row was meant to go.
    hiding: Option<SharedString>,
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
    /// A speaker row or zone header, by id, on this part (the row falls
    /// back to a harmless part when it hasn't that one).
    Speaker(SharedString, RowPart),
    Zone(SharedString, RowPart),
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
/// Speakers', then the first zone header's (a row's list is empty, so its
/// own list adds nothing); the streaming switch when every list is empty.
fn first_row(new: &MainLists) -> MainFocusTarget {
    let speaker = new.this_pc.first().or_else(|| new.connected.first());
    match (speaker, new.zones.first()) {
        (Some(speaker), _) => MainFocusTarget::Speaker(speaker.id.clone(), RowPart::Options),
        (None, Some(zone)) => MainFocusTarget::Zone(zone.id.clone(), RowPart::Options),
        (None, None) => MainFocusTarget::StreamingSwitch,
    }
}

impl MainFocus {
    /// Whether the name field of `id` in `place` has the focus.
    fn holds_name(&self, place: RenamePlace, id: &str) -> bool {
        let slot = match place {
            RenamePlace::Row => &self.speaker,
            RenamePlace::InZones => &self.in_zones,
            RenamePlace::Zone => &self.zone,
        };
        matches!(slot, Some((held, RowPart::Name)) if held.as_str() == id)
    }

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
        // The zones' cards are all rebuilt when their number changes.
        let zones_rebuilt = old.zones.len() != new.zones.len();
        if let Some((id, part)) = &self.speaker {
            let none: &[SpeakerItem] = &[];
            let mut lists: Vec<(&[SpeakerItem], &[SpeakerItem], Option<&SharedString>)> = vec![
                (&old.this_pc, &new.this_pc, None),
                (&old.connected, &new.connected, None),
            ];
            for zone in &old.zones {
                let rows = new
                    .zones
                    .iter()
                    .find(|z| z.id == zone.id)
                    .map_or(none, |z| &z.rows[..]);
                lists.push((&zone.rows, rows, Some(&zone.id)));
            }
            for (old_rows, new_rows, zone) in lists {
                let Some(at) = old_rows.iter().position(|s| s.id == *id) else {
                    continue;
                };
                if old_rows.len() == new_rows.len() && !(zone.is_some() && zones_rebuilt) {
                    break;
                }
                if new.rows().any(|s| s.id == *id) {
                    return Some(MainFocusTarget::Speaker(id.clone(), *part));
                }
                let part = if self.hiding.as_ref() == Some(id) {
                    RowPart::Toggle
                } else {
                    RowPart::Options
                };
                // A zone's last row (or every row, collapsed): its header.
                let header = zone
                    .filter(|zone| new.zones.iter().any(|z| z.id == **zone))
                    .map(|zone| MainFocusTarget::Zone(zone.clone(), RowPart::Options));
                return Some(neighbour(old_rows, new_rows, at, |s| &s.id).map_or_else(
                    || header.unwrap_or_else(|| first_row(new)),
                    |next| MainFocusTarget::Speaker(next, part),
                ));
            }
        }
        if let Some((id, part)) = &self.zone
            && zones_rebuilt
            && let Some(at) = old.zones.iter().position(|z| z.id == *id)
        {
            return Some(if new.zones.iter().any(|z| z.id == *id) {
                MainFocusTarget::Zone(id.clone(), *part)
            } else {
                neighbour(&old.zones, &new.zones, at, |z| &z.id).map_or_else(
                    || first_row(new),
                    |next| MainFocusTarget::Zone(next, RowPart::Options),
                )
            });
        }
        None
    }
}

/// A zone in the main window's lists: its ID and the member rows it shows
/// (none while it is collapsed).
#[derive(Debug, Default)]
struct ZoneRows {
    id: SharedString,
    rows: Vec<SpeakerItem>,
}

/// The main window's lists that a refresh can rebuild.
#[derive(Debug, Default)]
struct MainLists {
    notices: Vec<NoticeItem>,
    this_pc: Vec<SpeakerItem>,
    connected: Vec<SpeakerItem>,
    zones: Vec<ZoneRows>,
}

impl MainLists {
    /// Every speaker row, in zones or not.
    fn rows(&self) -> impl Iterator<Item = &SpeakerItem> {
        self.this_pc
            .iter()
            .chain(&self.connected)
            .chain(self.zones.iter().flat_map(|zone| &zone.rows))
    }
}

/// Runs after every refresh with the new state.
type RefreshHook = Box<dyn Fn(&AppState)>;

/// The application's windows and the backend behind them.
pub struct App {
    backend: RefCell<Box<dyn Backend>>,
    pub main: MainWindow,
    pub flyout: TrayFlyout,
    pub equalizer: EqualizerWindow,
    zone_dialog: ZoneNameDialog,
    /// The PIN prompt of an Apple TV pairing.
    pin_dialog: PinDialog,
    /// The speaker the PIN dialog is open for, while it is.
    pin_for: RefCell<Option<String>>,
    /// The PIN dialog shows a made-up pairing (the PIN preview), which no
    /// refresh closes.
    pin_preview: Cell<bool>,
    /// The password prompt of a speaker protected by a password.
    password_dialog: PasswordDialog,
    /// The speaker the password dialog is open for, while it is.
    password_for: RefCell<Option<String>>,
    /// Add by Address…, for a Google Cast speaker discovery can't find.
    cast_address_dialog: CastAddressDialog,
    /// The answer of the address check Add by Address… is waiting for.
    cast_check: RefCell<Option<std::sync::mpsc::Receiver<cast_address::CheckAnswer>>>,
    /// Looks for that answer while the check runs.
    cast_check_timer: Timer,
    about: AboutDialog,
    guide: GuideWindow,
    /// The paragraphs the guide shows.
    guide_steps: Rc<VecModel<GuideStepItem>>,
    /// The guide is open for the double-audio notice and follows it (not
    /// for a made-up preview notice).
    guide_follows_notice: Cell<bool>,
    /// The AudioSync Guide (Help > AudioSync Guide), `help_window.rs`.
    help: crate::HelpWindow,
    /// File > Web Remote, `remote_window.rs`.
    remote_window: crate::RemoteWindow,
    remote: RefCell<remote_window::RemoteHost>,
    log: DebugLog,
    ui: RefCell<UiState>,
    /// What has the keyboard focus in the main window's lists.
    main_focus: RefCell<MainFocus>,
    dialog_purpose: RefCell<DialogPurpose>,
    models: Models,
    /// With a tray icon, closing the main window keeps the app running there.
    tray_available: Cell<bool>,
    /// The appearance every window shows: View > Appearance, or the theme
    /// preview's.
    appearance: Cell<ui_model::Appearance>,
    /// False until the first Light or Dark: until then the windows follow
    /// Windows by themselves, after it AudioSync sets their palette and
    /// frame, for Match System too.
    themed_by_hand: Cell<bool>,
    menu_bar_theme: MenuBarTheme,
    /// Whether each window got its Windows 11 frame, as last logged.
    frames: RefCell<HashMap<&'static str, bool>>,
    /// Called after every refresh, e.g. to update the tray icon and tooltip.
    on_refresh: RefCell<Option<RefreshHook>>,
    /// Which new notice Narrator is told about.
    notice_announcer: RefCell<NoticeAnnouncer>,
    /// The calibration ending Narrator was last told about.
    calibration_announced: RefCell<Option<(String, Phase)>>,
    /// The Calibrate All ending Narrator was last told about.
    calibrate_all_announced: Cell<u64>,
    /// The delay fields holding a number out of range, so Narrator hears
    /// the range once as a field goes out of it (`newly_out_of_range`).
    out_of_range: RefCell<HashSet<String>>,
    /// Clears the announcement a while after it was made.
    announcement_timer: Timer,
    /// Polls the backend; it stops when the app is dropped.
    poll_timer: Timer,
    /// Moves a microphone calibration on while one holds the outputs.
    calibration_timer: Timer,
    /// Moves the input level meter while an input device is captured.
    meter_timer: Timer,
    /// The meter's length as last shown, 0 to 1.
    meter_level: Cell<f32>,
    /// Launch at Sign-In and its notice, which the windows own: the
    /// backend's notices are about audio.
    login: RefCell<LaunchAtLogin>,
    login_timer: Timer,
    /// The Windows updater, in a release build only.
    #[cfg(windows)]
    updater: RefCell<Option<crate::updater::Updater>>,
    /// The Now Playing card, and the source it was last given.
    now_playing: RefCell<NowPlayingCard>,
    now_playing_source: RefCell<Option<CardSource>>,
}

/// The main window's native window and the menu bar theme last sent to it.
/// muda answers that message by toggling the window's activation, which comes
/// back as a focus that applies the frame again, so it is sent only when it
/// changed.
#[derive(Default)]
struct MenuBarTheme(Cell<Option<(isize, bool)>>);

impl MenuBarTheme {
    /// Records `dark` for native window `id`; `true` when the message is due.
    fn due(&self, id: isize, dark: bool) -> bool {
        self.0.replace(Some((id, dark))) != Some((id, dark))
    }

    /// Makes the next `due` true, after Windows gave the menu bar its own theme.
    fn forget(&self) {
        self.0.set(None);
    }
}

/// Runs `action` on the app if it still exists.
#[cfg(windows)]
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn with_app(app: &Weak<App>, action: impl FnOnce(&App)) {
    if let Some(app) = app.upgrade() {
        action(&app);
    }
}

/// Applies the open rename once the focus has left its field for another
/// control, outside the focus callback, which must not rebuild the list.
fn apply_rename_left(app: &Weak<App>) {
    if let Some(id) = app.upgrade().and_then(|app| app.focus_left_rename()) {
        let app = app.clone();
        Timer::single_shot(Duration::ZERO, move || {
            with_app(&app, |app| app.end_rename_in(&id, true));
        });
    }
}

/// Asks the row or zone header where a name is edited to take the keyboard
/// focus on `part`.
fn focus_row(main: &MainWindow, place: RenamePlace, id: SharedString, part: RowPart) {
    match place {
        RenamePlace::Row => {
            main.set_focus_speaker_part(part);
            main.set_focus_speaker_id(id);
        }
        RenamePlace::InZones => {
            main.set_focus_in_zones_part(part);
            main.set_focus_in_zones_id(id);
        }
        RenamePlace::Zone => {
            main.set_focus_zone_part(part);
            main.set_focus_zone_id(id);
        }
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
            keep_main_min_size(window);
            window.set_size(size);
            if maximized {
                window.set_maximized(true);
            }
        } else if tries > 1 {
            set_size_once_native(main, size, maximized, tries - 1);
        }
    });
}

/// What a titled window does on gaining the focus besides taking its frame.
#[derive(Clone, Copy, PartialEq)]
enum Titled {
    Window,
    /// Keeps only the close button and fits the PIN dialog.
    Dialog,
    /// Holds `settings::MIN_WINDOW`.
    Main,
}

/// Holds the main window at `settings::MIN_WINDOW` natively (a MoveWindow
/// from another program shrank it to 286 x 193), growing it first if it is
/// smaller.
fn keep_main_min_size(window: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::dpi::LogicalSize;

    let (width, height) = crate::settings::MIN_WINDOW;
    window.with_winit_window(|window| {
        window.set_min_inner_size(Some(LogicalSize::new(width, height)));
    });
    crate::platform::hold_min_size(window, crate::settings::MIN_WINDOW);
}

/// Where the main window moves before it shows again: a place on a monitor
/// that is gone since it hid gets the launch's place, centered in the
/// `primary` monitor's work area and fitted to it. `None` when it stays
/// where it is: while it shows, or while minimized, when Windows parks it off
/// every monitor.
fn place_to_show(
    frame: WindowFrame,
    on_a_monitor: bool,
    minimized: bool,
    visible: bool,
    primary: Option<(crate::placement::Bounds, f64)>,
) -> Option<WindowFrame> {
    if on_a_monitor || minimized || visible {
        return None;
    }
    let (work_area, scale) = primary?;
    Some(fitted_main_frame(frame, work_area, scale, false))
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
        #[cfg(windows)]
        let log_for_updater = log.clone();
        crate::platform::set_min_size_log(log.clone());
        let models = Models {
            sources: Rc::new(VecModel::default()),
            input_pairs: Rc::new(VecModel::default()),
            mix_apps: Rc::new(VecModel::default()),
            this_pc: Rc::new(VecModel::default()),
            this_pc_zoned: Rc::new(VecModel::default()),
            connected_zoned: Rc::new(VecModel::default()),
            connected: Rc::new(VecModel::default()),
            all_speakers: Rc::new(VecModel::default()),
            zones: Rc::new(VecModel::default()),
            zone_models: RefCell::default(),
            notices: Rc::new(VecModel::default()),
            flyout_notices: Rc::new(VecModel::default()),
            gains: Rc::new(VecModel::default()),
            presets: Rc::new(VecModel::default()),
            speaker_presets: Rc::new(VecModel::default()),
            calibration_rows: Rc::new(VecModel::default()),
            calibration_left_out: Rc::new(VecModel::default()),
        };
        let app = Rc::new(App {
            backend: RefCell::new(backend),
            main: MainWindow::new()?,
            flyout: TrayFlyout::new()?,
            equalizer: EqualizerWindow::new()?,
            zone_dialog: ZoneNameDialog::new()?,
            pin_dialog: PinDialog::new()?,
            pin_for: RefCell::new(None),
            pin_preview: Cell::new(false),
            password_dialog: PasswordDialog::new()?,
            password_for: RefCell::new(None),
            cast_address_dialog: CastAddressDialog::new()?,
            cast_check: RefCell::default(),
            cast_check_timer: Timer::default(),
            about: AboutDialog::new()?,
            guide: GuideWindow::new()?,
            guide_steps: Rc::new(VecModel::default()),
            guide_follows_notice: Cell::new(false),
            help: crate::HelpWindow::new()?,
            remote_window: crate::RemoteWindow::new()?,
            remote: RefCell::default(),
            log,
            ui: RefCell::default(),
            main_focus: RefCell::default(),
            dialog_purpose: RefCell::new(DialogPurpose::Message),
            models,
            tray_available: Cell::new(false),
            appearance: Cell::new(ui_model::Appearance::System),
            themed_by_hand: Cell::new(false),
            menu_bar_theme: MenuBarTheme::default(),
            frames: RefCell::default(),
            on_refresh: RefCell::new(None),
            notice_announcer: RefCell::default(),
            calibration_announced: RefCell::default(),
            calibrate_all_announced: Cell::new(0),
            out_of_range: RefCell::default(),
            announcement_timer: Timer::default(),
            poll_timer: Timer::default(),
            calibration_timer: Timer::default(),
            meter_timer: Timer::default(),
            meter_level: Cell::new(0.0),
            login: RefCell::new(LaunchAtLogin::new(
                launch_at_login::system_store(),
                launch_at_login::own_exe(),
            )),
            login_timer: Timer::default(),
            #[cfg(windows)]
            updater: RefCell::new(crate::updater::for_this_exe(log_for_updater)),
            now_playing: RefCell::default(),
            now_playing_source: RefCell::default(),
        });
        let sessions = i32::try_from(audiosync_airplay::sender::AIRPLAY2_SESSIONS);
        app.main
            .global::<Strings>()
            .set_airplay2_sessions(sessions.unwrap_or(i32::MAX));
        app.attach_models();
        app.connect_main();
        app.connect_flyout();
        app.connect_equalizer();
        app.connect_dialogs();
        app.connect_password_dialog();
        app.connect_cast_address_dialog();
        app.connect_guide();
        app.connect_help();
        app.connect_remote();
        app.restore_view(true);
        app.restore_remote();
        app.launch_update_check();
        app.refresh();
        let weak = Rc::downgrade(&app);
        app.poll_timer
            .start(TimerMode::Repeated, POLL_INTERVAL, move || {
                with_app(&weak, |app| {
                    app.poll();
                    app.poll_updater();
                    app.follow_input_level(&weak);
                })
            });
        let weak = Rc::downgrade(&app);
        app.login_timer
            .start(TimerMode::Repeated, LOGIN_CHECK, move || {
                with_app(&weak, App::reread_launch_at_login)
            });
        Ok(app)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_tray_available(&self, available: bool) {
        self.tray_available.set(available);
    }

    /// The media sessions the Now Playing card reads and drives; without
    /// them the card never shows.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_media_controls(&self, controls: Box<dyn MediaControls>) {
        self.now_playing.borrow_mut().set_controls(controls);
        let _ = self.sync_now_playing();
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_on_refresh(&self, callback: impl Fn(&AppState) + 'static) {
        *self.on_refresh.borrow_mut() = Some(Box::new(callback));
    }

    pub fn strings(&self) -> Strings<'_> {
        self.main.global::<Strings>()
    }

    pub fn snapshot(&self) -> AppState {
        let mut state = self.backend.borrow().snapshot();
        state.notices.extend(self.login.borrow().notice().cloned());
        state
            .notices
            .extend(self.ui.borrow().airplay2_notice.clone());
        state.notices.extend(self.remote_public_notice());
        state.notices.extend(self.updater_notice());
        state
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn launch_at_login(&self) -> bool {
        self.login.borrow().is_on()
    }

    /// Reads Launch at Sign-In again, and shows it if it changed.
    pub fn reread_launch_at_login(&self) {
        if self.login.borrow_mut().reread() {
            self.refresh();
        }
    }

    /// The user turned Launch at Sign-In on or off.
    pub fn set_launch_at_login(&self, on: bool) {
        let outcome = self
            .login
            .borrow_mut()
            .set(on, crate::platform::notice_time);
        let status = self.login.borrow().status();
        self.log.log(|| match &outcome.error {
            Some(error) => format!(
                "launch at sign-in: turning it {} failed ({error}); {status:?}",
                if on { "on" } else { "off" }
            ),
            None => format!(
                "launch at sign-in: turned {}; {status:?}",
                if on { "on" } else { "off" }
            ),
        });
        if outcome.open_startup_settings && !crate::platform::open_settings(STARTUP_APPS_PAGE) {
            self.log
                .log(|| format!("launch at sign-in: {STARTUP_APPS_PAGE} didn't open"));
        }
        self.refresh();
    }

    fn dismiss_notice(&self, id: u64) {
        let airplay2 =
            id == AIRPLAY2_NOTICE_ID && self.ui.borrow_mut().airplay2_notice.take().is_some();
        if airplay2
            || self.login.borrow_mut().dismiss(id)
            || self.dismiss_remote_notice(id)
            || self.dismiss_updater_notice(id)
        {
            self.refresh();
        } else {
            self.update(|b| b.dismiss_notice(id));
        }
    }

    fn dismiss_all_notices(&self) {
        self.login.borrow_mut().dismiss_notice();
        self.ui.borrow_mut().airplay2_notice = None;
        self.dismiss_remote_notice(REMOTE_PUBLIC_NOTICE_ID);
        self.dismiss_updater_notice(UPDATER_NOTICE_ID);
        self.update(|b| b.dismiss_all_notices());
    }

    /// Changes the backend, then shows the result everywhere.
    pub fn update(&self, change: impl FnOnce(&mut dyn Backend)) {
        change(self.backend.borrow_mut().as_mut());
        self.refresh();
    }

    /// Ticks a calibration that holds the outputs every 20 ms until it
    /// lets them go.
    fn follow_calibration(&self, weak: &Weak<App>) {
        if !self.snapshot().calibration.busy || self.calibration_timer.running() {
            return;
        }
        let weak = weak.clone();
        self.calibration_timer
            .start(TimerMode::Repeated, CALIBRATION_TICK, move || {
                with_app(&weak, App::tick_calibration)
            });
    }

    fn tick_calibration(&self) {
        if self.backend.borrow_mut().tick_calibration() {
            self.refresh();
        }
        if !self.snapshot().calibration.busy {
            self.calibration_timer.stop();
        }
    }

    /// Starts the input level meter once an input device is captured; it
    /// stops itself when the capture ends or the source changes.
    fn follow_input_level(&self, weak: &Weak<App>) {
        if self.meter_timer.running() || !self.tick_input_level() {
            return;
        }
        let weak = weak.clone();
        self.meter_timer.start(
            TimerMode::Repeated,
            Duration::from_millis(u64::from(meter::METER_POLL_MS)),
            move || {
                with_app(&weak, |app| {
                    app.tick_input_level();
                })
            },
        );
    }

    /// Moves the meter to the peak captured since the last tick, or hides
    /// it while no input device is captured. Returns whether it shows.
    fn tick_input_level(&self) -> bool {
        let Some(peak) = self.backend.borrow_mut().input_peak() else {
            self.meter_timer.stop();
            self.meter_level.set(0.0);
            self.main.set_input_level(0.0);
            self.main.set_input_level_spoken(0);
            self.main.set_input_meter_shown(false);
            return false;
        };
        let level = meter::meter_level(self.meter_level.get(), peak);
        self.meter_level.set(level);
        self.main.set_input_level(level);
        self.main
            .set_input_level_spoken(meter::meter_spoken_percent(level) as i32);
        self.main.set_input_meter_shown(true);
        true
    }

    /// Lets the backend take in what happened on its own, and shows it.
    fn poll(&self) {
        let changed = self.backend.borrow_mut().poll();
        if self.backend.borrow_mut().take_view_restored() {
            let (remote, airplay2, airplay2_at_launch) = {
                let ui = self.ui.borrow();
                (ui.remote, ui.airplay2_experimental, ui.airplay2_at_launch)
            };
            // The window keeps its frame, and the sender its AirPlay 2 path.
            self.restore_view(false);
            self.ui.borrow_mut().airplay2_at_launch = airplay2_at_launch;
            let read = self.ui.borrow().airplay2_experimental;
            if read != airplay2 {
                self.set_airplay2_experimental(read);
            }
            if self.ui.borrow().remote != remote {
                self.restore_remote_again();
            }
        }
        if changed {
            self.refresh();
        } else if self.sync_now_playing() {
            self.publish_remote(&self.snapshot());
        }
        self.poll_remote();
    }

    /// Reads the media sessions for the Now Playing card while the main
    /// window shows or the web remote listens, and shows what changed.
    /// Returns whether the track changed.
    fn sync_now_playing(&self) -> bool {
        let window = self.main.window();
        let shows = (window.is_visible() && !window.is_minimized()) || self.remote_listening();
        let source = self.now_playing_source.borrow().clone();
        if !self.now_playing.borrow_mut().sync(source, shows) {
            return false;
        }
        let card = self.now_playing.borrow();
        let Some(track) = card.track() else {
            // Slint leaves nothing focused when the focused button goes.
            if self.main.get_now_playing_focused_button() >= 0 {
                self.main.set_now_playing_focused_button(-1);
                self.main.invoke_focus_source();
            }
            self.main.set_now_playing_shown(false);
            return true;
        };
        self.main.set_now_playing_title(track.title.as_str().into());
        self.main
            .set_now_playing_subtitle(track.subtitle().as_str().into());
        self.main.set_now_playing_playing(track.playing);
        let artwork = track.artwork.as_ref().map(|artwork| {
            let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &artwork.rgba,
                artwork.side,
                artwork.side,
            );
            slint::Image::from_rgba8_premultiplied(pixels)
        });
        self.main.set_now_playing_has_artwork(artwork.is_some());
        self.main
            .set_now_playing_artwork(artwork.unwrap_or_default());
        self.main.set_now_playing_shown(true);
        true
    }

    fn send_transport(&self, button: i32) {
        let command = match button {
            0 => Transport::Previous,
            1 => Transport::PlayPause,
            _ => Transport::Next,
        };
        self.log.log(|| format!("now playing: {command:?} pressed"));
        let shown = self
            .now_playing
            .borrow()
            .track()
            .map(|track| (track.app_id.clone(), track.playing));
        if let (Transport::PlayPause, Some((app, playing))) = (command, shown) {
            self.backend.borrow_mut().play_pause_pressed(&app, playing);
        }
        self.now_playing.borrow_mut().send(command);
    }

    /// Shows the main window and brings it to the front.
    pub fn show_main(&self) {
        let window = self.main.window();
        let position = window.position();
        let size = window.size().to_logical(window.scale_factor());
        let frame = WindowFrame {
            x: position.x,
            y: position.y,
            width: f64::from(size.width),
            height: f64::from(size.height),
            maximized: window.is_maximized(),
        };
        let on_a_monitor = crate::platform::is_on_a_monitor(frame.x, frame.y);
        let primary = crate::platform::monitor_at(0.0, 0.0);
        if let Some(fitted) = place_to_show(
            frame,
            on_a_monitor,
            window.is_minimized(),
            window.is_visible(),
            primary,
        ) {
            // Sizing a maximized window restores it, and the size is lost.
            if !fitted.maximized && (fitted.width, fitted.height) != (frame.width, frame.height) {
                window.set_size(slint::LogicalSize::new(
                    fitted.width as f32,
                    fitted.height as f32,
                ));
            }
            window.set_position(slint::PhysicalPosition::new(fitted.x, fitted.y));
            // Moving a maximized window restores it.
            if fitted.maximized {
                window.set_maximized(true);
            }
        }
        if let Err(error) = self.main.show() {
            self.show_window_error(&error);
            return;
        }
        self.apply_frame(self.main.window(), "main window", Frame::Titled);
        crate::platform::bring_to_front(self.main.window());
    }

    /// Shows every window in `appearance` at once: Match System as Windows
    /// is set, following its changes, Light and Dark whatever Windows uses.
    /// The theme preview uses it too, without saving.
    pub fn apply_appearance(&self, appearance: ui_model::Appearance) {
        use ui_model::Appearance;
        self.appearance.set(appearance);
        self.main.set_appearance(match appearance {
            Appearance::System => crate::Appearance::System,
            Appearance::Light => crate::Appearance::Light,
            Appearance::Dark => crate::Appearance::Dark,
        });
        if appearance == Appearance::System && !self.themed_by_hand.get() {
            return;
        }
        self.themed_by_hand.set(true);
        self.follow_appearance();
    }

    /// View > Appearance.
    fn choose_appearance(&self, choice: crate::Appearance) {
        let appearance = match choice {
            crate::Appearance::Light => ui_model::Appearance::Light,
            crate::Appearance::Dark => ui_model::Appearance::Dark,
            _ => ui_model::Appearance::System,
        };
        self.ui.borrow_mut().appearance = appearance;
        self.apply_appearance(appearance);
        self.remember_view();
    }

    /// Whether the windows show dark now.
    fn shows_dark(&self) -> bool {
        use slint::language::ColorScheme;
        use slint::private_unstable_api::re_exports::WindowInner;
        match self.appearance.get() {
            ui_model::Appearance::Light => false,
            ui_model::Appearance::Dark => true,
            // Slint keeps the Windows setting here, read from each new
            // window and on every theme change; winit never reports the
            // theme a window was given, only the one Windows uses.
            ui_model::Appearance::System => {
                WindowInner::from_pub(self.main.window())
                    .context()
                    .color_scheme(None)
                    == ColorScheme::Dark
            }
        }
    }

    /// Gives every window's palette, title bar and the main window's menu
    /// bar the appearance.
    fn follow_appearance(&self) {
        use slint::language::ColorScheme;
        let dark = self.shows_dark();
        let scheme = if dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        self.main.global::<Palette>().set_color_scheme(scheme);
        self.flyout.global::<Palette>().set_color_scheme(scheme);
        self.equalizer.global::<Palette>().set_color_scheme(scheme);
        self.zone_dialog
            .global::<Palette>()
            .set_color_scheme(scheme);
        self.pin_dialog.global::<Palette>().set_color_scheme(scheme);
        self.password_dialog
            .global::<Palette>()
            .set_color_scheme(scheme);
        self.cast_address_dialog
            .global::<Palette>()
            .set_color_scheme(scheme);
        self.about.global::<Palette>().set_color_scheme(scheme);
        self.guide.global::<Palette>().set_color_scheme(scheme);
        self.help.global::<Palette>().set_color_scheme(scheme);
        self.remote_window
            .global::<Palette>()
            .set_color_scheme(scheme);
        for window in [
            self.main.window(),
            self.flyout.window(),
            self.equalizer.window(),
            self.zone_dialog.window(),
            self.pin_dialog.window(),
            self.password_dialog.window(),
            self.cast_address_dialog.window(),
            self.about.window(),
            self.guide.window(),
            self.help.window(),
            self.remote_window.window(),
        ] {
            self.theme_frame(window);
            if std::ptr::eq(window, self.main.window()) {
                self.theme_menu_bar();
            } else {
                crate::platform::repaint_frame(window, false, dark);
            }
        }
    }

    /// Sends the main window's menu bar the appearance, unless that native
    /// window already has it (`menu_bar_theme`).
    fn theme_menu_bar(&self) {
        let window = self.main.window();
        let Some(id) = crate::platform::native_id(window) else {
            return;
        };
        let dark = self.shows_dark();
        if self.menu_bar_theme.due(id, dark) {
            crate::platform::repaint_frame(window, true, dark);
        }
    }

    /// With Light or Dark, gives every window the appearance again after
    /// Windows switches between light and dark, which may repaint a title bar
    /// or the menu bar in the Windows theme. Match System hears that switch
    /// from winit (`follow_titled_frame`).
    fn follow_windows_theme(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        crate::platform::set_on_color_set_change(Rc::new(move || {
            with_app(&weak, |app| {
                if !app.themed_by_hand.get() || app.appearance.get() == ui_model::Appearance::System
                {
                    return;
                }
                app.log.log(|| {
                    "Windows switched light and dark; the appearance applied again".to_owned()
                });
                app.menu_bar_theme.forget();
                app.follow_appearance();
            });
        }));
    }

    /// Gives a native window's title bar the appearance, once AudioSync
    /// sets it (`themed_by_hand`).
    fn theme_frame(&self, window: &slint::Window) {
        use slint::winit_030::WinitWindowAccessor;
        use slint::winit_030::winit::window::Theme;
        if !self.themed_by_hand.get() {
            return;
        }
        let theme = match self.appearance.get() {
            ui_model::Appearance::System => None,
            ui_model::Appearance::Light => Some(Theme::Light),
            ui_model::Appearance::Dark => Some(Theme::Dark),
        };
        window.with_winit_window(|window| window.set_theme(theme));
    }

    /// Gives a window that is showing its Windows 11 frame (`backdrop`), and
    /// logs whether DWM took it. `true` when it did.
    pub fn apply_frame(&self, window: &slint::Window, name: &'static str, frame: Frame) -> bool {
        self.use_brand_accent(window);
        self.theme_frame(window);
        crate::platform::follow_color_set(window);
        // Slint gives the main window's menu bar the Windows theme when it
        // creates it.
        if self.themed_by_hand.get() && std::ptr::eq(window, self.main.window()) {
            self.theme_menu_bar();
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

    /// Gives the standard controls (buttons, sliders, switches, fields) the
    /// brand teal for their accent instead of the Windows accent color. Slint
    /// keeps one accent for the whole app and reads the Windows one again
    /// whenever it creates a native window or the theme changes, so this runs
    /// after each (`apply_frame`, `follow_frame`). The Fluent style keeps its
    /// own lightness and takes only the hue: dark teal on light, mint on dark.
    pub fn use_brand_accent(&self, window: &slint::Window) {
        use slint::private_unstable_api::re_exports::WindowInner;
        let teal = self.main.global::<Tokens>().get_teal();
        WindowInner::from_pub(window)
            .context()
            .set_accent_color(teal);
    }

    /// Gives a titled window (every window but the flyout) its frame each
    /// time it gains the focus, which covers a native window created after
    /// `show` returned, and the brand accent again after a theme change.
    fn follow_frame(self: &Rc<Self>, window: fn(&App) -> &slint::Window, name: &'static str) {
        self.follow_titled_frame(window, name, Titled::Window);
    }

    /// `follow_frame` for a dialog, which also keeps only the close button
    /// in its title bar, as Windows dialogs do.
    fn follow_dialog_frame(
        self: &Rc<Self>,
        window: fn(&App) -> &slint::Window,
        name: &'static str,
    ) {
        self.follow_titled_frame(window, name, Titled::Dialog);
    }

    fn follow_titled_frame(
        self: &Rc<Self>,
        window: fn(&App) -> &slint::Window,
        name: &'static str,
        kind: Titled,
    ) {
        use slint::winit_030::winit::event::WindowEvent;
        use slint::winit_030::winit::window::WindowButtons;
        use slint::winit_030::{EventResult, WinitWindowAccessor};

        let weak = Rc::downgrade(self);
        // The main window's first event comes once its native window exists,
        // with or without the focus; outside this filter, as it resizes.
        let min_held = std::cell::Cell::new(kind != Titled::Main);
        window(self).on_winit_window_event(move |_, event| {
            if !min_held.replace(true) {
                let weak = weak.clone();
                Timer::single_shot(Duration::ZERO, move || {
                    with_app(&weak, |app| {
                        keep_main_min_size(window(app));
                    });
                });
            }
            match event {
                WindowEvent::Focused(true) => with_app(&weak, |app| {
                    if kind == Titled::Main {
                        keep_main_min_size(window(app));
                    }
                    if kind == Titled::Dialog {
                        window(app).with_winit_window(|window| {
                            window.set_enabled_buttons(WindowButtons::CLOSE);
                        });
                        // A PIN dialog opened before the event loop ran had
                        // no native window to fit yet.
                        app.fit_pin_dialog();
                    }
                    app.apply_frame(window(app), name, Frame::Titled);
                }),
                // Slint reads the Windows accent and theme after this filter
                // returns, and gives the menu bar the Windows theme.
                WindowEvent::ThemeChanged(_) => {
                    let weak = weak.clone();
                    Timer::single_shot(Duration::ZERO, move || {
                        with_app(&weak, |app| {
                            app.use_brand_accent(window(app));
                            if app.themed_by_hand.get() {
                                app.menu_bar_theme.forget();
                                app.follow_appearance();
                            }
                        });
                    });
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }

    fn attach_models(&self) {
        let m = &self.models;
        self.main.set_sources(ModelRc::from(m.sources.clone()));
        self.main
            .set_input_pairs(ModelRc::from(m.input_pairs.clone()));
        self.main.set_mix_apps(ModelRc::from(m.mix_apps.clone()));
        self.main.set_this_pc(ModelRc::from(m.this_pc.clone()));
        self.main.set_connected(ModelRc::from(m.connected.clone()));
        self.main
            .set_this_pc_zoned(ModelRc::from(m.this_pc_zoned.clone()));
        self.main
            .set_connected_zoned(ModelRc::from(m.connected_zoned.clone()));
        self.main.set_zones(ModelRc::from(m.zones.clone()));
        self.main.set_notices(ModelRc::from(m.notices.clone()));
        self.main
            .set_presets(ModelRc::from(m.speaker_presets.clone()));
        self.flyout
            .set_presets(ModelRc::from(m.speaker_presets.clone()));
        self.main
            .set_calibrate_all_rows(ModelRc::from(m.calibration_rows.clone()));
        self.main
            .set_calibrate_all_left_out(ModelRc::from(m.calibration_left_out.clone()));
        self.flyout.set_sources(ModelRc::from(m.sources.clone()));
        self.flyout
            .set_input_pairs(ModelRc::from(m.input_pairs.clone()));
        self.flyout
            .set_speakers(ModelRc::from(m.all_speakers.clone()));
        self.flyout.set_zones(ModelRc::from(m.zones.clone()));
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
    }

    /// Reads a snapshot and shows it in every window.
    pub fn refresh(&self) {
        // A speaker that went mid-rename takes its name along; back, it
        // shows that name, its field closed. A deleted zone's field just
        // goes.
        let gone = {
            let mut ui = self.ui.borrow_mut();
            let open = ui.rename.as_ref().map(|(id, _)| id.clone());
            let zone = ui.rename_place == RenamePlace::Zone;
            let snapshot = self.backend.borrow().snapshot();
            match open {
                Some(id) if zone && !snapshot.zones.iter().any(|z| z.id == id) => {
                    ui.rename = None;
                    None
                }
                Some(id) if !zone && snapshot.speaker(&id).is_none() => ui.rename.take(),
                _ => None,
            }
        };
        if let Some((id, text)) = gone {
            self.backend.borrow_mut().rename_speaker(&id, &text);
        }
        let state = self.snapshot();
        let strings = self.strings();
        let became_mix = {
            let mut ui = self.ui.borrow_mut();
            let is_mix = state.source_id.starts_with("mix:");
            let became_mix = is_mix && !ui.source_was_mix;
            ui.source_was_mix = is_mix;
            let show_virtual = ui.show_virtual;
            ui.listed_virtual.note(&state.speakers, show_virtual);
            // A speaker that went takes what was typed for it along: back,
            // it shows its saved delay.
            ui.delay_drafts
                .retain(|id, _| state.speakers.iter().any(|s| s.id == *id));
            ui.collapsed
                .retain(|id| state.zones.iter().any(|zone| zone.id == *id));
            became_mix
        };
        let ui = self.ui.borrow();
        let item = |speaker: &ui_model::Speaker, show_kind: bool| {
            let draft = ui
                .delay_drafts
                .get(&speaker.id)
                .and_then(|typed| typed.text(speaker.delay_ms));
            let mut row = speaker_item(
                &state,
                speaker,
                show_kind,
                ui.sync_open.contains(&speaker.id),
                draft,
            );
            // A Cast speaker's offset only holds the others back (spec C77).
            let min = if speaker.cast.is_some() {
                0.0
            } else {
                ui_model::MIN_DELAY_MS
            };
            row.delay_range_hint = range_hint(
                &strings,
                draft,
                min,
                ui_model::MAX_DELAY_MS,
                format_offset_ms,
            );
            if let Some((_, text)) = ui
                .rename
                .as_ref()
                .filter(|(id, _)| *id == speaker.id && ui.rename_place != RenamePlace::Zone)
            {
                row.renaming = true;
                row.name_text = text.as_str().into();
            }
            row.calibration_status = calibration_status(&strings, &state, speaker);
            row.calibration_left_out = speaker
                .mic_left_out
                .map_or_else(SharedString::default, |kind| {
                    calibration_left_out(&strings, kind)
                });
            row.availability = with_switch(row.availability, ui.airplay2_experimental);
            row.cast_group_note = cast_group_note(&strings, speaker);
            row.group_note = group_note(&strings, speaker);
            if let Some(zone) = state.zone_of(&speaker.id) {
                row.zone_id = zone.id.as_str().into();
                row.zone_name = zone.name.as_str().into();
                if let Some(note) = state.zone_note(zone, ui_model::ZoneRow::Listed(speaker)) {
                    row.zone_note = zone_note(&strings, note);
                }
            }
            row
        };
        let (this_pc, connected) = state.layout();
        let lists = |s: &ui_model::Speaker| ui.listed_virtual.lists(s, ui.show_virtual);
        let connected_virtual_hidden = only_virtual_outputs_hidden(&connected, lists);
        let listed = |s: &&ui_model::Speaker| lists(s);
        // A speaker in a zone shows under it, and keeps a compact row in its
        // own section after the full rows.
        let (this_pc, this_pc_zoned) =
            state.split_zoned(this_pc.into_iter().filter(listed).collect());
        let (connected, connected_zoned) =
            state.split_zoned(connected.into_iter().filter(listed).collect());
        // A zoned speaker's name field shows in the row it was asked from.
        let in_zones = ui.rename_place == RenamePlace::InZones;
        let compact = |s: &ui_model::Speaker, show_kind: bool| {
            let mut row = item(s, show_kind);
            row.renaming &= in_zones;
            row
        };
        let this_pc_zoned: Vec<SpeakerItem> = this_pc_zoned
            .iter()
            .map(|(s, _)| compact(s, false))
            .collect();
        let connected_zoned: Vec<SpeakerItem> = connected_zoned
            .iter()
            .map(|(s, _)| compact(s, true))
            .collect();
        let this_pc: Vec<SpeakerItem> = this_pc.into_iter().map(|s| item(s, false)).collect();
        let connected: Vec<SpeakerItem> = connected.into_iter().map(|s| item(s, true)).collect();
        let zones: Vec<ZoneRowsBuilt> = state
            .zones
            .iter()
            .map(|zone| {
                let members: Vec<SpeakerItem> = state
                    .zone_rows(zone)
                    .into_iter()
                    .map(|row| {
                        let mut built = match row {
                            ui_model::ZoneRow::Listed(s) => item(s, !s.kind.is_this_pc()),
                            ui_model::ZoneRow::NotFound(id) => {
                                missing_item(&strings, &state, &zone.id, id)
                            }
                        };
                        if let Some(note) = state.zone_note(zone, row) {
                            built.zone_note = zone_note(&strings, note);
                            built.zone_dimmed = zone_row_dimmed(row, note);
                        }
                        built.renaming &= !in_zones;
                        built
                    })
                    .collect();
                let targets = state
                    .zone_members(zone)
                    .filter_map(|s| {
                        Some(ZoneTarget {
                            member_id: s.id.as_str().into(),
                            target: s.in_target.as_ref()?.name.as_str().into(),
                        })
                    })
                    .collect();
                let counts = state.zone_counts(zone);
                let renaming = ui
                    .rename
                    .as_ref()
                    .filter(|(id, _)| *id == zone.id && ui.rename_place == RenamePlace::Zone);
                ZoneRowsBuilt {
                    item: ZoneItem {
                        id: zone.id.as_str().into(),
                        name: zone.name.as_str().into(),
                        state: row_state(state.zone_state(zone)),
                        on: count(counts.on),
                        total: count(counts.total),
                        waiting: count(counts.waiting),
                        present: count(counts.present),
                        volume: zone.volume as f32,
                        expanded: !ui.collapsed.contains(&zone.id),
                        can_calibrate: state.offers_zone_run(zone),
                        renaming: renaming.is_some(),
                        name_text: renaming
                            .map(|(_, text)| text.as_str().into())
                            .unwrap_or_default(),
                        ..ZoneItem::default()
                    },
                    members,
                    targets,
                }
            })
            .collect();
        let show_virtual = ui.show_virtual;
        let airplay2_experimental = ui.airplay2_experimental;
        let check_updates_automatically = ui.check_updates_automatically;
        let airplay_typed = ui
            .airplay_delay_draft
            .text(state.airplay_delay_ms)
            .map(str::to_owned);
        // No window call while `ui` is borrowed: showing a dialog runs Slint's
        // pending change handlers, and the focus ones borrow `ui` mutably (UI
        // Automation focuses and toggles in one turn).
        drop(ui);
        self.main
            .set_connected_virtual_hidden(connected_virtual_hidden);
        self.main.set_airplay_none_found(state.airplay_none_found);
        self.flyout.set_airplay_none_found(state.airplay_none_found);
        self.main.set_airplay_searching(state.airplay_searching);
        self.flyout.set_airplay_searching(state.airplay_searching);
        self.main.set_no_speaker_on(state.no_speaker_on);
        self.flyout.set_no_speaker_on(state.no_speaker_on);
        self.main.set_cast_list(cast_list(state.cast_list));
        self.main.set_cast_by_address(state.cast_by_address);
        self.main.set_this_pc_all_hidden(state.hidden_in(true));
        self.main.set_connected_all_hidden(state.hidden_in(false));
        self.flyout.set_all_hidden(state.all_hidden());
        self.flyout
            .set_connected_all_hidden(state.connected_all_hidden());
        self.main
            .set_hidden_speakers(ModelRc::new(VecModel::from(hidden_items(&state))));
        sync_model(&self.models.speaker_presets, preset_items(&state));
        self.main
            .set_can_save_preset(state.presets.len() < MAX_PRESETS);
        self.flyout
            .set_current_preset(current_preset_name(&state).into());
        let flyout_speakers: Vec<SpeakerItem> = this_pc.iter().chain(&connected).cloned().collect();
        let all: Vec<SpeakerItem> = flyout_speakers
            .iter()
            .cloned()
            .chain(zones.iter().flat_map(|zone| zone.members.iter().cloned()))
            .collect();
        let rows_typed = all.iter().any(|row| row.delay_pending);
        let mut out_of_range: Vec<(String, SharedString)> = all
            .iter()
            .filter(|row| !row.delay_range_hint.is_empty())
            .map(|row| (row.id.to_string(), row.delay_range_hint.clone()))
            .collect();
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
        let mut notice_said = None;
        match announcement {
            Announcement::Say(title) => {
                let said = strings.invoke_notice_announcement(title);
                self.announce(said.clone());
                notice_said = Some(said);
            }
            Announcement::Clear => self.clear_announcement(),
            Announcement::Keep => {}
        }
        let ended = calibration_ended(
            &mut self.calibration_announced.borrow_mut(),
            &state.calibration,
        );
        let calibration_said = ended
            .and_then(|id| all.iter().find(|row| row.id == id.as_str()))
            .map(|row| row.calibration_status.clone())
            .filter(|said| !said.is_empty());
        if let Some(said) = calibration_said {
            let said = match notice_said.take() {
                Some(notice) => strings.invoke_announcements(notice, said),
                None => said,
            };
            self.announce(said.clone());
            notice_said = Some(said);
        }
        let card = calibrate_all_card(&strings, &state);
        if self
            .calibrate_all_announced
            .replace(state.calibrate_all.endings)
            != state.calibrate_all.endings
        {
            let said = card.said.clone();
            if !said.is_empty() {
                let said = match notice_said.take() {
                    Some(notice) => strings.invoke_announcements(notice, said),
                    None => said,
                };
                self.announce(said.clone());
                notice_said = Some(said);
            }
        }
        let sources: Vec<SharedString> = state
            .sources
            .iter()
            .map(|s| match s.kind {
                // The backend's name for it is English; the UI's is translated.
                SourceKind::System => strings.get_source_system(),
                SourceKind::App | SourceKind::Input => s.name.as_str().into(),
                SourceKind::AppsExcept => strings.invoke_source_all_except(s.name.as_str().into()),
            })
            .collect();
        let source_index = state
            .sources
            .iter()
            .position(|s| s.id == state.source_id)
            .map_or(-1, |i| i as i32);
        let (input_pairs, input_pair_index) = input_pair_choice(&state, |first, last| {
            strings.invoke_input_channel_pair(first, last)
        });
        let status = strings.invoke_status(state.running, count(state.active_speakers()));
        let flyout_notices = newest_notices(&notices, FLYOUT_NOTICE_LIMIT);

        let m = &self.models;
        let flyout_shown = FlyoutCounts {
            notices: usize::try_from(self.flyout.get_notice_count()).unwrap_or(0),
            listed_notices: m.flyout_notices.row_count(),
            zones: m.zones.row_count(),
            speakers: m.all_speakers.row_count(),
            zone_members: self.zone_shape_shown(),
        };
        let flyout_removed = flyout_shown.removed_by(FlyoutCounts {
            notices: notices.len(),
            listed_notices: flyout_notices.len(),
            zones: zones.len(),
            speakers: flyout_speakers.len(),
            zone_members: zone_shape(
                zones
                    .iter()
                    .map(|zone| listed_members(zone.item.expanded, zone.members.len())),
            ),
        });
        let main_lists = MainLists {
            notices: newest_notices(&notices, MAIN_NOTICE_LIMIT),
            this_pc,
            connected,
            zones: zones
                .iter()
                .map(|zone| ZoneRows {
                    id: zone.item.id.clone(),
                    rows: if zone.item.expanded {
                        zone.members.clone()
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
        };
        let main_focus = self.main_focus_target(&main_lists);
        let MainLists {
            notices: main_notices,
            this_pc,
            connected,
            ..
        } = main_lists;
        sync_model(&m.sources, sources);
        sync_model(&m.input_pairs, input_pairs);
        self.main.set_input_pair_index(input_pair_index);
        self.flyout.set_input_pair_index(input_pair_index);
        sync_model(
            &m.mix_apps,
            state
                .mix_apps
                .iter()
                .map(|app| MixAppItem {
                    name: app.name.as_str().into(),
                    checked: app.checked,
                })
                .collect(),
        );
        self.main.set_mix_all_except(state.mix_all_except);
        // The list opens once as the source becomes a mix; folding it is
        // the user's.
        if became_mix {
            self.main.set_mix_open(true);
        }
        sync_model(&m.this_pc, this_pc);
        sync_model(&m.connected, connected);
        sync_model(&m.this_pc_zoned, this_pc_zoned);
        sync_model(&m.connected_zoned, connected_zoned);
        sync_model(&m.all_speakers, flyout_speakers);
        self.sync_zones(zones);
        sync_model(&m.notices, main_notices);
        sync_model(&m.flyout_notices, flyout_notices);
        self.flyout.set_notice_count(count(notices.len()));
        self.restore_flyout_focus(flyout_removed);
        if main_focus == Some(MainFocusTarget::StreamingSwitch) {
            self.main.invoke_focus_streaming_switch();
        }
        self.refresh_guide(&state);
        self.refresh_help(&state);
        self.show_pin_prompt(state.pin_prompt.as_ref());
        self.show_password_prompt(state.password_prompt.as_ref());

        let live_hint = match state.live_input_airplay_wait() {
            Some(AirPlayWait::Tenths(tenths)) => {
                strings.invoke_live_input_airplay(format!("{}.{}", tenths / 10, tenths % 10).into())
            }
            Some(AirPlayWait::Unknown) => strings.get_live_input_airplay_unknown(),
            None => SharedString::new(),
        };
        for window in [&self.main as &dyn WindowState, &self.flyout] {
            window.show_state(&state, &status, source_index);
            window.show_live_input_hint(&live_hint);
        }
        self.main.set_equalizer_on(state.equalizer.enabled);
        self.main.set_show_virtual_devices(show_virtual);
        self.main.set_airplay2_experimental(airplay2_experimental);
        self.main.set_launch_at_login(self.login.borrow().is_on());
        self.main.set_updater_available(self.has_updater());
        self.main
            .set_check_updates_automatically(check_updates_automatically);
        self.main
            .set_airplay_delay_shown(state.airplay_delay_ms.is_some());
        let airplay_delay = format_ms(state.airplay_delay_ms.unwrap_or(0.0));
        let typed = airplay_typed.as_deref();
        let airplay_typed = is_draft(typed, &airplay_delay);
        self.main.set_airplay_delay_pending(airplay_typed);
        self.main.set_delay_typing(rows_typed || airplay_typed);
        self.main
            .set_airplay_delay_text(typed.map_or(airplay_delay, str::to_owned).into());
        // The range the Mac's field takes too; the sender clamps the stored
        // adjustment on its own.
        let airplay_range_hint =
            range_hint(&strings, typed, 0.0, ui_model::MAX_DELAY_MS, format_ms);
        if !airplay_range_hint.is_empty() {
            out_of_range.push((AIRPLAY_DELAY_FIELD.to_owned(), airplay_range_hint.clone()));
        }
        self.main.set_airplay_delay_range_hint(airplay_range_hint);
        let alert = newly_out_of_range(&mut self.out_of_range.borrow_mut(), out_of_range);
        if let Some(hint) = alert {
            // Said alone, the range line would replace the notice's words.
            self.announce(match notice_said {
                Some(said) => strings.invoke_announcements(said, hint),
                None => hint,
            });
        }

        self.show_calibrate_all(card);
        self.show_equalizer_state(&state);
        *self.now_playing_source.borrow_mut() = CardSource::of(&state);
        let _ = self.sync_now_playing();
        self.publish_remote(&state);
        if let Some(callback) = self.on_refresh.borrow().as_ref() {
            callback(&state);
        }
    }

    /// Hide Speaker in a row's menu. The backend keeps the kind the row
    /// shows, for Hidden Speakers to name it while it isn't listed.
    fn hide_speaker(&self, id: &str) {
        let Some(speaker) = self.snapshot().speaker(id).cloned() else {
            return;
        };
        let strings = self.main.global::<Strings>();
        let kind = strings.invoke_speaker_kind(speaker_kind(speaker.kind));
        self.main_focus.borrow_mut().hiding = Some(id.into());
        self.update(|b| b.hide_speaker(id, &kind));
        self.main_focus.borrow_mut().hiding = None;
        if self.snapshot().speaker(id).is_none() {
            self.announce(strings.invoke_speaker_hidden(speaker.name.as_str().into()));
        }
    }

    /// An entry of Speakers > Hidden Speakers.
    fn unhide_speaker(&self, id: &str) {
        let state = self.snapshot();
        let Some(hidden) = state.hidden.iter().find(|hidden| hidden.id == id) else {
            return;
        };
        let said = self
            .main
            .global::<Strings>()
            .invoke_speaker_shown(hidden.name.as_str().into());
        self.update(|b| b.unhide_speaker(id));
        self.announce(said);
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

    fn show_calibrate_all(&self, card: CalibrateAllCard) {
        let main = &self.main;
        main.set_calibrate_all_running(card.running);
        main.set_calibrate_all_can_start(card.can_start);
        main.set_calibrate_all_status(card.status);
        main.set_calibrate_all_privacy(card.privacy);
        main.set_calibrate_all_results_shown(card.results_shown);
        main.set_calibrate_all_airplay(card.airplay);
        main.set_calibrate_all_zone(card.zone);
        main.set_calibrate_all_outcome(card.outcome);
        main.set_calibrate_all_can_apply(card.can_apply);
        main.set_calibrate_all_can_undo(card.can_undo);
        sync_model(&self.models.calibration_rows, card.rows);
        sync_model(&self.models.calibration_left_out, card.left_out);
    }

    /// Empties both live regions now.
    fn clear_announcement(&self) {
        self.announcement_timer.stop();
        clear_announcement(&self.main.as_weak(), &self.flyout.as_weak());
    }

    /// Puts each zone's rows into its kept models, then the zones into theirs.
    fn sync_zones(&self, zones: Vec<ZoneRowsBuilt>) {
        let m = &self.models;
        let items: Vec<ZoneItem> = {
            let mut kept = m.zone_models.borrow_mut();
            kept.retain(|id, _| zones.iter().any(|zone| zone.item.id == id.as_str()));
            zones
                .into_iter()
                .map(|zone| {
                    let models = kept.entry(zone.item.id.to_string()).or_default();
                    sync_model(&models.members, zone.members);
                    sync_model(&models.targets, zone.targets);
                    ZoneItem {
                        members: ModelRc::from(models.members.clone()),
                        targets: ModelRc::from(models.targets.clone()),
                        ..zone.item
                    }
                })
                .collect()
        };
        sync_model(&m.zones, items);
    }

    /// The shape of the zones' member lists the flyout shows now.
    fn zone_shape_shown(&self) -> u64 {
        let kept = self.models.zone_models.borrow();
        zone_shape(self.models.zones.iter().map(|zone| {
            let members = kept
                .get(zone.id.as_str())
                .map_or(0, |models| models.members.row_count());
            listed_members(zone.expanded, members)
        }))
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
            zones: flyout.get_zone_focused(),
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
            zones: m
                .zones
                .iter()
                .map(|zone| ZoneRows {
                    rows: if zone.expanded {
                        zone.members.iter().collect()
                    } else {
                        Vec::new()
                    },
                    id: zone.id,
                })
                .collect(),
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
            MainFocusTarget::Zone(id, part) => MainFocus {
                zone: Some((id.clone(), *part)),
                ..MainFocus::default()
            },
            MainFocusTarget::StreamingSwitch => MainFocus::default(),
        };
        drop(focus);
        main.set_focus_notice_index(-1);
        main.set_focus_speaker_id(SharedString::new());
        main.set_focus_zone_id(SharedString::new());
        match &target {
            MainFocusTarget::Notice(index) => main.set_focus_notice_index(count(*index)),
            MainFocusTarget::Speaker(id, part) => {
                main.set_focus_speaker_part(*part);
                main.set_focus_speaker_id(id.clone());
            }
            MainFocusTarget::Zone(id, part) => {
                main.set_focus_zone_part(*part);
                main.set_focus_zone_id(id.clone());
            }
            MainFocusTarget::StreamingSwitch => {}
        }
        Some(target)
    }

    fn show_equalizer_state(&self, state: &AppState) {
        let settings = &state.equalizer;
        let strings = self.strings();
        let names: Vec<SharedString> = eq::PRESETS
            .iter()
            .map(|p| strings.invoke_equalizer_preset(p.id.into()))
            .chain(
                state
                    .equalizer_curves
                    .iter()
                    .map(|c| c.name.as_str().into()),
            )
            .collect();
        let shown = self.shown_curve(state);
        let preset_index = ui_model::picker_index(shown);
        let curve_name = usize::try_from(preset_index)
            .ok()
            .and_then(|index| names.get(index).cloned())
            .unwrap_or_else(|| strings.get_equalizer_custom());
        sync_model(&self.models.presets, names);
        self.main.set_equalizer_curve(curve_name);
        self.equalizer
            .set_saved_curve_shown(matches!(shown, Some(CurveMatch::User(_))));
        sync_model(
            &self.models.gains,
            settings.gains_db.iter().map(|&g| g as f32).collect(),
        );
        self.equalizer.set_enabled(settings.enabled);
        self.equalizer.set_preamp(settings.preamp_db as f32);
        self.equalizer.set_preset_index(preset_index);
    }

    fn connect_main(self: &Rc<Self>) {
        self.follow_titled_frame(|app| app.main.window(), "main window", Titled::Main);
        self.follow_windows_theme();
        let main = &self.main;
        let weak = Rc::downgrade(self);

        let w = weak.clone();
        main.on_power_toggled(move |on| with_app(&w, |app| app.update(|b| b.set_running(on))));
        let w = weak.clone();
        main.on_source_selected(move |index| with_app(&w, |app| app.select_source(index)));
        let w = weak.clone();
        main.on_input_pair_selected(move |index| {
            with_app(&w, |app| app.select_input_pair(index));
        });
        let w = weak.clone();
        main.on_mix_app_toggled(move |index| with_app(&w, |app| app.toggle_mix_app(index)));
        let w = weak.clone();
        main.on_mix_mode_selected(move |on| {
            with_app(&w, |app| app.update(|b| b.set_mix_all_except(on)));
        });
        let w = weak.clone();
        main.on_switch_source_on_quit_toggled(move |on| {
            let setting = if on {
                WhenSourceQuits::SwitchToSystem
            } else {
                WhenSourceQuits::Stop
            };
            with_app(&w, |app| app.update(|b| b.set_when_source_quits(setting)))
        });
        let w = weak.clone();
        main.on_master_volume_changed(move |volume| {
            with_app(&w, |app| app.update(|b| b.set_master_volume(volume.into())))
        });
        let w = weak.clone();
        main.on_equalizer_requested(move || with_app(&w, App::show_equalizer));
        let w = weak.clone();
        main.on_now_playing_transport(move |button| with_app(&w, |app| app.send_transport(button)));
        let w = weak.clone();
        main.on_speaker_toggled(move |id| with_app(&w, |app| app.toggle_speaker(&id)));
        let w = weak.clone();
        main.on_speaker_volume_changed(move |id, volume| {
            with_app(&w, |app| {
                app.update(|b| b.set_speaker_volume(&id, volume.into()))
            })
        });
        let w = weak.clone();
        main.on_speaker_volume_editing(move |id, on| {
            // Nothing shown changes, so no refresh.
            with_app(&w, |app| {
                app.backend.borrow_mut().set_speaker_volume_editing(&id, on)
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
        main.on_delay_apply_requested(move || with_app(&w, |app| app.apply_delay_typing(None)));
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
        main.on_blank_clicked(move || with_app(&w, App::blank_click));
        main.global::<NameField>()
            .on_fit(|text| speaker_names::fitting(&text).into());
        main.global::<NameField>()
            .set_double_click(crate::platform::double_click_ms());
        let w = weak.clone();
        main.on_speaker_rename_requested(move |id| {
            with_app(&w, |app| app.start_rename(&id, RenamePlace::Row))
        });
        let w = weak.clone();
        main.on_speaker_in_zones_rename_requested(move |id| {
            with_app(&w, |app| app.start_rename(&id, RenamePlace::InZones))
        });
        let w = weak.clone();
        main.on_speaker_rename_edited(move |id, text| {
            with_app(&w, |app| app.edit_rename(&id, &text))
        });
        let w = weak.clone();
        main.on_speaker_rename_committed(move |id| {
            with_app(&w, |app| app.end_rename_in(&id, true))
        });
        let w = weak.clone();
        main.on_speaker_rename_cancelled(move |id| {
            with_app(&w, |app| app.end_rename_in(&id, false))
        });
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
        main.on_cast_muted_requested(move |id, muted| {
            with_app(&w, |app| app.update(|b| b.set_speaker_muted(&id, muted)))
        });
        let w = weak.clone();
        main.on_cast_resync_requested(move |id| {
            with_app(&w, |app| app.update(|b| b.resync_speaker(&id)))
        });
        let w = weak.clone();
        main.on_cast_add_address_requested(move || with_app(&w, App::ask_cast_address));
        let w = weak.clone();
        main.on_cast_remove_requested(move |id| {
            with_app(&w, |app| app.update(|b| b.remove_cast_address(&id)))
        });
        let w = weak.clone();
        main.on_speaker_hide_requested(move |id| with_app(&w, |app| app.hide_speaker(&id)));
        let w = weak.clone();
        main.on_speaker_unhide_requested(move |id| with_app(&w, |app| app.unhide_speaker(&id)));
        let w = weak.clone();
        main.on_calibrate_requested(move |id| {
            with_app(&w, |app| {
                app.update(|b| b.start_calibration(&id));
                app.follow_calibration(&w);
            });
        });
        let w = weak.clone();
        main.on_cancel_calibration_requested(move || {
            with_app(&w, |app| app.update(|b| b.cancel_calibration()))
        });
        let w = weak.clone();
        main.on_calibration_privacy_requested(move || with_app(&w, App::open_microphone_privacy));
        let w = weak.clone();
        main.on_calibrate_all_requested(move || {
            with_app(&w, |app| {
                app.update(|b| b.start_calibrate_all());
                app.follow_calibration(&w);
            });
        });
        let w = weak.clone();
        main.on_cancel_calibrate_all_requested(move || {
            with_app(&w, |app| app.update(|b| b.cancel_calibrate_all()))
        });
        let w = weak.clone();
        main.on_calibrate_all_again_requested(move |row| {
            with_app(&w, |app| {
                if let Ok(row) = usize::try_from(row) {
                    app.update(|b| b.measure_again(row));
                    app.follow_calibration(&w);
                }
            });
        });
        let w = weak.clone();
        main.on_calibrate_all_include_requested(move |row, included| {
            with_app(&w, |app| {
                let id = usize::try_from(row).ok().and_then(|row| {
                    let state = app.snapshot();
                    state
                        .calibrate_all
                        .left_out
                        .get(row)
                        .map(|left| left.id.clone())
                });
                if let Some(id) = id {
                    app.update(|b| b.include_in_calibration(&id, included));
                }
            });
        });
        let w = weak.clone();
        main.on_calibrate_all_apply_requested(move || {
            with_app(&w, |app| app.update(|b| b.apply_calibrate_all()))
        });
        let w = weak.clone();
        main.on_calibrate_all_undo_requested(move || {
            with_app(&w, |app| app.update(|b| b.undo_calibrate_all()))
        });
        let w = weak.clone();
        main.on_calibrate_all_close_requested(move || {
            with_app(&w, |app| app.update(|b| b.close_calibrate_all()))
        });
        let w = weak.clone();
        main.on_zone_toggled(move |id| with_app(&w, |app| app.update(|b| b.toggle_zone(&id))));
        let w = weak.clone();
        main.on_zone_expanded_changed(move |id, open| {
            with_app(&w, |app| {
                // A click on the header of the zone being renamed only
                // applies the name, as on a speaker's row.
                if app.renaming(&id, RenamePlace::Zone) {
                    app.blank_click();
                } else {
                    app.set_zone_expanded(&id, open);
                }
            })
        });
        let w = weak.clone();
        main.on_zone_volume_changed(move |id, volume| {
            with_app(&w, |app| {
                app.update(|b| b.set_zone_volume(&id, volume.into()))
            })
        });
        let w = weak.clone();
        main.on_zone_rename_requested(move |id| {
            with_app(&w, |app| app.start_rename(&id, RenamePlace::Zone))
        });
        let w = weak.clone();
        main.on_zone_rename_edited(move |id, text| with_app(&w, |app| app.edit_rename(&id, &text)));
        let w = weak.clone();
        main.on_zone_rename_committed(move |id| with_app(&w, |app| app.end_rename_in(&id, true)));
        let w = weak.clone();
        main.on_zone_rename_cancelled(move |id| with_app(&w, |app| app.end_rename_in(&id, false)));
        let w = weak.clone();
        main.on_zone_deleted(move |id| with_app(&w, |app| app.delete_zone(&id)));
        let w = weak.clone();
        main.on_zone_calibrate_requested(move |id| {
            with_app(&w, |app| {
                app.update(|b| b.start_calibrate_zone(&id));
                app.follow_calibration(&w);
            });
        });
        let w = weak.clone();
        main.on_zone_use_target_requested(move |member| {
            with_app(&w, |app| app.update(|b| b.use_target_instead(&member)))
        });
        let w = weak.clone();
        main.on_new_zone_requested(move || with_app(&w, App::ask_new_zone));
        let w = weak.clone();
        main.on_preset_recall_requested(move |id| with_app(&w, |app| app.recall_preset(&id)));
        let w = weak.clone();
        main.on_preset_rename_requested(move |id| with_app(&w, |app| app.ask_rename_preset(&id)));
        let w = weak.clone();
        main.on_preset_update_requested(move |id| with_app(&w, |app| app.update_preset(&id)));
        let w = weak.clone();
        main.on_preset_delete_requested(move |id| with_app(&w, |app| app.delete_preset(&id)));
        let w = weak.clone();
        main.on_save_preset_requested(move || with_app(&w, App::ask_save_preset));
        let w = weak.clone();
        self.equalizer
            .on_preset_shortcut(move |n| with_app(&w, |app| app.recall_preset_at(n)));
        let w = weak.clone();
        self.help
            .on_preset_shortcut(move |n| with_app(&w, |app| app.recall_preset_at(n)));
        let w = weak.clone();
        self.remote_window
            .on_preset_shortcut(move |n| with_app(&w, |app| app.recall_preset_at(n)));
        let w = weak.clone();
        self.guide
            .on_preset_shortcut(move |n| with_app(&w, |app| app.recall_preset_at(n)));
        let w = weak.clone();
        main.on_speaker_move_to_zone_requested(move |id, zone| {
            with_app(&w, |app| app.move_to_zone(&id, &zone))
        });
        let w = weak.clone();
        main.on_speaker_new_zone_requested(move |id| {
            with_app(&w, |app| app.ask_new_zone_with(&id))
        });
        let w = weak.clone();
        main.on_speaker_remove_from_zone_requested(move |id| {
            with_app(&w, |app| app.update(|b| b.remove_from_zone(&id)))
        });
        let w = weak.clone();
        main.on_speaker_only_this_requested(move |id| {
            with_app(&w, |app| app.update(|b| b.only_this_speaker(&id)))
        });
        let w = weak.clone();
        main.on_refresh_devices(move || with_app(&w, |app| app.update(|b| b.refresh_devices())));
        let w = weak.clone();
        main.on_show_virtual_devices_toggled(move |on| {
            with_app(&w, |app| app.show_virtual_devices(on))
        });
        let w = weak.clone();
        main.on_appearance_chosen(move |choice| with_app(&w, |app| app.choose_appearance(choice)));
        let w = weak.clone();
        main.on_notice_dismissed(move |id| with_app(&w, |app| app.dismiss_notice(id as u64)));
        let w = weak.clone();
        main.on_notices_dismissed_all(move || with_app(&w, App::dismiss_all_notices));
        let w = weak.clone();
        main.on_launch_at_login_toggled(move |on| with_app(&w, |app| app.set_launch_at_login(on)));
        let w = weak.clone();
        main.on_check_updates_automatically_toggled(move |on| {
            with_app(&w, |app| app.set_check_updates_automatically(on))
        });
        let w = weak.clone();
        main.on_check_for_updates_requested(move || with_app(&w, App::check_for_updates));
        let w = weak.clone();
        main.on_notice_update_button_pressed(move |button| {
            with_app(&w, |app| app.press_update_button(button))
        });
        let w = weak.clone();
        main.on_airplay2_experimental_toggled(move |on| {
            with_app(&w, |app| app.set_airplay2_experimental(on))
        });
        let w = weak.clone();
        main.on_notice_guide_requested(move |id| with_app(&w, |app| app.show_guide(id)));
        let w = weak.clone();
        main.on_notice_retry_requested(move |_| {
            with_app(&w, |app| app.update(|b| b.retry_airplay()))
        });
        let w = weak.clone();
        main.on_notice_privacy_requested(move |_| with_app(&w, App::open_microphone_privacy));
        let w = weak.clone();
        main.on_notice_focus_changed(move |id, focused| {
            with_app(&w, |app| {
                note_focus(&mut app.main_focus.borrow_mut().notice, id, focused);
            });
            apply_rename_left(&w);
        });
        let w = weak.clone();
        main.on_dismiss_all_focus_changed(move |focused| {
            with_app(&w, |app| {
                app.main_focus.borrow_mut().dismiss_all = focused;
            });
            apply_rename_left(&w);
        });
        let w = weak.clone();
        main.on_speaker_focus_changed(move |id, part| {
            with_app(&w, |app| {
                note_row_focus(&mut app.main_focus.borrow_mut().speaker, id, part);
            });
            apply_rename_left(&w);
        });
        let w = weak.clone();
        main.on_zone_focus_changed(move |id, part| {
            with_app(&w, |app| {
                note_row_focus(&mut app.main_focus.borrow_mut().zone, id, part);
            });
            apply_rename_left(&w);
        });
        let w = weak.clone();
        main.on_in_zones_focus_changed(move |id, part| {
            with_app(&w, |app| {
                note_row_focus(&mut app.main_focus.borrow_mut().in_zones, id, part);
            });
            apply_rename_left(&w);
        });
        let w = weak.clone();
        main.on_control_focus_changed(move |control, focused| {
            with_app(&w, |app| {
                note_focus(&mut app.main_focus.borrow_mut().control, control, focused);
            });
            apply_rename_left(&w);
        });
        let w = weak.clone();
        main.on_close_requested(move || with_app(&w, App::close_main));
        main.on_exit_requested(|| {
            let _ = slint::quit_event_loop();
        });
        // Ctrl+Q quits from the windows without a menu bar too, as
        // Command-Q does from any window on the Mac.
        let quit = || {
            let _ = slint::quit_event_loop();
        };
        self.equalizer.on_exit_requested(quit);
        self.help.on_exit_requested(quit);
        self.guide.on_exit_requested(quit);
        self.about.on_exit_requested(quit);
        self.remote_window.on_exit_requested(quit);
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
        flyout.on_input_pair_selected(move |index| {
            with_app(&w, |app| app.select_input_pair(index));
        });
        let w = weak.clone();
        flyout.on_master_volume_changed(move |volume| {
            with_app(&w, |app| app.update(|b| b.set_master_volume(volume.into())))
        });
        let w = weak.clone();
        flyout.on_speaker_toggled(move |id| with_app(&w, |app| app.toggle_speaker(&id)));
        let w = weak.clone();
        flyout.on_zone_toggled(move |id| with_app(&w, |app| app.update(|b| b.toggle_zone(&id))));
        let w = weak.clone();
        flyout.on_zone_expanded_changed(move |id, open| {
            with_app(&w, |app| app.set_zone_expanded(&id, open))
        });
        let w = weak.clone();
        flyout.on_preset_recall_requested(move |id| with_app(&w, |app| app.recall_preset(&id)));
        let w = weak.clone();
        flyout.on_preset_shortcut(move |n| with_app(&w, |app| app.recall_preset_at(n)));
        let w = weak.clone();
        flyout.on_notice_dismissed(move |id| with_app(&w, |app| app.dismiss_notice(id as u64)));
        let w = weak.clone();
        flyout.on_notices_dismissed_all(move || with_app(&w, App::dismiss_all_notices));
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
        flyout.on_notice_privacy_requested(move |_| with_app(&w, App::open_microphone_privacy));
        let w = weak.clone();
        flyout.on_notice_update_button_pressed(move |button| {
            with_app(&w, |app| app.press_update_button(button))
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
                app.ui.borrow_mut().chosen_curve = None;
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
                let curves = app.snapshot().equalizer_curves;
                let picked = ui_model::picked_curve(index, curves.len());
                let gains_db = match picked {
                    Some(CurveMatch::Preset(i)) => eq::PRESETS[i].gains_db,
                    Some(CurveMatch::User(i)) => curves[i].gains_db,
                    None => return,
                };
                app.ui.borrow_mut().chosen_curve = picked;
                // Like the Mac: picking a curve turns the equalizer on.
                app.change_equalizer(|s| {
                    s.gains_db = gains_db;
                    s.enabled = true;
                });
            })
        });
        let w = weak.clone();
        eq_window.on_save_curve(move || with_app(&w, App::ask_save_curve));
        let w = weak.clone();
        eq_window.on_rename_curve(move || with_app(&w, App::ask_rename_curve));
        let w = weak.clone();
        eq_window.on_delete_curve(move || {
            with_app(&w, |app| {
                let state = app.snapshot();
                if let Some(CurveMatch::User(index)) = app.shown_curve(&state) {
                    let mut curves = state.equalizer_curves;
                    eq::delete_curve(&mut curves, index);
                    app.ui.borrow_mut().chosen_curve = None;
                    app.update(|b| b.set_equalizer_curves(curves));
                }
            })
        });
        let w = weak.clone();
        eq_window.on_reset(move || {
            with_app(&w, |app| {
                app.ui.borrow_mut().chosen_curve = None;
                app.change_equalizer(EqSettings::reset);
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
        self.follow_dialog_frame(|app| app.zone_dialog.window(), "zone name dialog");
        self.follow_dialog_frame(|app| app.about.window(), "about dialog");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.zone_dialog
            .on_confirmed(move |name| with_app(&w, |app| app.confirm_dialog(&name)));
        let w = weak.clone();
        self.zone_dialog.on_cancelled(move || {
            with_app(&w, |app| {
                let _ = app.zone_dialog.hide();
            })
        });

        self.follow_dialog_frame(|app| app.pin_dialog.window(), "PIN dialog");
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
            self.fit_pin_dialog();
        }
    }

    fn open_pin_dialog(&self) {
        if let Err(error) = self.pin_dialog.show() {
            self.show_window_error(&error);
            return;
        }
        self.pin_dialog.invoke_prepare();
        self.fit_pin_dialog();
        crate::platform::bring_to_front(self.pin_dialog.window());
    }

    /// Grows the PIN dialog to its layout's lowest height at its width. The
    /// window is made at the height the layout has before the message wraps,
    /// and Windows doesn't grow it when the message gets longer (a wrong PIN).
    fn fit_pin_dialog(&self) {
        let window = self.pin_dialog.window();
        let size = window.size().to_logical(window.scale_factor());
        let fit = self.pin_dialog.get_fit_height();
        // Before the native window exists there is no width to fit to.
        if size.width > 0.0 && size.height < fit {
            window.set_size(slint::LogicalSize::new(size.width, fit));
        }
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

    /// Opens the microphone privacy settings for the notice or the
    /// calibration that asks.
    fn open_microphone_privacy(&self) {
        let uri = SettingsPage::MicrophonePrivacy.uri();
        if !crate::platform::open_settings(uri) {
            self.log
                .log(|| format!("notice: the shell did not open {uri}"));
        }
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

    fn toggle_mix_app(&self, index: i32) {
        let state = self.snapshot();
        if let Some(app) = usize::try_from(index)
            .ok()
            .and_then(|i| state.mix_apps.get(i))
        {
            self.update(|b| b.toggle_mix_app(&app.id));
        }
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

    /// `index` is the pair's place in the list, which is its stored index.
    fn select_input_pair(&self, index: i32) {
        if let Ok(pair) = u16::try_from(index) {
            self.update(|b| b.select_input_pair(pair));
        }
    }

    fn toggle_speaker(&self, id: &str) {
        let state = self.snapshot();
        if let Some(speaker) = state.speaker(id) {
            let on = state.speaker_state(speaker).click_turns_on();
            self.update(|b| b.set_speaker_enabled(id, on));
        }
    }

    /// Opens or closes a speaker's sync controls (its delay button, or a
    /// click on its row). What was typed in the other delay fields applies
    /// first, their controls staying open; closing applies its own typing.
    /// A click on the window's blank space, or on the row being renamed.
    fn blank_click(&self) {
        self.end_rename(true);
        self.end_delay_typing(true);
    }

    fn toggle_sync(&self, id: &str) {
        let renaming_it = matches!(&self.ui.borrow().rename, Some((open, _)) if open == id);
        if renaming_it {
            self.blank_click();
            return;
        }
        self.end_rename(true);
        self.apply_delay_typing(Some(id));
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

    fn has_updater(&self) -> bool {
        #[cfg(windows)]
        return self.updater.borrow().is_some();
        #[cfg(not(windows))]
        return false;
    }

    fn updater_notice(&self) -> Option<ui_model::Notice> {
        #[cfg(windows)]
        return self.updater.borrow().as_ref().and_then(|updater| {
            updater.notice().map(|notice| ui_model::Notice {
                id: UPDATER_NOTICE_ID,
                subject: "update".to_owned(),
                message: NoticeMessage::Update(notice.clone()),
                time: updater.notice_time().to_owned(),
            })
        });
        #[cfg(not(windows))]
        return None;
    }

    fn dismiss_updater_notice(&self, id: u64) -> bool {
        if id != UPDATER_NOTICE_ID {
            return false;
        }
        #[cfg(windows)]
        if let Some(updater) = self.updater.borrow_mut().as_mut() {
            updater.dismiss();
            return true;
        }
        false
    }

    /// The launch check (spec S305), once the saved settings are in.
    fn launch_update_check(&self) {
        #[cfg(windows)]
        {
            let (automatic, last, record) = {
                let ui = self.ui.borrow();
                (
                    ui.check_updates_automatically,
                    ui.last_update_check,
                    ui.update_record.clone(),
                )
            };
            if let Some(updater) = self.updater.borrow_mut().as_mut() {
                updater.set_record(record);
                updater.launch_check(automatic, last, unix_now());
            }
        }
    }

    /// The updater's record of a version that didn't install (spec S316),
    /// into the settings.
    #[cfg(windows)]
    fn keep_update_record(&self) {
        let record = match self.updater.borrow().as_ref() {
            Some(updater) => updater.record().cloned(),
            None => return,
        };
        self.ui.borrow_mut().update_record = record;
        self.remember_view();
    }

    /// At quit, before the settings are saved: after a swap whose exe
    /// Windows security removed, the old one comes back (spec S315).
    pub fn updater_at_exit(&self) {
        #[cfg(windows)]
        {
            let changed = match self.updater.borrow_mut().as_mut() {
                Some(updater) => updater.at_quit(),
                None => return,
            };
            if changed {
                self.keep_update_record();
            }
        }
    }

    /// When Windows ends the session, which may end the process before the
    /// event loop does. Returns whether no change is left unsaved.
    #[cfg(windows)]
    pub fn at_session_end(&self) -> bool {
        self.updater_at_exit();
        self.save_settings()
    }

    /// Takes the updater worker's news; a check that read the appcast
    /// stores its time.
    fn poll_updater(&self) {
        #[cfg(windows)]
        {
            let polled = match self.updater.borrow_mut().as_mut() {
                Some(updater) => updater.poll(),
                None => return,
            };
            if polled.checked {
                self.ui.borrow_mut().last_update_check = Some(unix_now());
                self.remember_view();
            }
            if polled.record_changed {
                self.keep_update_record();
            }
            if polled.quit {
                // The quit path stops streaming and saves the settings.
                let _ = slint::quit_event_loop();
            }
            if polled.changed {
                self.refresh();
            }
        }
    }

    fn set_check_updates_automatically(&self, on: bool) {
        self.ui.borrow_mut().check_updates_automatically = on;
        self.log.log(|| {
            format!(
                "updater: check for updates automatically turned {}",
                if on { "on" } else { "off" }
            )
        });
        self.remember_view();
        self.refresh();
    }

    /// Help > Check for Updates…, whatever the switch and the time say.
    fn check_for_updates(&self) {
        #[cfg(windows)]
        if let Some(updater) = self.updater.borrow_mut().as_mut() {
            updater.check(crate::updater::Trigger::Manual);
        }
        self.refresh();
    }

    fn press_update_button(&self, button: UpdateButton) {
        #[cfg(windows)]
        {
            use crate::updater::{Action, UpdaterButton};
            let button = match button {
                UpdateButton::Download => UpdaterButton::Download,
                UpdateButton::NotNow => UpdaterButton::NotNow,
                UpdateButton::Cancel => UpdaterButton::Cancel,
                UpdateButton::TryAgain => UpdaterButton::TryAgain,
                UpdateButton::OpenReleasePage => UpdaterButton::OpenReleasePage,
                UpdateButton::RestartNow => UpdaterButton::RestartNow,
            };
            let action = match self.updater.borrow_mut().as_mut() {
                Some(updater) => updater.press(button),
                None => return,
            };
            match action {
                Action::Nothing => {}
                Action::OpenReleasePage => {
                    if !crate::platform::open_settings(crate::updater::RELEASE_PAGE) {
                        self.log
                            .log(|| "updater: the release page didn't open".to_owned());
                    }
                }
            }
        }
        #[cfg(not(windows))]
        let _ = button;
        self.refresh();
    }

    /// Saved for the next launch: the running sender keeps the mode it
    /// started with, which a notice says until the item is back to it.
    fn set_airplay2_experimental(&self, on: bool) {
        let prefer = self.backend.borrow().airplay2_auto();
        {
            let mut ui = self.ui.borrow_mut();
            ui.airplay2_experimental = on;
            ui.airplay2_notice = (on != ui.airplay2_at_launch).then(|| ui_model::Notice {
                id: AIRPLAY2_NOTICE_ID,
                subject: "AirPlay 2 (Experimental)".to_owned(),
                message: NoticeMessage::AirPlay2NextLaunch { on, prefer },
                time: crate::platform::notice_time(),
            });
        }
        self.log.log(|| {
            format!(
                "{}: turned {}; applies at the next launch",
                if prefer {
                    "Prefer AirPlay 2"
                } else {
                    "AirPlay 2 (experimental)"
                },
                if on { "on" } else { "off" }
            )
        });
        self.remember_view();
        self.refresh();
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
    /// the rest: View > Show Virtual Devices, View > Appearance, and the main
    /// window's frame (see [`ui_model::frame_to_keep`]).
    fn remember_view(&self) {
        let mut backend = self.backend.borrow_mut();
        let saved = backend.view_settings();
        let settings = ViewSettings {
            show_virtual_devices: self.ui.borrow().show_virtual,
            appearance: self.ui.borrow().appearance,
            window: ui_model::frame_to_keep(self.main_window_shows(), saved.window),
            remote: self.ui.borrow().remote,
            airplay2_experimental: self.ui.borrow().airplay2_experimental,
            check_updates_automatically: self.ui.borrow().check_updates_automatically,
            last_update_check: self.ui.borrow().last_update_check,
            update_record: self.ui.borrow().update_record.clone(),
        };
        if settings != saved {
            backend.set_view_settings(settings);
        }
    }

    /// Shows the windows with the settings saved last time: View > Show
    /// Virtual Devices, View > Appearance, and the main window's size and
    /// place, fitted to the monitor it opens on (the one at its saved place,
    /// else the primary one, where it opens centered), maximized when it was.
    /// Without `with_frame` the main window keeps its size and place.
    fn restore_view(&self, with_frame: bool) {
        let saved = self.backend.borrow().view_settings();
        let airplay2_auto = self.backend.borrow().airplay2_auto();
        self.main.set_airplay2_auto(airplay2_auto);
        self.help.set_airplay2_auto(airplay2_auto);
        self.ui.borrow_mut().show_virtual = saved.show_virtual_devices;
        self.ui.borrow_mut().appearance = saved.appearance;
        self.ui.borrow_mut().remote = saved.remote;
        self.ui.borrow_mut().airplay2_experimental = saved.airplay2_experimental;
        self.ui.borrow_mut().airplay2_at_launch = saved.airplay2_experimental;
        self.ui.borrow_mut().check_updates_automatically = saved.check_updates_automatically;
        self.ui.borrow_mut().last_update_check = saved.last_update_check;
        self.ui.borrow_mut().update_record = saved.update_record.clone();
        self.apply_appearance(saved.appearance);
        let (true, Some(saved_frame)) = (with_frame, saved.window) else {
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
    /// about to quit. Returns whether no change is left unsaved.
    pub fn save_settings(&self) -> bool {
        // Quit drops a typed delay that wasn't applied and applies an open
        // rename, so a focus loss as the window goes applies nothing more.
        let rename = {
            let mut ui = self.ui.borrow_mut();
            ui.delay_drafts.clear();
            ui.airplay_delay_draft.cancel();
            ui.rename.take().map(|open| (open, ui.rename_place))
        };
        match rename {
            Some(((id, text), RenamePlace::Zone)) => {
                self.backend.borrow_mut().rename_zone(&id, &text)
            }
            Some(((id, text), _)) => self.backend.borrow_mut().rename_speaker(&id, &text),
            None => {}
        }
        self.remember_view();
        self.backend.borrow_mut().save_settings()
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

    /// Enter in a speaker's delay field: applies what was typed and
    /// keeps its sync controls open, the focus in the field, to try the next
    /// value; other rows' stay open with their typing.
    fn enter_delay(&self, id: &str) {
        self.commit_delay(id);
        self.main.set_focus_speaker_part(RowPart::Sync);
        self.main.set_focus_speaker_id(id.into());
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

    /// Rename… in a speaker's or zone's menu, F2, or a double-click on a
    /// name: that row or zone header shows a name field with the focus. A
    /// rename open elsewhere applies first; one open in this place keeps its
    /// typing.
    fn start_rename(&self, id: &str, place: RenamePlace) {
        if !self.renaming(id, place) {
            self.end_rename(true);
            let state = self.snapshot();
            let name = match place {
                RenamePlace::Zone => state
                    .zones
                    .iter()
                    .find(|z| z.id == id)
                    .map(|z| z.name.clone()),
                RenamePlace::Row | RenamePlace::InZones => {
                    state.speaker(id).map(|s| s.name.clone())
                }
            };
            let Some(name) = name else {
                return;
            };
            {
                let mut ui = self.ui.borrow_mut();
                ui.rename = Some((id.to_owned(), name));
                ui.rename_place = place;
                ui.rename_focused = false;
            }
            // The field takes the focus; a control that went while it had
            // the focus never reported losing it.
            self.main_focus.borrow_mut().control = None;
            self.refresh();
        }
        // After the menu that asked for it has closed and given the focus
        // back to the control that had it, as on the Mac.
        let main = self.main.as_weak();
        let id = SharedString::from(id);
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(main) = main.upgrade() {
                focus_row(&main, place, id, RowPart::Name);
            }
        });
    }

    /// Whether `id`'s name is being edited in `place`.
    fn renaming(&self, id: &str, place: RenamePlace) -> bool {
        let ui = self.ui.borrow();
        ui.rename_place == place && matches!(&ui.rename, Some((open, _)) if open == id)
    }

    fn edit_rename(&self, id: &str, text: &str) {
        if let Some((open, typed)) = self.ui.borrow_mut().rename.as_mut()
            && open == id
        {
            *typed = text.to_owned();
        }
    }

    /// Ends the rename only if it is open in row `id`: a field that loses
    /// the focus as it closes, after another row's rename began, must not
    /// end that one.
    fn end_rename_in(&self, id: &str, apply: bool) {
        let open = matches!(&self.ui.borrow().rename, Some((open, _)) if open.as_str() == id);
        if open {
            self.end_rename(apply);
        }
    }

    /// Ends the rename open in a row or zone header, if any: Enter, the
    /// field losing the focus or a click outside it applies the name
    /// (`apply`), Escape keeps the old one. The keyboard focus, when it was
    /// in the field, goes to the row's or header's toggle, which Narrator
    /// reads with the name.
    fn end_rename(&self, apply: bool) {
        let Some((id, text)) = self.ui.borrow_mut().rename.take() else {
            return;
        };
        let place = {
            let mut ui = self.ui.borrow_mut();
            ui.rename_focused = false;
            std::mem::take(&mut ui.rename_place)
        };
        let focused = self.main_focus.borrow().holds_name(place, &id);
        match (apply, place) {
            (true, RenamePlace::Zone) => self.update(|b| b.rename_zone(&id, &text)),
            (true, _) => self.update(|b| b.rename_speaker(&id, &text)),
            (false, _) => self.refresh(),
        }
        if focused {
            focus_row(&self.main, place, id.as_str().into(), RowPart::Toggle);
        }
    }

    /// The speaker or zone whose rename to apply, once the focus has left
    /// its name field for another control, as on the Mac. Losing the focus
    /// to another window or a popup, which give it back, applies nothing.
    fn focus_left_rename(&self) -> Option<String> {
        let mut ui = self.ui.borrow_mut();
        let id = ui.rename.as_ref()?.0.clone();
        let focus = self.main_focus.borrow();
        if focus.holds_name(ui.rename_place, &id) {
            ui.rename_focused = true;
            return None;
        }
        let elsewhere = focus.speaker.is_some()
            || focus.zone.is_some()
            || focus.in_zones.is_some()
            || focus.notice.is_some()
            || focus.dismiss_all
            || focus.control.is_some();
        (ui.rename_focused && elsewhere).then_some(id)
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
    /// Applies what was typed in every delay field but `except`'s, in a
    /// fixed order; every sync control stays open. Ctrl+S passes `None`.
    fn apply_delay_typing(&self, except: Option<&str>) {
        let mut typed: Vec<String> = self
            .ui
            .borrow()
            .delay_drafts
            .keys()
            .filter(|id| Some(id.as_str()) != except)
            .cloned()
            .collect();
        typed.sort();
        for id in typed {
            self.commit_delay(&id);
        }
        self.commit_airplay_delay();
    }

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

    fn shown_curve(&self, state: &AppState) -> Option<CurveMatch> {
        ui_model::shown_curve(state, self.ui.borrow().chosen_curve)
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

    /// Speakers > New Zone from Active Speakers… (Ctrl+N): a name for a
    /// zone of the speakers that are on, or why none can be made yet.
    fn ask_new_zone(&self) {
        let strings = self.strings();
        let dialog = &self.zone_dialog;
        if self.snapshot().active_speakers() == 0 {
            *self.dialog_purpose.borrow_mut() = DialogPurpose::Message;
            dialog.set_heading(strings.get_new_zone_title());
            dialog.set_message(strings.get_new_zone_needs_speakers());
            dialog.set_show_field(false);
            dialog.set_confirm_label(strings.get_ok());
        } else {
            *self.dialog_purpose.borrow_mut() = DialogPurpose::NewZone;
            dialog.set_heading(strings.get_new_zone_title());
            dialog.set_message(strings.get_new_zone_message());
            self.ask_zone_name(&strings);
        }
        self.open_dialog();
    }

    /// New Zone… in a speaker's menu: a name for a zone of that speaker.
    fn ask_new_zone_with(&self, speaker_id: &str) {
        let Some(speaker) = self.snapshot().speaker(speaker_id).cloned() else {
            return;
        };
        let strings = self.strings();
        let dialog = &self.zone_dialog;
        *self.dialog_purpose.borrow_mut() = DialogPurpose::NewZoneWith(speaker_id.to_owned());
        dialog.set_heading(strings.get_new_zone_title());
        dialog.set_message(strings.invoke_new_zone_for_speaker(speaker.name.as_str().into()));
        self.ask_zone_name(&strings);
        self.open_dialog();
    }

    fn ask_zone_name(&self, strings: &Strings<'_>) {
        let dialog = &self.zone_dialog;
        dialog.set_placeholder(strings.get_new_zone_placeholder());
        dialog.set_field_label(strings.get_new_zone_field_label());
        dialog.set_name(SharedString::new());
        dialog.set_show_field(true);
        dialog.set_confirm_label(strings.get_create());
    }

    fn ask_save_curve(&self) {
        let state = self.snapshot();
        let strings = self.strings();
        let dialog = &self.zone_dialog;
        let shown = match self.shown_curve(&state) {
            Some(CurveMatch::User(index)) => Some(&state.equalizer_curves[index]),
            _ => None,
        };
        if shown.is_none() && state.equalizer_curves.len() >= eq::MAX_USER_CURVES {
            *self.dialog_purpose.borrow_mut() = DialogPurpose::Message;
            dialog.set_heading(strings.get_save_curve_title());
            dialog.set_message(strings.invoke_curves_full(count(eq::MAX_USER_CURVES)));
            dialog.set_show_field(false);
            dialog.set_confirm_label(strings.get_ok());
        } else {
            let name = shown.map_or_else(
                || strings.invoke_numbered_curve(count(state.equalizer_curves.len() + 1)),
                |curve| curve.name.as_str().into(),
            );
            *self.dialog_purpose.borrow_mut() = DialogPurpose::SaveCurve;
            dialog.set_heading(strings.get_save_curve_title());
            dialog.set_message(strings.get_save_curve_message());
            dialog.set_placeholder(strings.get_curve_name_field_label());
            dialog.set_field_label(strings.get_curve_name_field_label());
            dialog.set_name(name);
            dialog.set_show_field(true);
            dialog.set_confirm_label(strings.get_save());
        }
        self.open_dialog();
    }

    fn ask_rename_curve(&self) {
        let state = self.snapshot();
        let Some(CurveMatch::User(index)) = self.shown_curve(&state) else {
            return;
        };
        let name = state.equalizer_curves[index].name.as_str();
        let strings = self.strings();
        let dialog = &self.zone_dialog;
        *self.dialog_purpose.borrow_mut() = DialogPurpose::RenameCurve(name.to_owned());
        dialog.set_heading(strings.get_rename_curve_title());
        dialog.set_message(strings.invoke_rename_message(name.into()));
        dialog.set_placeholder(strings.get_curve_name_field_label());
        dialog.set_field_label(strings.get_curve_name_field_label());
        dialog.set_name(name.into());
        dialog.set_show_field(true);
        dialog.set_confirm_label(strings.get_rename());
        self.open_dialog();
    }

    /// Shows `message` in the name dialog, without a field.
    fn show_dialog_message(&self, heading: SharedString, message: SharedString) {
        let dialog = &self.zone_dialog;
        *self.dialog_purpose.borrow_mut() = DialogPurpose::Message;
        dialog.set_heading(heading);
        dialog.set_message(message);
        dialog.set_show_field(false);
        dialog.set_confirm_label(self.strings().get_ok());
        self.open_dialog();
    }

    /// A new zone's name: what was typed, else the first "Zone n" no zone
    /// has, counting on from the zones there are.
    fn new_zone_name(&self, typed: &str) -> String {
        if !typed.trim().is_empty() {
            return typed.to_owned();
        }
        let zones = self.snapshot().zones;
        let strings = self.strings();
        (zones.len() + 1..)
            .map(|n| strings.invoke_numbered_zone(count(n)).to_string())
            .find(|name| zones.iter().all(|zone| zone.name != *name))
            .unwrap_or_default()
    }

    /// The arrow of zone `id`, or Left and Right on its header.
    pub fn set_zone_expanded(&self, id: &str, open: bool) {
        let changed = {
            let mut ui = self.ui.borrow_mut();
            if open {
                ui.collapsed.remove(id)
            } else {
                ui.collapsed.insert(id.to_owned())
            }
        };
        if changed {
            self.refresh();
        }
    }

    /// Delete Zone: its speakers go back to the plain lists; Narrator says
    /// it went, since its header did.
    fn delete_zone(&self, id: &str) {
        let Some(zone) = self.snapshot().zones.into_iter().find(|z| z.id == id) else {
            return;
        };
        self.update(|b| b.delete_zone(id));
        let said = self
            .strings()
            .invoke_zone_deleted(zone.name.as_str().into());
        self.announce(said);
    }

    /// Move to Zone in a speaker's menu; Narrator says where it went.
    fn move_to_zone(&self, speaker_id: &str, zone_id: &str) {
        let state = self.snapshot();
        let (Some(speaker), Some(zone)) = (
            state.speaker(speaker_id),
            state.zones.iter().find(|z| z.id == zone_id),
        ) else {
            return;
        };
        let said = self
            .strings()
            .invoke_speaker_moved_to_zone(speaker.name.as_str().into(), zone.name.as_str().into());
        self.update(|b| b.move_to_zone(speaker_id, zone_id));
        self.announce(said);
    }

    /// Recalls preset `id` (spec S265) from any path; it opens, raises and
    /// focuses nothing (S266).
    fn recall_preset(&self, id: &str) {
        self.ui.borrow_mut().chosen_curve = None;
        self.update(|b| b.recall_preset(id));
    }

    /// Ctrl+`n`: the `n`th preset in list order; nothing without one.
    fn recall_preset_at(&self, n: i32) {
        let Some(index) = usize::try_from(n).ok().and_then(|n| n.checked_sub(1)) else {
            return;
        };
        if let Some(preset) = self.snapshot().presets.get(index) {
            self.recall_preset(&preset.id);
        }
    }

    fn ask_save_preset(&self) {
        let strings = self.strings();
        let state = self.snapshot();
        if state.presets.len() >= MAX_PRESETS {
            self.show_dialog_message(strings.get_save_preset_title(), strings.get_presets_full());
            return;
        }
        if ui_model::speakers_on_now(&state).is_empty() {
            self.show_dialog_message(
                strings.get_save_preset_title(),
                strings.get_save_preset_needs_speakers(),
            );
            return;
        }
        let dialog = &self.zone_dialog;
        *self.dialog_purpose.borrow_mut() = DialogPurpose::SavePreset;
        dialog.set_heading(strings.get_save_preset_title());
        dialog.set_message(strings.get_save_preset_message());
        dialog.set_placeholder(strings.get_preset_placeholder());
        dialog.set_field_label(strings.get_preset_field_label());
        dialog.set_name(SharedString::new());
        dialog.set_show_field(true);
        dialog.set_confirm_label(strings.get_save());
        self.open_dialog();
    }

    fn ask_rename_preset(&self, id: &str) {
        let Some(preset) = self.snapshot().presets.into_iter().find(|p| p.id == id) else {
            return;
        };
        let strings = self.strings();
        let dialog = &self.zone_dialog;
        *self.dialog_purpose.borrow_mut() = DialogPurpose::RenamePreset(id.to_owned());
        dialog.set_heading(strings.get_rename_preset_title());
        dialog.set_message(strings.invoke_rename_message(preset.name.as_str().into()));
        dialog.set_placeholder(strings.get_preset_placeholder());
        dialog.set_field_label(strings.get_preset_field_label());
        dialog.set_name(preset.name.as_str().into());
        dialog.set_show_field(true);
        dialog.set_confirm_label(strings.get_rename());
        self.open_dialog();
    }

    /// Save in the Save Preset dialog: a blank name becomes "Preset n".
    fn save_preset(&self, typed: &str) {
        let names: Vec<String> = self
            .snapshot()
            .presets
            .into_iter()
            .map(|p| p.name)
            .collect();
        let strings = self.strings();
        let name = new_preset_name(typed, &names, |n| {
            strings.invoke_numbered_preset(count(n)).to_string()
        });
        let saved = self.backend.borrow_mut().save_preset(&name);
        self.refresh();
        match saved {
            Ok(()) => self.announce(strings.invoke_preset_saved(name.as_str().into())),
            Err(PresetError::Full) => self
                .show_dialog_message(strings.get_save_preset_title(), strings.get_presets_full()),
            // The last speaker went off while the dialog was open.
            Err(PresetError::NoSpeakerOn) => self.show_dialog_message(
                strings.get_save_preset_title(),
                strings.get_save_preset_needs_speakers(),
            ),
        }
    }

    /// Update to Current Settings; it asks nothing (spec S262).
    fn update_preset(&self, id: &str) {
        let Some(preset) = self.snapshot().presets.into_iter().find(|p| p.id == id) else {
            return;
        };
        let updated = self.backend.borrow_mut().update_preset(id);
        self.refresh();
        let strings = self.strings();
        match updated {
            Ok(()) => self.announce(strings.invoke_preset_updated(preset.name.as_str().into())),
            Err(_) => self.show_dialog_message(
                strings.get_update_preset(),
                strings.get_save_preset_needs_speakers(),
            ),
        }
    }

    /// Delete; it asks nothing (spec S262). The focus goes to the next row,
    /// else the previous one, else Save as Preset… (S267).
    fn delete_preset(&self, id: &str) {
        let presets = self.snapshot().presets;
        let Some(index) = presets.iter().position(|p| p.id == id) else {
            return;
        };
        let neighbour = presets
            .get(index + 1)
            .or_else(|| index.checked_sub(1).and_then(|i| presets.get(i)))
            .map(|p| p.id.clone());
        self.update(|b| b.delete_preset(id));
        match neighbour {
            Some(next) => self.main.set_focus_preset_id(next.into()),
            None => self.main.set_focus_save_preset(true),
        }
        let said = self
            .strings()
            .invoke_preset_deleted(presets[index].name.as_str().into());
        self.announce(said);
    }

    fn open_dialog(&self) {
        if let Err(error) = self.zone_dialog.show() {
            self.show_window_error(&error);
            return;
        }
        self.zone_dialog.invoke_prepare();
        crate::platform::bring_to_front(self.zone_dialog.window());
    }

    fn confirm_dialog(&self, name: &str) {
        let purpose = self.dialog_purpose.borrow().clone();
        let _ = self.zone_dialog.hide();
        match purpose {
            DialogPurpose::NewZone => {
                let name = self.new_zone_name(name);
                // The dialog only offers a name while a speaker is on; if the
                // last one went off meanwhile, nothing is made.
                let _ = self.backend.borrow_mut().new_zone(&name);
                self.refresh();
            }
            DialogPurpose::NewZoneWith(id) => {
                let name = self.new_zone_name(name);
                self.update(|b| b.new_zone_with(&id, &name));
            }
            DialogPurpose::Message => {}
            DialogPurpose::SavePreset => self.save_preset(name),
            DialogPurpose::RenamePreset(id) => self.update(|b| b.rename_preset(&id, name)),
            DialogPurpose::SaveCurve => {
                let state = self.snapshot();
                let mut curves = state.equalizer_curves;
                // An empty name saves nothing (spec S270).
                if eq::curve_name(name).is_none() {
                    return;
                }
                if let Some(index) = eq::save_curve(&mut curves, name, &state.equalizer.gains_db) {
                    self.ui.borrow_mut().chosen_curve = Some(CurveMatch::User(index));
                    self.update(|b| b.set_equalizer_curves(curves));
                } else {
                    let strings = self.strings();
                    self.show_dialog_message(
                        strings.get_save_curve_title(),
                        strings.invoke_curves_full(count(eq::MAX_USER_CURVES)),
                    );
                }
            }
            DialogPurpose::RenameCurve(old) => {
                let mut curves = self.snapshot().equalizer_curves;
                let Some(index) = curves.iter().position(|curve| curve.name == old) else {
                    return;
                };
                if eq::rename_curve(&mut curves, index, name) {
                    self.update(|b| b.set_equalizer_curves(curves));
                } else if let Some(taken) = eq::curve_name(name) {
                    let strings = self.strings();
                    self.show_dialog_message(
                        strings.get_rename_curve_title(),
                        strings.invoke_curve_name_taken(taken.as_str().into()),
                    );
                }
            }
        }
    }

    /// Closing the main window keeps AudioSync running in the tray, like the
    /// Mac app in the menu bar. Without a tray icon, it quits.
    fn close_main(&self) {
        // With a tray this is a click outside the controls, so what was typed
        // applies; without one it quits, and a quit drops it.
        self.end_rename(self.tray_available.get());
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

    /// Sizes the flyout to show every part down to its buttons, at most
    /// `room` logical pixels high, and returns that height. Windows makes it
    /// at Slint's preferred height, which counts a wrapped notice as one line
    /// and cut the buttons off. On a screen too short for it all, the zones
    /// and speakers list gets less room and scrolls; when that is not enough,
    /// everything between the header and the buttons scrolls.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn fit_flyout(&self, room: f32) -> f32 {
        let flyout = &self.flyout;
        let set_list_cut = |cut: f32| {
            if flyout.get_list_cut() != cut {
                flyout.set_list_cut(cut);
            }
        };
        let set_body_cut = |cut: f32| {
            if flyout.get_body_cut() != cut {
                flyout.set_body_cut(cut);
            }
        };
        let cut = flyout.get_list_cut() > 0.0 || flyout.get_body_cut() > 0.0;
        let mut height = flyout.get_fit_height();
        if !flyout_fit_holds(height, room, cut) {
            set_list_cut(0.0);
            set_body_cut(0.0);
            height = flyout.get_fit_height();
            if height > room {
                set_list_cut(height - room);
                height = flyout.get_fit_height();
            }
            if height > room {
                set_body_cut(height - room);
                height = flyout.get_fit_height();
            }
        }
        let height = height.min(room);
        let window = flyout.window();
        let size = window.size().to_logical(window.scale_factor());
        if (size.height - height).abs() >= 0.5 {
            window.set_size(slint::LogicalSize::new(size.width, height));
        }
        height
    }
}

/// Whether the flyout's cuts still fit its content in `room`, so the
/// refresh every 500 ms makes no layout pass: with a cut it fills the room
/// exactly, without one it fits.
fn flyout_fit_holds(fit: f32, room: f32, cut: bool) -> bool {
    if cut {
        (fit - room).abs() < 0.5
    } else {
        fit <= room
    }
}

/// The properties the main window and the flyout share.
trait WindowState {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32);
    fn show_live_input_hint(&self, hint: &SharedString);
}

impl WindowState for MainWindow {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32) {
        self.set_running(state.running);
        self.set_status(status.clone());
        self.set_source_index(source_index);
        self.set_master_volume(state.master_volume as f32);
        self.set_switch_source_on_quit(state.when_source_quits == WhenSourceQuits::SwitchToSystem);
    }

    fn show_live_input_hint(&self, hint: &SharedString) {
        self.set_live_input_hint(hint.clone());
    }
}

impl WindowState for TrayFlyout {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32) {
        self.set_running(state.running);
        self.set_status(status.clone());
        self.set_source_index(source_index);
        self.set_master_volume(state.master_volume as f32);
    }

    fn show_live_input_hint(&self, hint: &SharedString) {
        self.set_live_input_hint(hint.clone());
    }
}

/// The channel pairs the chosen input device offers, labeled by `label`
/// (first and last channel, from 1), and the place of the one it plays: a
/// saved pair the device no longer has plays 1-2, so 1-2 shows.
fn input_pair_choice(
    state: &AppState,
    label: impl Fn(i32, i32) -> SharedString,
) -> (Vec<SharedString>, i32) {
    let channels = state
        .sources
        .iter()
        .find(|source| source.id == state.source_id && source.kind == SourceKind::Input)
        .map_or(0, |source| source.channels);
    let labels = InputPair::offered(channels)
        .into_iter()
        .map(|pair| label(i32::from(pair.left) + 1, i32::from(pair.right) + 1))
        .collect();
    let (playing, _) = InputPair::resolve(state.input_pair, channels);
    (labels, i32::from(playing.index))
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
#[cfg(any(windows, target_os = "linux", test))]
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
#[cfg(any(windows, target_os = "linux", test))]
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

/// The AirPlay delay field's key among the fields out of range (no speaker
/// id has this form).
const AIRPLAY_DELAY_FIELD: &str = "airplay-delay";

/// The range line to announce: that of a field whose number has just gone
/// out of range. A field that stays out of range as more is typed isn't
/// announced again; `fields` becomes the fields out of range now.
fn newly_out_of_range(
    fields: &mut HashSet<String>,
    now: Vec<(String, SharedString)>,
) -> Option<SharedString> {
    let alert = now
        .iter()
        .find(|(id, _)| !fields.contains(id))
        .map(|(_, hint)| hint.clone());
    *fields = now.into_iter().map(|(id, _)| id).collect();
    alert
}

/// What a delay field says under it while it holds a number outside
/// `min..=max`: the range and the value Enter applies instead. Empty
/// otherwise, which also takes the field's red outline away.
fn range_hint(
    strings: &Strings,
    typed: Option<&str>,
    min: f64,
    max: f64,
    format: fn(f64) -> String,
) -> SharedString {
    match typed.and_then(ui_model::parse_ms) {
        Some(ms) if !(min..=max).contains(&ms) => strings.invoke_delay_out_of_range(
            format(min).into(),
            format(max).into(),
            format(ms.clamp(min, max)).into(),
        ),
        _ => SharedString::default(),
    }
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
        SpeakerState::Waiting(ui_model::Waiting::ForAirPlay2Slot) => RowState::WaitingForAirplay2,
        SpeakerState::Waiting(ui_model::Waiting::ForPtp) => RowState::WaitingForPtp,
        SpeakerState::Waiting(ui_model::Waiting::NotOnNetwork) => RowState::NotOnNetwork,
    }
}

pub fn speaker_kind(kind: ui_model::SpeakerKind) -> SpeakerKind {
    use ui_model::SpeakerKind as K;
    match kind {
        K::AirPlay => SpeakerKind::Airplay,
        K::AirPlay2 => SpeakerKind::Airplay2,
        K::Cast => SpeakerKind::Cast,
        K::BuiltIn => SpeakerKind::BuiltIn,
        K::Headphones => SpeakerKind::Headphones,
        K::Bluetooth => SpeakerKind::Bluetooth,
        K::Usb => SpeakerKind::Usb,
        K::UsbHeadphones => SpeakerKind::UsbHeadphones,
        K::UsbSpeakers => SpeakerKind::UsbSpeakers,
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
        ui_model::Availability::PairingNotYet => Availability::PairingNotYet,
        ui_model::Availability::AirPlay2NotYet => Availability::Airplay2NotYet,
        ui_model::Availability::Unsupported => Availability::Unsupported,
    }
}

/// A receiver that needs AirPlay 2 while the switch is on: this launch runs
/// without it, and the next one plays it.
fn with_switch(availability: Availability, airplay2_switch: bool) -> Availability {
    if airplay2_switch && availability == Availability::Airplay2NotYet {
        Availability::Airplay2NextLaunch
    } else {
        availability
    }
}

fn airplay_problem(problem: ui_model::AirPlayProblem) -> AirPlayProblem {
    use ui_model::AirPlayProblem as P;
    match problem {
        P::NotResponding => AirPlayProblem::NotResponding,
        P::ConnectionLost => AirPlayProblem::ConnectionLost,
        P::Busy => AirPlayProblem::Busy,
        P::Refused => AirPlayProblem::Refused,
        P::NeedsPassword => AirPlayProblem::NeedsPassword,
        P::NeedsPin => AirPlayProblem::NeedsPin,
        P::NotAccepted => AirPlayProblem::NotAccepted,
        P::PairingFailed => AirPlayProblem::PairingFailed,
        P::PairingNotYet => AirPlayProblem::PairingNotYet,
        P::AirPlay2NotYet => AirPlayProblem::Airplay2NotYet,
        P::TimingUnavailable => AirPlayProblem::TimingUnavailable,
        P::TooManyAirPlay2Speakers => AirPlayProblem::TooManyAirplay2Speakers,
        P::PtpOtherNetwork => AirPlayProblem::PtpOtherNetwork,
        P::PlaysThroughSpeakers => AirPlayProblem::PlaysThroughSpeakers,
        P::ReceiverUnverified => AirPlayProblem::ReceiverUnverified,
        P::AppleTvPlays => AirPlayProblem::AppleTvPlays,
        P::PlaysInGroup => AirPlayProblem::PlaysInGroup,
        P::Unsupported => AirPlayProblem::Unsupported,
    }
}

fn cast_problem(problem: ui_model::CastProblem) -> CastProblem {
    use ui_model::CastProblem as P;
    match problem {
        P::Busy => CastProblem::Busy,
        P::NotResponding => CastProblem::NotResponding,
        P::Refused => CastProblem::Refused,
        P::Blocked => CastProblem::Blocked,
        P::TakenOver => CastProblem::TakenOver,
        P::ConnectionLost => CastProblem::ConnectionLost,
        P::SearchStopped => CastProblem::SearchStopped,
    }
}

fn cast_state(state: ui_model::CastState) -> CastState {
    use ui_model::CastState as S;
    match state {
        S::Idle => CastState::Idle,
        S::Connecting => CastState::Connecting,
        S::Playing => CastState::Playing,
        S::Reconnecting => CastState::Reconnecting,
    }
}

fn cast_list(list: ui_model::CastList) -> CastList {
    use ui_model::CastList as L;
    match list {
        L::Quiet => CastList::Quiet,
        L::Searching => CastList::Searching,
        L::NoneFound => CastList::NoneFound,
        L::Failed => CastList::Failed,
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
    let renamed = speaker.name != speaker.system_name;
    SpeakerItem {
        id: speaker.id.as_str().into(),
        name: speaker.name.as_str().into(),
        system_name: if renamed {
            speaker.system_name.as_str().into()
        } else {
            SharedString::default()
        },
        device_name: speaker.system_name.as_str().into(),
        // Set by the caller while the name is edited (`App::refresh`).
        renaming: false,
        name_text: SharedString::default(),
        kind: speaker_kind(speaker.kind),
        show_kind,
        state: row_state(state.speaker_state(speaker)),
        volume: speaker.volume as f32,
        has_delay: speaker.delay_ms.is_some(),
        delay_ms: speaker.delay_ms.unwrap_or(0.0).round() as i32,
        delay_pending: is_draft(draft, &delay),
        delay_text: draft.map_or(delay, str::to_string).into(),
        // Needs the strings: set by the caller (`App::refresh`).
        delay_range_hint: SharedString::default(),
        reference_name: state.reference_name.as_deref().unwrap_or_default().into(),
        sync_open: sync_open && speaker.delay_ms.is_some(),
        calibratable: is_calibratable(speaker),
        calibrating: state.calibration.speaker_id.as_deref() == Some(speaker.id.as_str())
            && state.calibration.phase.is_running(),
        calibration_enabled: !state.calibration.busy
            && calibration::unavailable(
                state.running,
                state.reference_name.is_some(),
                speaker.enabled,
            )
            .is_none(),
        // Needs the strings: set by the caller (`App::refresh`).
        calibration_status: SharedString::default(),
        calibration_privacy: offers_privacy(
            state.calibration.speaker_id.as_deref() == Some(speaker.id.as_str())
                && state.calibration.phase == Phase::Failed(Failure::MicrophoneDenied),
            crate::platform::can_open_settings,
        ),
        // Needs the strings: set by the caller (`App::refresh`).
        calibration_left_out: SharedString::default(),
        auto_connect: speaker.auto_connect,
        availability: availability(speaker.availability),
        paired: speaker.paired,
        holds_password: speaker.holds_password,
        holds_credentials: speaker.holds_credentials,
        code_every_time: speaker.code_every_time,
        device_volume,
        device_volume_percent,
        cast: speaker.cast.is_some(),
        cast_state: speaker
            .cast
            .as_ref()
            .map_or(CastState::Idle, |cast| cast_state(cast.state)),
        muted: speaker.cast.as_ref().is_some_and(|cast| cast.muted),
        volume_fixed: speaker.cast.as_ref().is_some_and(|cast| cast.volume_fixed),
        volume_step: speaker.cast.as_ref().map_or(1.0, |cast| cast.volume_step) as f32,
        cast_group: cast_group(speaker).is_some(),
        cast_added_by_address: speaker
            .cast
            .as_ref()
            .is_some_and(|cast| cast.added_by_address),
        hideable: !speaker.is_reference(),
        // Needs the strings: set by the caller (`App::refresh`).
        cast_group_note: SharedString::default(),
        group_note: SharedString::default(),
        zone_id: SharedString::default(),
        zone_name: SharedString::default(),
        zone_movable: speaker.in_target.is_none(),
        // Needs the zone: set by the caller (`App::refresh`).
        zone_note: SharedString::default(),
        zone_dimmed: false,
        missing: false,
    }
}

/// The row of zone member `id` while it isn't listed: its name and kind as
/// last known, and a menu that can still move it or take it out.
fn missing_item(strings: &Strings<'_>, state: &AppState, zone_id: &str, id: &str) -> SpeakerItem {
    let absent = state.absent_member(id);
    let name = absent
        .map(|member| member.name.as_str())
        .filter(|name| !name.is_empty())
        .map_or_else(|| strings.get_unknown_device(), SharedString::from);
    let kind = absent.map_or(ui_model::SpeakerKind::Other, |member| member.kind);
    SpeakerItem {
        id: id.into(),
        device_name: name.clone(),
        name,
        kind: speaker_kind(kind),
        show_kind: !kind.is_this_pc(),
        state: RowState::Off,
        availability: Availability::Open,
        volume_step: 1.0,
        zone_id: zone_id.into(),
        zone_movable: true,
        missing: true,
        ..SpeakerItem::default()
    }
}

/// Whether a zone member's row is dimmed: as on the Mac, one that can't
/// play with its zone, whether it is listed or not.
fn zone_row_dimmed(row: ui_model::ZoneRow<'_>, note: ui_model::ZoneNote<'_>) -> bool {
    matches!(row, ui_model::ZoneRow::NotFound(_))
        || matches!(
            note,
            ui_model::ZoneNote::InTarget(_) | ui_model::ZoneNote::NeedsAirPlay2
        )
}

/// The words of a zone member's note.
fn zone_note(strings: &Strings<'_>, note: ui_model::ZoneNote<'_>) -> SharedString {
    match note {
        ui_model::ZoneNote::NeedsAirPlay2 => strings.get_needs_airplay2(),
        ui_model::ZoneNote::InTarget(target) => strings.invoke_zone_member_in_target(target.into()),
        ui_model::ZoneNote::NotFound => strings.get_zone_member_not_found(),
        ui_model::ZoneNote::CastStayedOff => strings.get_cast_stayed_off(),
    }
}

/// The entries of Speakers > Hidden Speakers.
fn hidden_items(state: &AppState) -> Vec<HiddenItem> {
    state
        .hidden
        .iter()
        .map(|hidden| HiddenItem {
            id: hidden.id.as_str().into(),
            name: hidden.name.as_str().into(),
            kind: hidden.kind_text.as_str().into(),
            present: hidden.present.is_some(),
        })
        .collect()
}

fn cast_group(speaker: &ui_model::Speaker) -> Option<&ui_model::CastGroup> {
    speaker.cast.as_ref()?.group.as_ref()
}

/// The note under a Cast group's name, which Narrator reads too: what it
/// is and its members as far as its session named them (spec C94). Empty
/// for any other speaker.
fn cast_group_note(strings: &Strings<'_>, speaker: &ui_model::Speaker) -> SharedString {
    let Some(group) = cast_group(speaker) else {
        return SharedString::default();
    };
    let (members, unknown) = match &group.members {
        ui_model::CastMembers::NotReported => (SharedString::default(), false),
        ui_model::CastMembers::Unknown => (SharedString::default(), true),
        ui_model::CastMembers::Known(names) => (joined(strings, names), names.is_empty()),
    };
    strings.invoke_cast_group_note(members, unknown)
}

/// The line a stereo pair or home theater group row shows after its kind,
/// which Narrator reads too: what it is and its members, or which member is
/// missing (spec P176, P181). Empty for any other row.
fn group_note(strings: &Strings<'_>, speaker: &ui_model::Speaker) -> SharedString {
    let Some(group) = &speaker.group else {
        return SharedString::default();
    };
    let kind = match group.kind {
        AirPlayGroupKind::StereoPair => SpeakerGroup::StereoPair,
        AirPlayGroupKind::HomeTheater => SpeakerGroup::HomeTheater,
    };
    strings.invoke_group_note(
        kind,
        joined(strings, &group.members),
        joined(strings, &group.missing),
        group.incomplete,
    )
}

/// A local output with an offset (not the reference, AirPlay or Cast) that
/// a microphone can hear: it can be calibrated with the microphone.
fn is_calibratable(speaker: &ui_model::Speaker) -> bool {
    speaker.delay_ms.is_some()
        && speaker.cast.is_none()
        && speaker.mic_left_out.is_none()
        && !matches!(
            speaker.kind,
            ui_model::SpeakerKind::AirPlay
                | ui_model::SpeakerKind::AirPlay2
                | ui_model::SpeakerKind::Cast
        )
}

/// What a calibration needs that is off, in the order streaming, `speaker`,
/// the reference; all three when `missing` names none.
fn calibration_needs(
    strings: &Strings<'_>,
    reference: &str,
    missing: Missing,
    speaker: SharedString,
) -> SharedString {
    let missing = if missing.is_empty() {
        Missing::ALL
    } else {
        missing
    };
    let reference = if reference.is_empty() {
        strings.get_calibration_own_speakers()
    } else {
        reference.into()
    };
    strings.invoke_calibration_needs(
        missing.streaming,
        if missing.speaker {
            speaker
        } else {
            SharedString::default()
        },
        if missing.reference {
            reference
        } else {
            SharedString::default()
        },
    )
}

/// Why a calibration of one speaker, or of all of them, can't start.
fn calibration_turn_on(strings: &Strings<'_>, reference: &str, missing: Missing) -> SharedString {
    let missing = if missing.is_empty() {
        Missing::ALL
    } else {
        missing
    };
    let needs = calibration_needs(
        strings,
        reference,
        missing,
        strings.get_calibration_this_speaker(),
    );
    strings.invoke_calibration_turn_on(needs, missing.speaker)
}

/// The line under a speaker's Calibrate button: what its calibration does,
/// how it ended, or why one can't start; empty when there is nothing to say.
fn calibration_status(
    strings: &Strings<'_>,
    state: &AppState,
    speaker: &ui_model::Speaker,
) -> SharedString {
    if !is_calibratable(speaker) {
        return SharedString::default();
    }
    let reference = state.reference_name.as_deref().unwrap_or_default();
    let shown = if state.calibration.speaker_id.as_deref() == Some(speaker.id.as_str()) {
        state.calibration.phase
    } else {
        Phase::Idle
    };
    match shown {
        Phase::Idle => calibration::unavailable(
            state.running,
            state.reference_name.is_some(),
            speaker.enabled,
        )
        .map_or_else(SharedString::default, |failure| {
            calibration_failure(strings, reference, failure)
        }),
        Phase::Running(Step::Preparing) => strings.get_calibration_preparing(),
        Phase::Running(Step::Measuring(Which::Reference)) => {
            strings.invoke_calibration_listening(reference.into())
        }
        Phase::Running(Step::Measuring(Which::Chosen)) => {
            strings.get_calibration_listening_chosen()
        }
        Phase::Running(Step::Estimating) => strings.get_calibration_estimating(),
        Phase::Done { offset_ms, clamped } => {
            let ms = offset_ms.round() as i32;
            if clamped {
                strings.invoke_calibration_done_clamped(ms)
            } else {
                strings.invoke_calibration_done(ms)
            }
        }
        Phase::Cancelled => strings.get_calibration_cancelled(),
        Phase::Failed(failure) => {
            strings.invoke_calibration_failed(calibration_failure(strings, reference, failure))
        }
    }
}

/// Why microphone calibration leaves an output out, in its delay controls.
fn calibration_left_out(strings: &Strings<'_>, kind: ui_model::MicKind) -> SharedString {
    match kind {
        ui_model::MicKind::Virtual => strings.get_calibration_left_out_virtual(),
        _ => strings.get_calibration_left_out_headphones(),
    }
}

/// Why Calibrate All leaves an output out, or that it is included anyway.
fn left_out_reason(strings: &Strings<'_>, row: &LeftOutRow) -> SharedString {
    match (row.included, row.kind) {
        (true, _) => strings.get_calibrate_all_included(),
        (false, kind) => calibration_left_out(strings, kind),
    }
}

/// The speaker whose calibration just ended, once per ending, so Narrator
/// hears how it ended without the focus moving to its line.
fn calibration_ended(
    announced: &mut Option<(String, Phase)>,
    calibration: &ui_model::CalibrationState,
) -> Option<String> {
    let ended = matches!(
        calibration.phase,
        Phase::Done { .. } | Phase::Failed(_) | Phase::Cancelled
    );
    let now = calibration
        .speaker_id
        .clone()
        .filter(|_| ended)
        .map(|id| (id, calibration.phase));
    if now == *announced {
        return None;
    }
    announced.clone_from(&now);
    now.map(|(id, _)| id)
}

/// What the Calibrate All card shows.
struct CalibrateAllCard {
    running: bool,
    can_start: bool,
    status: SharedString,
    privacy: bool,
    results_shown: bool,
    rows: Vec<CalibrationRowItem>,
    left_out: Vec<LeftOutItem>,
    airplay: SharedString,
    /// For a Calibrate Zone run: where to bring the PC.
    zone: SharedString,
    outcome: SharedString,
    can_apply: bool,
    can_undo: bool,
    /// What Narrator hears when the run, Apply or Undo ends.
    said: SharedString,
}

/// Whether a calibration the microphone refused (`denied`) offers Open
/// Privacy Settings: only where something can open that page (`can_open`,
/// [`crate::platform::can_open_settings`]), which Linux can't.
fn offers_privacy(denied: bool, can_open: impl Fn(&str) -> bool) -> bool {
    denied && can_open(SettingsPage::MicrophonePrivacy.uri())
}

fn calibrate_all_card(strings: &Strings<'_>, state: &AppState) -> CalibrateAllCard {
    let all = &state.calibrate_all;
    let busy = state.calibration.busy;
    let reference = state.reference_name.as_deref().unwrap_or_default();
    let status = match &all.phase {
        AllPhase::Running(progress) => all_progress_text(strings, progress),
        AllPhase::Failed(Failure::NothingToMeasure) if all.zone.is_some() => {
            let zone = all.zone.as_ref().map_or("", |zone| zone.name.as_str());
            strings.invoke_calibrate_all_stopped(strings.invoke_calibrate_zone_nothing(zone.into()))
        }
        AllPhase::Failed(failure) => {
            strings.invoke_calibrate_all_stopped(calibration_failure(strings, reference, *failure))
        }
        AllPhase::Stale => strings.get_calibrate_all_stale(),
        AllPhase::CannotMeasureAgain(name, missing) => strings.invoke_calibrate_all_cannot_again(
            calibration_needs(strings, reference, *missing, name.as_str().into()),
        ),
        AllPhase::Idle if all.other_running => strings.get_calibrate_all_busy(),
        AllPhase::Idle => match all.blocked {
            Some(Failure::NoReference) => strings.get_calibration_no_reference(),
            Some(Failure::NothingToMeasure) => {
                strings.invoke_calibrate_all_nothing(reference.into())
            }
            Some(Failure::OnlyLeftOut) => strings.get_calibrate_all_only_left_out(),
            Some(failure) => calibration_failure(strings, reference, failure),
            None => strings.invoke_calibrate_all_hint(reference.into()),
        },
    };
    let failed = matches!(
        all.phase,
        AllPhase::Failed(_) | AllPhase::Stale | AllPhase::CannotMeasureAgain(..)
    );
    let sheet = all.sheet.as_ref();
    let outcome = sheet.map_or_else(SharedString::default, |sheet| {
        sheet_outcome(strings, reference, sheet)
    });
    let said = if failed || outcome.is_empty() {
        status.clone()
    } else {
        outcome.clone()
    };
    CalibrateAllCard {
        running: matches!(all.phase, AllPhase::Running(_)),
        can_start: !busy && all.blocked.is_none(),
        privacy: offers_privacy(
            all.phase == AllPhase::Failed(Failure::MicrophoneDenied)
                || sheet.is_some_and(|sheet| sheet.failure == Some(Failure::MicrophoneDenied)),
            crate::platform::can_open_settings,
        ),
        results_shown: sheet.is_some(),
        left_out: all
            .left_out
            .iter()
            .map(|row| LeftOutItem {
                name: row.name.as_str().into(),
                reason: left_out_reason(strings, row),
                help: match row.kind {
                    ui_model::MicKind::Virtual => strings.get_include_anyway_help_virtual(),
                    _ => strings.get_include_anyway_help_headphones(),
                },
                included: row.included,
            })
            .collect(),
        rows: sheet.map_or_else(Vec::new, |sheet| {
            let off = all.zone.as_ref().map_or(&[][..], |zone| &zone.off_rows);
            sheet
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| CalibrationRowItem {
                    name: row.name.as_str().into(),
                    detail: if off.contains(&index) {
                        strings.get_calibrate_all_off()
                    } else {
                        row_detail(strings, reference, row)
                    },
                    can_measure_again: row.can_measure_again && !busy,
                })
                .collect()
        }),
        airplay: sheet.and_then(|sheet| sheet.airplay.as_ref()).map_or_else(
            SharedString::default,
            |shared| {
                let line = airplay_line(strings, shared);
                match all
                    .zone
                    .as_ref()
                    .and_then(|zone| airplay_moves_line(strings, &zone.airplay_moves))
                {
                    Some(moves) => format!("{line} {moves}").into(),
                    None => line,
                }
            },
        ),
        zone: all
            .zone
            .as_ref()
            .map_or_else(SharedString::default, |zone| {
                strings.invoke_calibrate_zone_bring(zone.name.as_str().into())
            }),
        outcome,
        can_apply: sheet.is_some_and(|sheet| sheet.can_apply),
        can_undo: sheet.is_some_and(|sheet| sheet.can_undo),
        status,
        said,
    }
}

fn all_progress_text(strings: &Strings<'_>, progress: &AllProgress) -> SharedString {
    match progress {
        AllProgress::Preparing => strings.get_calibration_preparing(),
        AllProgress::Measuring {
            name,
            reference: true,
            ..
        } => strings.invoke_calibration_listening(name.as_str().into()),
        AllProgress::Measuring {
            name,
            searching: true,
            ..
        } => strings.invoke_calibrate_all_searching(name.as_str().into()),
        AllProgress::Measuring { name, take, .. } => strings
            .invoke_calibrate_all_sweep(name.as_str().into(), i32::try_from(*take).unwrap_or(1)),
        AllProgress::Estimating => strings.get_calibrate_all_estimating(),
    }
}

fn ended_text(strings: &Strings<'_>, reason: EndReason) -> SharedString {
    match reason {
        EndReason::Cancelled => strings.get_calibrate_all_ended_cancelled(),
        EndReason::SpeakersChanged => strings.get_calibrate_all_ended_speakers(),
        EndReason::SpeakerTurnedOff => strings.get_calibrate_all_ended_off(),
        EndReason::SourceChanged => strings.get_calibrate_all_ended_source(),
    }
}

/// The line under the results: how the run, Apply, Undo or the last
/// Measure Again ended.
fn sheet_outcome(strings: &Strings<'_>, reference: &str, sheet: &SheetState) -> SharedString {
    if let Some(failure) = sheet.failure {
        return strings
            .invoke_calibrate_all_stopped(calibration_failure(strings, reference, failure));
    }
    if let Some(reason) = sheet.ended {
        return strings.invoke_calibrate_all_stopped(ended_text(strings, reason));
    }
    if sheet.undone {
        return strings.get_calibrate_all_undone();
    }
    if sheet.applied {
        return strings.get_calibrate_all_applied();
    }
    if let Some(reason) = sheet.again_ended {
        return strings.invoke_calibrate_all_again_stopped(ended_text(strings, reason));
    }
    let measured = sheet
        .rows
        .iter()
        .filter(|row| row.state != SheetRowState::Reference);
    let clear = measured
        .clone()
        .filter(|row| row.state.is_certain())
        .count();
    if clear == 0 {
        return strings.get_calibrate_all_nothing_certain();
    }
    strings.invoke_calibrate_all_finished(count(clear), count(measured.count()))
}

/// A delay as its field shows it: a local speaker's relative to the
/// reference, with its sign; a Cast or AirPlay delay as is.
fn sheet_ms(strings: &Strings<'_>, class: Class, ms: f64) -> SharedString {
    let ms = ms.round() as i32;
    match class {
        Class::Local => strings.invoke_delay_value(ms),
        Class::Cast | Class::AirPlay => strings.invoke_calibrate_all_ms(ms),
    }
}

fn row_detail(strings: &Strings<'_>, reference: &str, row: &SheetRow) -> SharedString {
    let old = row.old_ms.map_or_else(
        || strings.get_calibrate_all_none(),
        |ms| sheet_ms(strings, row.class, ms),
    );
    let measured = |m: &Measurement| {
        (
            old.clone(),
            sheet_ms(strings, row.class, m.new_ms),
            m.uncertainty_ms.ceil().max(1.0) as i32,
        )
    };
    match &row.state {
        SheetRowState::Reference => strings.get_calibrate_all_reference(),
        SheetRowState::NotMeasured => strings.get_calibrate_all_not_measured(),
        SheetRowState::Certain(m) => {
            let (old, new, uncertainty) = measured(m);
            if m.clamped {
                strings.invoke_calibrate_all_clamped(old, new, uncertainty)
            } else {
                strings.invoke_calibrate_all_certain(old, new, uncertainty)
            }
        }
        SheetRowState::Uncertain(m) => {
            let (old, new, uncertainty) = measured(m);
            strings.invoke_calibrate_all_uncertain(old, new, uncertainty)
        }
        SheetRowState::Refused(refused) => match calibration::outcome(&Err(*refused)) {
            calibration::Outcome::Refuse(failure) => strings
                .invoke_calibrate_all_refused(calibration_failure(strings, reference, failure)),
            calibration::Outcome::Commit { .. } => strings.get_calibrate_all_not_measured(),
        },
    }
}

fn airplay_line(strings: &Strings<'_>, shared: &AirPlayShared) -> SharedString {
    let old = shared.old_ms.map_or_else(
        || strings.get_calibrate_all_none(),
        |ms| sheet_ms(strings, Class::AirPlay, ms),
    );
    let new = shared.new_ms.unwrap_or(0.0).round() as i32;
    let spread = shared.spread_ms.round() as i32;
    let set = if shared.certain_rows == 1 {
        strings.invoke_calibrate_all_airplay_one(old.clone(), new)
    } else {
        strings.invoke_calibrate_all_airplay_median(
            old.clone(),
            new,
            count(shared.certain_rows),
            spread,
        )
    };
    match shared.state {
        SharedState::NoneCertain => strings.invoke_calibrate_all_airplay_none(old),
        SharedState::Certain => set,
        SharedState::Spread => {
            let differ = strings.invoke_calibrate_all_airplay_spread(spread);
            format!("{set} {differ}").into()
        }
    }
}

/// Under a zone run's AirPlay line: the other zones the shared delay moves,
/// and the speakers in no zone (spec S255); `None` when it moves no other.
fn airplay_moves_line(strings: &Strings<'_>, moves: &AirPlayMoves) -> Option<SharedString> {
    let quoted: Vec<String> = moves.zones.iter().map(|name| format!("“{name}”")).collect();
    let named = (!quoted.is_empty()).then(|| joined(strings, &quoted));
    match (named, moves.no_zone) {
        (None, false) => None,
        (None, true) => Some(strings.get_calibrate_zone_airplay_no_zone()),
        (Some(named), false) => Some(strings.invoke_calibrate_zone_airplay_zones(named)),
        (Some(named), true) => Some(strings.invoke_calibrate_zone_airplay_both(named)),
    }
}

fn calibration_failure(strings: &Strings<'_>, reference: &str, failure: Failure) -> SharedString {
    let subject = |which: Option<Which>| -> SharedString {
        match which {
            Some(Which::Reference) if !reference.is_empty() => reference.into(),
            Some(Which::Chosen) => strings.get_calibration_this_speaker(),
            _ => strings.get_calibration_a_speaker(),
        }
    };
    match failure {
        Failure::NoReference => strings.get_calibration_no_reference(),
        Failure::NotPlaying(missing) => calibration_turn_on(strings, reference, missing),
        Failure::MicrophoneDenied => strings.get_calibration_microphone_denied(),
        Failure::NoMicrophone => strings.get_calibration_no_microphone(),
        Failure::UnsupportedRate => strings.get_calibration_unsupported_rate(),
        Failure::SourceSilent => strings.get_calibration_source_silent(),
        Failure::Interrupted => strings.get_calibration_interrupted(),
        Failure::MicrophoneChanged => strings.get_calibration_microphone_changed(),
        Failure::MicrophoneSkipped => strings.get_calibration_microphone_skipped(),
        Failure::HandsFreeMicrophone => strings.get_calibration_hands_free(),
        Failure::MicrophoneInUse => strings.get_calibration_microphone_in_use(),
        Failure::Unfinished => strings.get_calibration_unfinished(),
        Failure::NothingToMeasure => strings.invoke_calibrate_all_nothing(reference.into()),
        Failure::OnlyLeftOut => strings.get_calibrate_all_only_left_out(),
        Failure::TooShort(_) => strings.get_calibration_too_short(),
        Failure::TooLong(_) => strings.get_calibration_too_long(),
        Failure::Clipped(which) => strings.invoke_calibration_clipped(subject(which)),
        Failure::TooQuiet(which) => strings.invoke_calibration_too_quiet(subject(which)),
        Failure::AmbiguousEcho(which) => strings.invoke_calibration_echo(subject(which)),
        Failure::Implausible => strings.get_calibration_implausible(),
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
        has_privacy: matches!(notice.message, NoticeMessage::MicrophoneBlocked { .. }),
        update_buttons: update_buttons(&notice.message),
        info: notice.message.reports_a_change(),
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

/// A source in the UI's words: what the double-audio notice and its guide
/// name it with.
fn source_name(strings: &Strings<'_>, source: &SourceName) -> SharedString {
    match source {
        SourceName::Named(name) => name.as_str().into(),
        SourceName::Apps(names) => joined(strings, names),
        SourceName::AllAppsExcept(names) => {
            strings.invoke_source_all_except(joined(strings, names))
        }
    }
}

/// A source or channel pair as the source picker names it.
fn switch_name(strings: &Strings<'_>, name: &SwitchName) -> SharedString {
    match name {
        SwitchName::System => strings.get_source_system(),
        SwitchName::Source(source) => source_name(strings, source),
        SwitchName::Channels { first, last } => {
            strings.invoke_input_channel_pair(i32::from(*first), i32::from(*last))
        }
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
        Some(source) => strings.invoke_guide_intro_app(source_name(strings, source), outputs),
        None => strings.invoke_guide_intro_system(outputs),
    }
}

/// A guide step in the UI's words.
fn guide_step_text(strings: &Strings<'_>, step: &GuideStep) -> SharedString {
    match step {
        GuideStep::RouteApp {
            app: SourceName::AllAppsExcept(names),
        } => strings.invoke_guide_route_all_except(joined(strings, names)),
        GuideStep::RouteApp { app } => strings.invoke_guide_route_app(source_name(strings, app)),
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
        GuideStep::KeepAppUnmuted {
            app: SourceName::AllAppsExcept(names),
        } => strings.invoke_guide_keep_all_except_unmuted(joined(strings, names)),
        GuideStep::KeepAppUnmuted { app } => {
            strings.invoke_guide_keep_app_unmuted(source_name(strings, app))
        }
    }
}

/// The UI's name of a step's Settings page; none when nothing installed
/// could show it, so the step has no button.
fn guide_page(page: Option<SettingsPage>) -> GuidePage {
    if page.is_some_and(|page| !crate::platform::can_open_settings(page.uri())) {
        return GuidePage::None;
    }
    match page {
        None | Some(SettingsPage::MicrophonePrivacy) => GuidePage::None,
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
        NoticeMessage::SourceSwitched { source } => {
            strings.invoke_notice_source_switched(source.as_str().into())
        }
        NoticeMessage::SwitchFailed { new, old } => strings
            .invoke_notice_switch_failed(switch_name(strings, new), switch_name(strings, old)),
        NoticeMessage::SourceNotRunning { source } => {
            strings.invoke_notice_source_not_running(source.as_str().into())
        }
        NoticeMessage::SourceRefused { source } => {
            strings.invoke_notice_source_refused(source.as_str().into())
        }
        NoticeMessage::InputNotConnected { source } => {
            strings.invoke_notice_input_not_connected(source.as_str().into())
        }
        NoticeMessage::InputDisconnected { source } => {
            strings.invoke_notice_input_disconnected(source.as_str().into())
        }
        NoticeMessage::MicrophoneBlocked { source } => {
            strings.invoke_notice_microphone_blocked(source.as_str().into())
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
        NoticeMessage::PairFirst { speaker } => {
            strings.invoke_notice_pair_first(speaker.as_str().into())
        }
        NoticeMessage::AirPlayRetrying { speaker, problem }
        | NoticeMessage::AirPlayCantPlay { speaker, problem } => {
            strings.invoke_notice_airplay(airplay_problem(*problem), speaker.as_str().into())
        }
        NoticeMessage::AirPlayPlaysInGroup { speaker, target } => strings
            .invoke_notice_airplay_plays_in_group(speaker.as_str().into(), target.as_str().into()),
        NoticeMessage::AirPlayMemberNeedsPassword { target, member } => strings
            .invoke_notice_airplay_member_password(member.as_str().into(), target.as_str().into()),
        NoticeMessage::CastCantPlay { speaker, problem } => {
            strings.invoke_notice_cast(cast_problem(*problem), speaker.as_str().into())
        }
        NoticeMessage::AirPlayAheadOfCast { speaker, tenths } => {
            let seconds = format!("{}.{}", tenths / 10, tenths % 10);
            strings.invoke_notice_airplay_ahead(seconds.into(), speaker.as_str().into())
        }
        NoticeMessage::AirPlayStopped => strings.get_notice_airplay_stopped(),
        NoticeMessage::AirPlayPtpPortsTaken => strings.get_notice_airplay_ptp_ports_taken(),
        NoticeMessage::SettingsNotSaved => strings.get_notice_settings_not_saved(),
        NoticeMessage::PairingsNotSaved => strings.get_notice_pairings_not_saved(),
        NoticeMessage::SettingsKeptAside { file } => {
            strings.invoke_notice_settings_kept_aside(file.as_str().into())
        }
        NoticeMessage::PairingsKeptAside { file } => {
            strings.invoke_notice_pairings_kept_aside(file.as_str().into())
        }
        NoticeMessage::NoSoundOutput => strings.get_notice_no_sound_output(),
        NoticeMessage::LaunchAtLoginFailed => strings.get_notice_launch_at_login_failed(),
        NoticeMessage::LaunchAtLoginNotRemoved => strings.get_notice_launch_at_login_not_removed(),
        NoticeMessage::LaunchAtLoginOffInSettings => {
            strings.get_notice_launch_at_login_off_in_settings()
        }
        NoticeMessage::AirPlay2NextLaunch { on, prefer } => match (on, prefer) {
            (true, false) => strings.get_notice_airplay2_on_next_launch(),
            (false, false) => strings.get_notice_airplay2_off_next_launch(),
            (true, true) => strings.get_notice_airplay2_prefer_on_next_launch(),
            (false, true) => strings.get_notice_airplay2_prefer_off_next_launch(),
        },
        NoticeMessage::RemoteOnPublicNetwork => strings.get_remote_public_network(),
        NoticeMessage::GroupsBecameZones => strings.get_notice_groups_became_zones(),
        NoticeMessage::Update(notice) => update_notice_text(strings, notice),
        NoticeMessage::PresetLeftOff {
            preset,
            left_off,
            nothing_changed,
        } => {
            let members: Vec<String> = left_off
                .iter()
                .map(|(name, reason)| {
                    strings
                        .invoke_preset_member_left_off(
                            name.as_str().into(),
                            preset_left_off(strings, reason),
                        )
                        .to_string()
                })
                .collect();
            // Commas only, as on the Mac: each item already ends in its reason.
            let list: SharedString =
                join_names(&members, |head, last| format!("{head}, {last}")).into();
            if *nothing_changed {
                strings.invoke_preset_nothing_changed(preset.as_str().into(), list)
            } else {
                strings.invoke_preset_left_off(preset.as_str().into(), list)
            }
        }
        NoticeMessage::HeardTwice {
            source,
            mute,
            shared,
            ..
        } => {
            let mute = joined(strings, mute);
            let shared = joined(strings, shared);
            match source {
                Some(source) => strings.invoke_notice_heard_twice_app(
                    source_name(strings, source),
                    mute,
                    shared,
                ),
                None => strings.invoke_notice_heard_twice_system(mute, shared),
            }
        }
    }
}

/// Why a preset's member stayed off, as the preset notice says it.
fn preset_left_off(strings: &Strings<'_>, reason: &PresetLeftOff) -> SharedString {
    match reason {
        PresetLeftOff::NotFound => strings.get_zone_member_not_found(),
        PresetLeftOff::Hidden => strings.get_preset_hidden(),
        PresetLeftOff::NeedsAirPlay2 => strings.get_needs_airplay2(),
        PresetLeftOff::InTarget(target) => {
            strings.invoke_zone_member_in_target(target.as_str().into())
        }
        PresetLeftOff::Busy => strings.get_preset_busy(),
        PresetLeftOff::CantPlay => strings.get_preset_cant_play(),
    }
}

/// The presets as the Presets card, View > Presets and the flyout list them.
fn preset_items(state: &AppState) -> Vec<PresetItem> {
    state
        .presets
        .iter()
        .map(|preset| PresetItem {
            id: preset.id.as_str().into(),
            name: preset.name.as_str().into(),
            current: state.current_preset.as_deref() == Some(preset.id.as_str()),
        })
        .collect()
}

/// The current preset's name; empty while none is current.
fn current_preset_name(state: &AppState) -> &str {
    state
        .presets
        .iter()
        .find(|preset| state.current_preset.as_deref() == Some(preset.id.as_str()))
        .map_or("", |preset| preset.name.as_str())
}

/// A few words that name a notice, for its dismiss button.
fn notice_title(strings: &Strings<'_>, message: &NoticeMessage) -> SharedString {
    let (kind, name) = match message {
        NoticeMessage::NoSpeaker => (NoticeKind::NoSpeaker, ""),
        NoticeMessage::SourceQuit { source } => (NoticeKind::SourceQuit, source.as_str()),
        NoticeMessage::SourceSwitched { source } => (NoticeKind::SourceSwitched, source.as_str()),
        NoticeMessage::SwitchFailed { new, .. } => {
            return strings
                .invoke_notice_title(NoticeKind::SwitchFailed, switch_name(strings, new));
        }
        NoticeMessage::SourceNotRunning { source } => {
            (NoticeKind::SourceNotRunning, source.as_str())
        }
        NoticeMessage::SourceRefused { source } => (NoticeKind::SourceRefused, source.as_str()),
        NoticeMessage::InputNotConnected { source } => {
            (NoticeKind::InputNotConnected, source.as_str())
        }
        NoticeMessage::InputDisconnected { source } => {
            (NoticeKind::InputDisconnected, source.as_str())
        }
        NoticeMessage::MicrophoneBlocked { .. } => (NoticeKind::MicrophoneBlocked, ""),
        NoticeMessage::CaptureFailed => (NoticeKind::CaptureFailed, ""),
        NoticeMessage::CaptureLost => (NoticeKind::CaptureLost, ""),
        NoticeMessage::SpeakerFailed { speaker } => (NoticeKind::SpeakerFailed, speaker.as_str()),
        NoticeMessage::SpeakerLost { speaker } => (NoticeKind::SpeakerLost, speaker.as_str()),
        NoticeMessage::SpeakerBusy { speaker } => (NoticeKind::SpeakerBusy, speaker.as_str()),
        NoticeMessage::PairFirst { speaker } => (NoticeKind::PairFirst, speaker.as_str()),
        NoticeMessage::AirPlayRetrying {
            speaker,
            problem:
                ui_model::AirPlayProblem::TooManyAirPlay2Speakers
                | ui_model::AirPlayProblem::PtpOtherNetwork,
        } => (NoticeKind::AirplayWaiting, speaker.as_str()),
        NoticeMessage::AirPlayRetrying { speaker, .. } => {
            (NoticeKind::AirplayRetrying, speaker.as_str())
        }
        NoticeMessage::AirPlayCantPlay { speaker, .. } => {
            (NoticeKind::AirplayCantPlay, speaker.as_str())
        }
        NoticeMessage::AirPlayMemberNeedsPassword { target, .. } => {
            (NoticeKind::AirplayCantPlay, target.as_str())
        }
        NoticeMessage::AirPlayPlaysInGroup { speaker, .. } => {
            (NoticeKind::AirplayCantPlay, speaker.as_str())
        }
        NoticeMessage::CastCantPlay { speaker, .. } => (NoticeKind::CastCantPlay, speaker.as_str()),
        NoticeMessage::AirPlayAheadOfCast { speaker, .. } => {
            (NoticeKind::AirplayAheadOfCast, speaker.as_str())
        }
        NoticeMessage::AirPlayStopped => (NoticeKind::AirplayStopped, ""),
        NoticeMessage::AirPlayPtpPortsTaken => (NoticeKind::AirplayPtpPortsTaken, ""),
        NoticeMessage::SettingsNotSaved => (NoticeKind::SettingsNotSaved, ""),
        NoticeMessage::PairingsNotSaved => (NoticeKind::PairingsNotSaved, ""),
        NoticeMessage::SettingsKeptAside { .. } => (NoticeKind::SettingsKeptAside, ""),
        NoticeMessage::PairingsKeptAside { .. } => (NoticeKind::PairingsKeptAside, ""),
        NoticeMessage::NoSoundOutput => (NoticeKind::NoSoundOutput, ""),
        NoticeMessage::LaunchAtLoginFailed => (NoticeKind::LaunchAtLoginFailed, ""),
        NoticeMessage::LaunchAtLoginNotRemoved => (NoticeKind::LaunchAtLoginNotRemoved, ""),
        NoticeMessage::LaunchAtLoginOffInSettings => (NoticeKind::LaunchAtLoginOffInSettings, ""),
        NoticeMessage::AirPlay2NextLaunch { .. } => (NoticeKind::Airplay2NextLaunch, ""),
        NoticeMessage::RemoteOnPublicNetwork => (NoticeKind::RemotePublicNetwork, ""),
        NoticeMessage::GroupsBecameZones => (NoticeKind::GroupsBecameZones, ""),
        NoticeMessage::Update(_) => (NoticeKind::Update, ""),
        NoticeMessage::PresetLeftOff { .. } => (NoticeKind::PresetsLeftOff, ""),
        NoticeMessage::HeardTwice { .. } => (NoticeKind::HeardTwice, ""),
    };
    strings.invoke_notice_title(kind, name.into())
}

/// The updater's notice in the UI's words (spec S308).
fn update_notice_text(strings: &Strings<'_>, notice: &ui_model::UpdaterNotice) -> SharedString {
    use ui_model::UpdaterNotice as N;
    match notice {
        N::Available {
            new,
            running,
            last: None,
        } => strings.invoke_notice_update_available(new.into(), running.into()),
        N::Available {
            new,
            running,
            last: Some(ui_model::UpdateRecordReason::Blocked),
        } => strings.invoke_notice_update_available_blocked_before(new.into(), running.into()),
        N::Available {
            new,
            running,
            last: Some(ui_model::UpdateRecordReason::FailedStart),
        } => strings.invoke_notice_update_available_failed_before(new.into(), running.into()),
        N::Downloading {
            percent: Some(percent),
        } => strings.invoke_notice_update_downloading(percent.to_string().into()),
        N::Checking => strings.get_notice_update_checking(),
        N::Downloading { percent: None } => strings.get_notice_update_downloading_started(),
        N::Installed { version } => strings.invoke_notice_update_installed(version.into()),
        N::UpToDate { running } => strings.invoke_notice_update_up_to_date(running.into()),
        N::Unreachable => strings.get_notice_update_unreachable(),
        N::DownloadFailed => strings.get_notice_update_download_failed(),
        N::BadSignature => strings.get_notice_update_bad_signature(),
        N::BrokenDownload => strings.get_notice_update_broken(),
        N::DiskFull => strings.get_notice_update_disk_full(),
        N::ReadOnly => strings.get_notice_update_read_only(),
        N::InUse => strings.get_notice_update_in_use(),
        N::Failed => strings.get_notice_update_failed(),
        N::RestoreFailed { renames } => {
            let renames: Vec<String> = renames
                .iter()
                .map(|(bak, live)| {
                    strings
                        .invoke_notice_update_rename(bak.into(), live.into())
                        .to_string()
                })
                .collect();
            strings.invoke_notice_update_restore_failed(joined(strings, &renames))
        }
        N::RelaunchFailed => strings.get_notice_update_relaunch_failed(),
        N::Blocked => strings.get_notice_update_blocked(),
    }
}

/// The updater's notice's buttons as the UI names them.
fn update_buttons(message: &NoticeMessage) -> ModelRc<UpdateButton> {
    use ui_model::UpdaterButton as B;
    let NoticeMessage::Update(notice) = message else {
        return ModelRc::default();
    };
    let buttons: Vec<UpdateButton> = notice
        .buttons()
        .iter()
        .map(|button| match button {
            B::Download => UpdateButton::Download,
            B::NotNow => UpdateButton::NotNow,
            B::Cancel => UpdateButton::Cancel,
            B::TryAgain => UpdateButton::TryAgain,
            B::OpenReleasePage => UpdateButton::OpenReleasePage,
            B::RestartNow => UpdateButton::RestartNow,
        })
        .collect();
    ModelRc::new(VecModel::from(buttons))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::{MOCK_CAST_HOLD, MockBackend};
    use std::sync::PoisonError;

    use slint::platform::software_renderer::{MinimalSoftwareWindow, Rgb565Pixel};
    use slint::platform::{Key, PointerEventButton, WindowEvent};

    #[test]
    fn the_menu_bar_hears_a_theme_once_per_native_window() {
        let theme = MenuBarTheme::default();
        assert!(theme.due(7, true), "the first theme is sent");
        // The focus muda's activation toggle brings back must not send it again.
        assert!(!theme.due(7, true));
        assert!(!theme.due(7, true));
        assert!(theme.due(7, false), "a new appearance is sent");
        assert!(theme.due(8, false), "a new native window gets it too");
        assert!(!theme.due(8, false));
        theme.forget();
        assert!(theme.due(8, false), "after Windows repainted the menu bar");
    }

    /// A platform without native windows, so a test can build the whole App
    /// and drive it through the windows' callbacks or keys.
    struct Headless;

    thread_local! {
        /// The windows the headless platform made on this thread.
        static HEADLESS_WINDOWS: RefCell<Vec<Weak<MinimalSoftwareWindow>>> =
            RefCell::default();
        /// Slint's time on this thread once the test holds its clock.
        static HELD_CLOCK: Cell<Option<Duration>> = const { Cell::new(None) };
    }

    static CLOCK_START: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);

    impl slint::platform::Platform for Headless {
        fn create_window_adapter(
            &self,
        ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            let window = MinimalSoftwareWindow::new(Default::default());
            HEADLESS_WINDOWS.with(|windows| windows.borrow_mut().push(Rc::downgrade(&window)));
            Ok(window)
        }

        fn duration_since_start(&self) -> Duration {
            HELD_CLOCK
                .with(Cell::get)
                .unwrap_or_else(|| CLOCK_START.elapsed())
        }
    }

    /// Stops Slint's clock on this thread for the rest of the test: the
    /// double-click timers then see two clicks as one double-click however
    /// long a loaded machine takes between them.
    fn hold_clock() {
        HELD_CLOCK.with(|clock| clock.set(Some(CLOCK_START.elapsed())));
    }

    /// Moves the clock [`hold_clock`] stopped on by `by`.
    fn advance_clock(by: Duration) {
        HELD_CLOCK.with(|clock| clock.set(clock.get().map(|now| now + by)));
    }

    /// Draws a frame of every headless window, as the event loop would after
    /// an event: lists are laid out then, and their new rows built.
    pub(super) fn draw_frames() {
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
    pub(super) fn press(window: &slint::Window, key: impl Into<SharedString>) {
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

    /// Turns the mouse wheel over `x`, `y` by `delta_y` logical pixels
    /// (negative scrolls down), then draws a frame.
    fn scroll(window: &slint::Window, x: f32, y: f32, delta_y: f32) {
        let position = slint::LogicalPosition::new(x, y);
        window.dispatch_event(WindowEvent::PointerMoved { position });
        window.dispatch_event(WindowEvent::PointerScrolled {
            position,
            delta_x: 0.0,
            delta_y,
        });
        draw_frames();
    }

    /// Presses Tab `times` times in `window`.
    pub(super) fn tab(window: &slint::Window, times: usize) {
        for _ in 0..times {
            press(window, Key::Tab);
        }
    }

    /// The App over `backend`, with headless windows.
    pub(super) fn app_over(backend: impl Backend + 'static) -> Rc<App> {
        app_logging(backend, DebugLog::new(None))
    }

    /// [`app_over`] writing to `log`.
    pub(super) fn app_logging(backend: impl Backend + 'static, log: DebugLog) -> Rc<App> {
        // The platform is set per thread: a thread that already built an
        // App has it, which is fine.
        let _ = slint::platform::set_platform(Box::new(Headless));
        App::new(Box::new(backend), log).expect("the windows build headless")
    }

    #[test]
    fn launch_at_sign_in_follows_the_file_menu_and_its_notice_is_dismissed_here() {
        let app = app_over(MockBackend::new());
        let store = launch_at_login::MemoryStore::default();
        *app.login.borrow_mut() = LaunchAtLogin::new(
            Box::new(store.clone()),
            Some(std::path::PathBuf::from(r"C:\Apps\audiosync.exe")),
        );
        app.refresh();
        assert!(!app.main.get_launch_at_login());

        app.main.invoke_launch_at_login_toggled(true);
        assert!(app.main.get_launch_at_login());
        assert!(store.entry().run.is_some());

        // Task Manager turns it off; the next reading shows it.
        store.entry().approval = Some(vec![3; 12]);
        app.reread_launch_at_login();
        assert!(!app.main.get_launch_at_login());

        store.entry().refuse_writes = true;
        store.entry().approval = None;
        store.entry().run = None;
        app.reread_launch_at_login();
        app.main.invoke_launch_at_login_toggled(true);
        let notices = app.snapshot().notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].message, NoticeMessage::LaunchAtLoginFailed);
        assert_eq!(app.main.get_notices().row_count(), 1);

        app.main
            .invoke_notice_dismissed(launch_at_login::NOTICE_ID as i32);
        assert!(app.snapshot().notices.is_empty());
        assert_eq!(app.main.get_notices().row_count(), 0);
    }

    #[test]
    fn an_input_device_with_more_than_two_channels_offers_its_pairs_in_both_windows() {
        let app = app_over(MockBackend::new());
        let labels = |model: ModelRc<SharedString>| -> Vec<String> {
            (0..model.row_count())
                .filter_map(|row| model.row_data(row))
                .map(String::from)
                .collect()
        };
        assert_eq!(
            app.main.get_input_pairs().row_count(),
            0,
            "not for the whole system"
        );

        let line_in = app
            .snapshot()
            .sources
            .iter()
            .position(|source| source.id == "input:line")
            .expect("listed");
        app.main.invoke_source_selected(line_in as i32);
        let offered = ["Channels 1-2", "Channels 3-4", "Channel 5"];
        assert_eq!(labels(app.main.get_input_pairs()), offered);
        assert_eq!(labels(app.flyout.get_input_pairs()), offered);
        assert_eq!(app.main.get_input_pair_index(), 0);

        app.flyout.invoke_input_pair_selected(2);
        assert_eq!(app.snapshot().input_pair, 2);
        assert_eq!(app.main.get_input_pair_index(), 2);
        assert_eq!(app.flyout.get_input_pair_index(), 2);

        app.main.invoke_source_selected(0);
        assert_eq!(app.main.get_input_pairs().row_count(), 0);
        assert_eq!(app.main.get_input_pair_index(), 0);
    }

    #[test]
    fn a_saved_pair_the_device_no_longer_has_shows_channels_one_and_two() {
        let mut state = MockBackend::new().snapshot();
        state.source_id = "input:line".into();
        state.input_pair = 7;
        let (labels, index) =
            input_pair_choice(&state, |first, last| format!("{first}-{last}").into());
        assert_eq!(labels, ["1-2", "3-4", "5-5"]);
        assert_eq!(index, 0);
        state
            .sources
            .iter_mut()
            .for_each(|source| source.channels = 2);
        assert_eq!(
            input_pair_choice(&state, |_, _| "x".into()),
            (Vec::new(), 0)
        );
    }

    #[test]
    fn turning_airplay2_on_says_it_applies_at_the_next_launch_until_turned_back() {
        let app = app_over(MockBackend::new());
        assert!(app.snapshot().notices.is_empty());

        app.main.invoke_airplay2_experimental_toggled(true);
        let notices = app.snapshot().notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].message,
            NoticeMessage::AirPlay2NextLaunch {
                on: true,
                prefer: false
            }
        );
        assert_eq!(app.main.get_notices().row_count(), 1);
        let strings = app.main.global::<Strings>();
        assert_eq!(
            notice_item(&strings, &notices[0]).message,
            "AirPlay 2 (Experimental) is on. AudioSync plays to AirPlay 2 speakers, stereo pairs and home theater groups once you quit and reopen it."
        );
        // Under the AirPlay 2 flag the item is Prefer AirPlay 2 (spec S313).
        let mut preferred = notices[0].clone();
        preferred.message = NoticeMessage::AirPlay2NextLaunch {
            on: false,
            prefer: true,
        };
        assert_eq!(
            notice_item(&strings, &preferred).message,
            "Prefer AirPlay 2 is off. Speakers that play both AirPlay 1 and AirPlay 2 go back to AirPlay 1 once you quit and reopen AudioSync."
        );

        app.main.invoke_airplay2_experimental_toggled(false);
        assert!(app.snapshot().notices.is_empty());
        assert_eq!(app.main.get_notices().row_count(), 0);

        app.main.invoke_airplay2_experimental_toggled(true);
        app.main.invoke_notice_dismissed(AIRPLAY2_NOTICE_ID as i32);
        assert!(app.snapshot().notices.is_empty());
        assert!(
            app.ui.borrow().airplay2_experimental,
            "dismissing keeps it on"
        );
    }

    // covers: P213
    #[test]
    fn the_now_playing_play_pause_tells_the_backend_what_the_card_showed() {
        use crate::now_playing_card::fake::{FakeControls, track};
        let backend = MockBackend::new();
        let pressed = backend.play_pauses.clone();
        let app = app_over(backend);
        let fake = FakeControls::default();
        app.set_media_controls(fake.controls());
        app.main.show().expect("the main window shows");
        app.poll();
        app.main.invoke_now_playing_transport(1);
        assert!(pressed.borrow().is_empty(), "no session shown");

        fake.read(Some(track("Spotify.exe", "Song", "", true)));
        app.poll();
        app.main.invoke_now_playing_transport(0);
        app.main.invoke_now_playing_transport(1);
        app.main.invoke_now_playing_transport(2);
        fake.read(Some(track("Spotify.exe", "Song", "", false)));
        app.poll();
        app.main.invoke_now_playing_transport(1);
        assert_eq!(
            *pressed.borrow(),
            [
                ("Spotify.exe".to_owned(), true),
                ("Spotify.exe".to_owned(), false)
            ]
        );
    }

    #[test]
    fn the_now_playing_card_follows_the_media_session_and_its_buttons_drive_it() {
        use crate::now_playing_card::fake::{FakeControls, track};
        let app = app_over(MockBackend::new());
        let fake = FakeControls::default();
        app.set_media_controls(fake.controls());
        app.main.show().expect("the main window shows");
        app.poll();
        assert_eq!(fake.following(), Some(CardSource::AnyApp));
        assert!(!app.main.get_now_playing_shown(), "no session yet");

        fake.read(Some(track("Spotify.exe", "Song", "", true)));
        app.poll();
        assert!(app.main.get_now_playing_shown());
        assert_eq!(app.main.get_now_playing_title(), "Song");
        assert_eq!(app.main.get_now_playing_subtitle(), "Spotify");
        assert!(app.main.get_now_playing_playing());
        assert!(!app.main.get_now_playing_has_artwork());

        app.main.invoke_now_playing_transport(0);
        app.main.invoke_now_playing_transport(1);
        app.main.invoke_now_playing_transport(2);
        assert_eq!(
            fake.sent(),
            [Transport::Previous, Transport::PlayPause, Transport::Next]
        );

        fake.read(Some(track("Spotify.exe", "Song", "Band", false)));
        app.poll();
        assert!(!app.main.get_now_playing_playing());
        assert_eq!(app.main.get_now_playing_subtitle(), "Band");

        // The focus on a button goes to the source list as the card hides.
        app.main.set_now_playing_focused_button(1);
        fake.read(None);
        app.poll();
        assert!(!app.main.get_now_playing_shown());
        assert_eq!(app.main.get_now_playing_focused_button(), -1);

        app.main.hide().expect("the main window hides");
        app.poll();
        assert_eq!(fake.following(), None, "nothing is read while hidden");
    }

    #[test]
    fn view_appearance_turns_every_window_light_or_dark_and_is_kept() {
        use slint::language::ColorScheme;
        let app = app_over(MockBackend::new());
        let dark = |app: &App| {
            [
                app.main.global::<Tokens>().get_dark(),
                app.flyout.global::<Tokens>().get_dark(),
                app.equalizer.global::<Tokens>().get_dark(),
                app.help.global::<Tokens>().get_dark(),
            ]
        };
        assert_eq!(app.main.get_appearance(), crate::Appearance::System);

        app.main.invoke_appearance_chosen(crate::Appearance::Dark);
        assert_eq!(app.main.get_appearance(), crate::Appearance::Dark);
        assert_eq!(dark(&app), [true; 4]);
        assert_eq!(
            app.backend.borrow().view_settings().appearance,
            ui_model::Appearance::Dark
        );

        app.main.invoke_appearance_chosen(crate::Appearance::Light);
        assert_eq!(dark(&app), [false; 4]);
        assert_eq!(
            app.pin_dialog.global::<Palette>().get_color_scheme(),
            ColorScheme::Light
        );

        // Headless, Slint knows no Windows theme, so Match System is light.
        app.main.invoke_appearance_chosen(crate::Appearance::System);
        assert_eq!(app.main.get_appearance(), crate::Appearance::System);
        assert_eq!(dark(&app), [false; 4]);
        assert_eq!(
            app.backend.borrow().view_settings().appearance,
            ui_model::Appearance::System
        );
    }

    #[test]
    fn a_saved_appearance_shows_at_launch_and_the_preview_is_not_saved() {
        let mut backend = MockBackend::new();
        backend.set_view_settings(ViewSettings {
            appearance: ui_model::Appearance::Dark,
            ..ViewSettings::default()
        });
        let app = app_over(backend);
        assert_eq!(app.main.get_appearance(), crate::Appearance::Dark);
        assert!(app.about.global::<Tokens>().get_dark());

        app.apply_appearance(ui_model::Appearance::Light);
        app.main.invoke_show_virtual_devices_toggled(true);
        assert_eq!(
            app.backend.borrow().view_settings().appearance,
            ui_model::Appearance::Dark,
            "the preview's appearance isn't saved with the next change"
        );
    }

    // covers: S212
    #[test]
    fn a_late_read_keeps_the_window_frame_and_logs_no_unchanged_airplay2_choice() {
        use crate::debug_log;
        let path = debug_log::test_dir("view-late-read").join(debug_log::FILE_NAME);
        let backend = MockBackend::new();
        let late_view = backend.late_view.clone();
        let log = DebugLog::new(Some(path.clone()));
        let app = app_logging(backend, log.clone());
        let size = app.main.window().size();
        *late_view.borrow_mut() = Some(ViewSettings {
            window: Some(ui_model::WindowFrame {
                x: 40,
                y: 40,
                width: 701.0,
                height: 503.0,
                maximized: true,
            }),
            ..ViewSettings::default()
        });
        app.poll();
        assert_eq!(app.main.window().size(), size, "the frame stays");
        assert!(log.flush(debug_log::TEST_FLUSH));
        let written = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!written.contains("applies at the next launch"), "{written}");
    }

    // covers: S212
    #[test]
    fn window_settings_read_after_launch_show_and_survive_a_quit() {
        let backend = MockBackend::new();
        let late_view = backend.late_view.clone();
        let app = app_over(backend);
        let file = ViewSettings {
            appearance: ui_model::Appearance::Dark,
            show_virtual_devices: true,
            airplay2_experimental: true,
            ..ViewSettings::default()
        };
        *late_view.borrow_mut() = Some(file.clone());
        app.poll();
        assert_eq!(app.main.get_appearance(), crate::Appearance::Dark);
        assert!(
            app.ui.borrow().airplay2_notice.is_some(),
            "the sender keeps its path until the next launch"
        );
        app.save_settings();
        assert_eq!(app.backend.borrow().view_settings(), file);
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

    // covers: P215
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
        let mut gone = retrying.clone();
        gone.id = "airplay:gone".into();
        gone.waiting = Some(ui_model::Waiting::NotOnNetwork);
        backend.add_speaker(busy);
        backend.add_speaker(retrying);
        backend.add_speaker(gone);
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
        assert_eq!(row("airplay:gone").state, RowState::NotOnNetwork);

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
        let slot = "Waiting: too many AirPlay 2 speakers".to_owned();
        assert_eq!(
            read(RowState::WaitingForAirplay2),
            (slot.clone(), turn_off.clone(), slot)
        );
        let ptp = "Waiting: AirPlay 2 speakers on another network".to_owned();
        assert_eq!(
            read(RowState::WaitingForPtp),
            (ptp.clone(), turn_off.clone(), ptp)
        );
        // The Mac row's words (P215).
        let away = "Not on the network right now, plays once it's back".to_owned();
        assert_eq!(
            read(RowState::NotOnNetwork),
            (away.clone(), turn_off.clone(), away)
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

    // covers: S220
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
            .invoke_delay_edited("local:desk".into(), "+".into());
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
            .invoke_delay_edited("local:desk".into(), "+".into());
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

    #[test]
    fn only_a_local_output_with_an_offset_offers_calibration_and_says_what_it_needs() {
        let app = app_over(MockBackend::new());
        assert!(row_of(&app, "local:desk").calibratable);
        assert!(
            !row_of(&app, "local:speakers").calibratable,
            "the reference"
        );
        let desk = row_of(&app, "local:desk");
        assert!(!desk.calibration_enabled, "streaming is off");
        assert!(
            desk.calibration_status.starts_with("Turn on streaming"),
            "{}",
            desk.calibration_status
        );

        let mut backend = MockBackend::new();
        backend.set_running(true);
        backend.set_speaker_enabled("local:desk", true);
        let app = app_over(backend);
        let desk = row_of(&app, "local:desk");
        assert!(desk.calibration_enabled);
        assert_eq!(desk.calibration_status, "", "nothing stands in the way");

        let mut state = app.snapshot();
        state.calibration.speaker_id = Some("local:desk".into());
        state.calibration.phase = Phase::Failed(Failure::MicrophoneDenied);
        let speaker = state.speaker("local:desk").unwrap().clone();
        let line = calibration_status(&app.strings(), &state, &speaker);
        assert!(
            line.to_lowercase().contains("privacy"),
            "it says where to allow it: {line}"
        );
        state.calibration.phase = Phase::Done {
            offset_ms: 412.4,
            clamped: false,
        };
        let line = calibration_status(&app.strings(), &state, &speaker);
        assert!(line.contains("412"), "{line}");
    }

    #[test]
    fn a_calibration_that_cannot_start_names_only_what_is_off() {
        fn switch(state: &mut AppState, streaming: bool, desk: bool, reference: bool) {
            state.running = streaming;
            for speaker in &mut state.speakers {
                match speaker.id.as_str() {
                    "local:desk" => speaker.enabled = desk,
                    "local:speakers" => speaker.enabled = reference,
                    _ => {}
                }
            }
        }
        fn engine(session: Option<u64>, playing: &[&str]) -> calibration::Engine {
            calibration::Engine {
                session,
                playing: playing.iter().map(|id| id.to_string()).collect(),
                reference: Some("builtin".into()),
                ..Default::default()
            }
        }
        fn lineup(session: Option<u64>, ids: &[&str]) -> crate::audio::calibrate_all::Lineup {
            crate::audio::calibrate_all::Lineup {
                session,
                reference: Some("builtin".into()),
                members: ids
                    .iter()
                    .map(|id| crate::audio::calibrate_all::Member {
                        id: id.to_string(),
                        class: audiosync_core::calibrate_all::Class::Local,
                        offset_ms: Some(0.0),
                        latency_ms: 2500.0,
                        sender_ms: 0.0,
                    })
                    .collect(),
                ..Default::default()
            }
        }
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let mut state = app.snapshot();
        state.reference_name = Some("Speakers".into());
        let row = |state: &AppState| {
            let desk = state.speaker("local:desk").unwrap().clone();
            calibration_status(&strings, state, &desk).to_string()
        };
        let mut rows = Vec::new();
        for (streaming, desk, reference) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
            (false, false, true),
            (false, true, false),
            (false, false, false),
        ] {
            switch(&mut state, streaming, desk, reference);
            rows.push(row(&state));
        }
        assert_eq!(
            rows,
            [
                "Turn on streaming to calibrate.",
                "Turn on this speaker to calibrate it.",
                // The button stays enabled while the reference is off; the press names it.
                "",
                "Turn on streaming and this speaker to calibrate it.",
                "Turn on streaming to calibrate.",
                "Turn on streaming and this speaker to calibrate it.",
            ],
            "the line under the button"
        );

        let pressed: Vec<String> = [
            engine(None, &["builtin", "usb"]),
            engine(Some(1), &["builtin"]),
            engine(Some(1), &["usb"]),
            engine(Some(1), &[]),
        ]
        .iter()
        .map(|engine| {
            let failure = calibration::blocker(engine, "usb").expect("it can't start");
            calibration_failure(&strings, "Speakers", failure).to_string()
        })
        .collect();
        assert_eq!(
            pressed,
            [
                "Turn on streaming to calibrate.",
                "Turn on this speaker to calibrate it.",
                "Turn on Speakers to calibrate.",
                "Turn on this speaker and Speakers to calibrate it.",
            ],
            "the press"
        );

        let none = std::collections::BTreeSet::new();
        let all: Vec<String> = [lineup(None, &["builtin", "usb"]), lineup(Some(1), &["usb"])]
            .iter()
            .map(|lineup| {
                let mut state = app.snapshot();
                state.reference_name = Some("Speakers".into());
                state.calibrate_all.phase = AllPhase::Idle;
                state.calibrate_all.other_running = false;
                state.calibrate_all.blocked = crate::audio::calibrate_all::blocked(lineup, &none);
                calibrate_all_card(&strings, &state).status.to_string()
            })
            .collect();
        assert_eq!(
            all,
            [
                "Turn on streaming to calibrate.",
                "Turn on Speakers to calibrate."
            ],
            "Calibrate All"
        );

        let mut both = lineup(None, &[]);
        both.reference_off = true;
        assert_eq!(
            crate::audio::calibrate_all::blocked(&both, &none)
                .map(|failure| { calibration_failure(&strings, "", failure).to_string() }),
            Some("Turn on streaming and the PC's own speakers to calibrate.".to_owned()),
            "Calibrate All, without a reference name"
        );

        let again: Vec<String> = [
            Missing::STREAMING,
            Missing::SPEAKER,
            Missing::REFERENCE,
            Missing {
                reference: true,
                ..Missing::STREAMING
            },
            Missing {
                speaker: true,
                ..Missing::STREAMING
            },
            Missing::ALL,
        ]
        .into_iter()
        .map(|missing| {
            let mut state = app.snapshot();
            state.reference_name = Some("Speakers".into());
            state.calibrate_all.other_running = false;
            state.calibrate_all.phase = AllPhase::CannotMeasureAgain("Den".into(), missing);
            calibrate_all_card(&strings, &state).status.to_string()
        })
        .collect();
        assert_eq!(
            again,
            [
                "Turn on streaming to measure it again.",
                "Turn on Den to measure it again.",
                "Turn on Speakers to measure it again.",
                "Turn on streaming and Speakers to measure it again.",
                "Turn on streaming and Den to measure it again.",
                "Turn on streaming, Den and Speakers to measure it again.",
            ],
            "Measure Again"
        );
    }

    #[test]
    fn outputs_left_out_by_kind_show_why_on_the_card_and_in_their_delay_controls() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let mut state = app.snapshot();
        state.calibrate_all.blocked = Some(Failure::OnlyLeftOut);
        state.calibrate_all.left_out = vec![
            LeftOutRow {
                id: "phones".into(),
                name: "Headphones".into(),
                kind: ui_model::MicKind::Headphones,
                included: false,
            },
            LeftOutRow {
                id: "cable".into(),
                name: "Cable Input".into(),
                kind: ui_model::MicKind::Virtual,
                included: true,
            },
        ];
        let card = calibrate_all_card(&strings, &state);
        assert!(!card.can_start);
        assert!(card.status.contains("Include Anyway"), "{}", card.status);
        assert!(
            !card.status.contains("Only"),
            "not the only-the-reference text"
        );
        assert_eq!(card.left_out.len(), 2);
        assert_eq!(
            card.left_out[0].reason,
            "Left out of microphone calibration: the microphone can't hear headphones."
        );
        assert!(!card.left_out[0].included);
        assert_eq!(
            card.left_out[1].reason,
            "Included anyway: measured in this run."
        );
        assert!(card.left_out[0].help.contains("as headphones"));
        assert!(card.left_out[1].help.starts_with("For a virtual output"));

        let mut speaker = state.speaker("local:desk").unwrap().clone();
        speaker.enabled = true;
        assert!(is_calibratable(&speaker));
        speaker.mic_left_out = Some(ui_model::MicKind::Virtual);
        assert!(!is_calibratable(&speaker), "no Calibrate with Microphone");
        assert_eq!(calibration_status(&strings, &state, &speaker), "");
        assert_eq!(
            calibration_left_out(&strings, ui_model::MicKind::Virtual),
            "Left out of microphone calibration: a virtual output has nothing for the \
             microphone to hear."
        );
    }

    #[test]
    fn the_calibrate_all_card_names_each_step_and_says_how_the_airplay_speakers_differ() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let mut state = app.snapshot();
        state.calibrate_all.phase = AllPhase::Running(AllProgress::Measuring {
            name: "Den".into(),
            reference: false,
            take: 2,
            searching: false,
        });
        let card = calibrate_all_card(&strings, &state);
        assert!(card.running);
        assert!(card.status.contains("Den") && card.status.contains('2'));
        state.calibrate_all.phase = AllPhase::Running(AllProgress::Measuring {
            name: "Den".into(),
            reference: false,
            take: 1,
            searching: true,
        });
        let card = calibrate_all_card(&strings, &state);
        assert!(card.status.contains("up to a minute"), "{}", card.status);

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
        state.calibrate_all.phase = AllPhase::Idle;
        state.calibrate_all.sheet = Some(SheetState {
            rows: vec![
                row("Speakers", Class::Local, SheetRowState::Reference),
                row("Kitchen", Class::AirPlay, SheetRowState::Certain(clear)),
                row(
                    "Bedroom",
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
                certain_rows: 2,
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
        });
        let card = calibrate_all_card(&strings, &state);
        assert!(card.results_shown && card.can_apply);
        assert!(
            card.rows[1].detail.contains("2040"),
            "{}",
            card.rows[1].detail
        );
        assert!(card.rows[2].detail.contains("Measure Again"));
        assert!(card.airplay.contains("23 ms"), "{}", card.airplay);
        assert!(card.airplay.contains("differ by"), "over 10 ms apart");
        let never = SheetRow {
            old_ms: None,
            ..row("Den", Class::Cast, SheetRowState::Certain(clear))
        };
        let detail = row_detail(&strings, "", &never);
        assert!(detail.starts_with("Was none."), "{detail}");
        let sheet = state.calibrate_all.sheet.as_mut().unwrap();
        let shared = sheet.airplay.as_mut().unwrap();
        shared.spread_ms = 4.0;
        shared.state = SharedState::Certain;
        let card = calibrate_all_card(&strings, &state);
        assert!(card.airplay.contains("4 ms apart"), "{}", card.airplay);
        assert!(!card.airplay.contains("differ by"));
        assert_eq!(card.said, strings.invoke_calibrate_all_finished(1, 2));

        let sheet = state.calibrate_all.sheet.as_mut().unwrap();
        sheet.ended = Some(EndReason::SpeakerTurnedOff);
        sheet.can_apply = false;
        sheet.failure = None;
        let card = calibrate_all_card(&strings, &state);
        assert!(!card.can_apply);
        assert_eq!(
            card.said,
            strings.invoke_calibrate_all_stopped(strings.get_calibrate_all_ended_off())
        );

        state.calibrate_all.sheet = None;
        state.calibrate_all.phase = AllPhase::Failed(Failure::MicrophoneDenied);
        let card = calibrate_all_card(&strings, &state);
        assert!(card.privacy && !card.results_shown);
        assert!(
            card.said.to_lowercase().contains("privacy"),
            "{}",
            card.said
        );
    }

    #[test]
    fn the_calibrate_all_card_says_what_holds_it_and_when_nothing_was_clear() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let mut state = app.snapshot();
        state.calibrate_all.phase = AllPhase::Idle;
        state.calibrate_all.other_running = true;
        let card = calibrate_all_card(&strings, &state);
        assert_eq!(card.status, strings.get_calibrate_all_busy());

        state.calibrate_all.other_running = false;
        state.calibrate_all.phase = AllPhase::CannotMeasureAgain("Den".into(), Missing::SPEAKER);
        let card = calibrate_all_card(&strings, &state);
        assert!(card.status.contains("Den"), "{}", card.status);
        assert!(
            card.status.ends_with("to measure it again."),
            "{}",
            card.status
        );
        assert_eq!(card.said, card.status);

        let unclear = Measurement {
            new_ms: 2040.0,
            uncertainty_ms: 14.0,
            difference_ms: 40.0,
            clamped: false,
        };
        let row = |name: &str, state| SheetRow {
            name: name.into(),
            class: Class::Cast,
            old_ms: None,
            state,
            can_measure_again: true,
        };
        state.calibrate_all.phase = AllPhase::Idle;
        state.calibrate_all.sheet = Some(SheetState {
            rows: vec![
                row("Speakers", SheetRowState::Reference),
                row("Den", SheetRowState::Uncertain(unclear)),
            ],
            airplay: None,
            ended: None,
            failure: None,
            again_ended: None,
            can_apply: false,
            applied: false,
            can_undo: false,
            undone: false,
        });
        let card = calibrate_all_card(&strings, &state);
        assert_eq!(card.outcome, strings.get_calibrate_all_nothing_certain());
    }

    #[test]
    fn an_airplay2_row_waits_for_the_next_launch_once_the_switch_is_on() {
        assert_eq!(
            with_switch(Availability::Airplay2NotYet, false),
            Availability::Airplay2NotYet
        );
        assert_eq!(
            with_switch(Availability::Airplay2NotYet, true),
            Availability::Airplay2NextLaunch
        );
        assert_eq!(with_switch(Availability::Open, true), Availability::Open);
    }

    #[test]
    fn a_calibrations_ending_is_announced_once() {
        let mut announced = None;
        let mut calibration = ui_model::CalibrationState {
            speaker_id: Some("local:desk".into()),
            phase: Phase::Running(Step::Preparing),
            busy: true,
        };
        assert_eq!(calibration_ended(&mut announced, &calibration), None);
        calibration.phase = Phase::Failed(Failure::MicrophoneSkipped);
        assert_eq!(
            calibration_ended(&mut announced, &calibration).as_deref(),
            Some("local:desk")
        );
        calibration.busy = false;
        assert_eq!(
            calibration_ended(&mut announced, &calibration),
            None,
            "said"
        );
        calibration.phase = Phase::Idle;
        assert_eq!(calibration_ended(&mut announced, &calibration), None);
        calibration.phase = Phase::Failed(Failure::MicrophoneSkipped);
        assert_eq!(
            calibration_ended(&mut announced, &calibration).as_deref(),
            Some("local:desk"),
            "a second run that ends the same way"
        );
    }

    #[test]
    fn a_blocked_microphone_offers_its_privacy_settings_on_the_row() {
        let mut backend = MockBackend::new();
        backend.set_running(true);
        backend.set_speaker_enabled("local:desk", true);
        let app = app_over(backend);
        let mut state = app.snapshot();
        let speaker = state.speaker("local:desk").unwrap().clone();
        assert!(!speaker_item(&state, &speaker, false, true, None).calibration_privacy);
        state.calibration.speaker_id = Some("local:desk".into());
        state.calibration.phase = Phase::Failed(Failure::MicrophoneDenied);
        assert!(speaker_item(&state, &speaker, false, true, None).calibration_privacy);
        let bt = state.speaker("local:bt").unwrap().clone();
        assert!(!speaker_item(&state, &bt, false, true, None).calibration_privacy);
    }

    #[test]
    fn open_privacy_settings_shows_only_where_something_opens_that_page() {
        let privacy = SettingsPage::MicrophonePrivacy.uri();
        assert!(offers_privacy(true, |uri| uri == privacy));
        // Linux has no program for the page (guide::linux_program): no button.
        assert!(!offers_privacy(true, |uri| uri != privacy));
        assert!(!offers_privacy(false, |_| true));
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
    fn the_delay_button_applies_every_typed_delay_and_closes_only_its_controls() {
        let app = app_over(MockBackend::new());
        type_into_two_rows(&app, "-200");

        app.main.invoke_sync_toggled("local:desk".into());
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(400.0),
            "closing applied it"
        );
        assert_eq!(desk_delay_text(&app), "+400");
        assert_eq!(
            delay_of(&app, "local:bt"),
            Some(-200.0),
            "the other open row's typing applied too"
        );
        let ui = app.ui.borrow();
        assert!(!ui.sync_open.contains("local:desk"));
        assert!(ui.sync_open.contains("local:bt"), "the headset's stay open");
        assert!(ui.delay_drafts.is_empty());
    }

    #[test]
    fn enter_applies_its_rows_typed_delay_and_keeps_every_control_open() {
        let app = app_over(MockBackend::new());
        type_into_two_rows(&app, "-200");
        app.main.invoke_delay_committed("local:desk".into());
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(400.0),
            "Enter applied it"
        );
        let ui = app.ui.borrow();
        assert!(
            ui.sync_open.contains("local:desk"),
            "its controls stay open"
        );
        assert!(ui.sync_open.contains("local:bt"), "and the headset's");
        assert!(ui.delay_drafts.contains_key("local:bt"), "with its draft");
        drop(ui);
        assert_eq!(delay_of(&app, "local:bt"), Some(-250.0));
    }

    #[test]
    fn a_click_on_a_rows_box_toggles_its_controls_and_applies_the_typing_first() {
        let app = app_over(MockBackend::new());
        draw_frames();
        let window = app.main.window();
        app.main.invoke_sync_toggled("local:bt".into());
        app.main
            .invoke_delay_edited("local:bt".into(), "-200".into());

        // The blank part of the desk's row, right of its name.
        click(window, 600.0, 516.0);
        assert_eq!(
            delay_of(&app, "local:bt"),
            Some(-200.0),
            "the headset's typing applied first"
        );
        let open = |app: &App| app.ui.borrow().sync_open.clone();
        assert!(open(&app).contains("local:desk"), "the desk's opened");
        assert!(open(&app).contains("local:bt"), "the headset's stay open");

        app.main
            .invoke_delay_edited("local:desk".into(), "400".into());
        app.main.invoke_airplay_delay_edited("2100".into());
        click(window, 600.0, 516.0);
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(400.0),
            "closing applied it"
        );
        assert_eq!(app.snapshot().airplay_delay_ms, Some(2100.0));
        assert!(!open(&app).contains("local:desk"), "and closed it");

        // The reference row has no delay: a click there is outside.
        click(window, 600.0, 516.0);
        app.main
            .invoke_delay_edited("local:desk".into(), "300".into());
        click(window, 600.0, 190.0);
        assert_eq!(delay_of(&app, "local:desk"), Some(300.0));
        assert!(open(&app).is_empty());
    }

    fn ctrl_s(window: &slint::Window) {
        window.dispatch_event(WindowEvent::KeyPressed {
            text: Key::Control.into(),
        });
        press(window, "s");
        window.dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
    }

    #[test]
    fn ctrl_s_applies_every_typed_delay_wherever_the_focus_is_and_keeps_the_controls_open() {
        let app = app_over(MockBackend::new());
        app.main.show().unwrap();
        draw_frames();
        type_into_two_rows(&app, "-200");
        app.main.invoke_airplay_delay_edited("2100".into());
        app.main.invoke_focus_streaming_switch();
        draw_frames();
        ctrl_s(app.main.window());
        assert_eq!(delay_of(&app, "local:desk"), Some(400.0));
        assert_eq!(delay_of(&app, "local:bt"), Some(-200.0));
        assert_eq!(app.snapshot().airplay_delay_ms, Some(2100.0));
        let ui = app.ui.borrow();
        assert_eq!(ui.sync_open.len(), 2, "the sync controls stay open");
        assert!(ui.delay_drafts.is_empty() && ui.airplay_delay_draft.draft.is_none());
    }

    #[test]
    fn the_range_line_is_announced_once_as_a_field_goes_out_of_range() {
        let mut fields = HashSet::new();
        let hint = |text: &str| SharedString::from(text);
        let desk = |text: &str| vec![("local:desk".to_owned(), hint(text))];
        assert_eq!(newly_out_of_range(&mut fields, desk("A")), Some(hint("A")));
        assert_eq!(
            newly_out_of_range(&mut fields, desk("B")),
            None,
            "still out"
        );
        assert_eq!(newly_out_of_range(&mut fields, Vec::new()), None);
        assert_eq!(
            newly_out_of_range(&mut fields, desk("C")),
            Some(hint("C")),
            "out again"
        );
        let both = vec![
            ("local:desk".to_owned(), hint("C")),
            (AIRPLAY_DELAY_FIELD.to_owned(), hint("D")),
        ];
        assert_eq!(newly_out_of_range(&mut fields, both), Some(hint("D")));

        let app = app_over(MockBackend::new());
        app.main.invoke_sync_toggled("local:desk".into());
        app.main
            .invoke_delay_edited("local:desk".into(), "+30000".into());
        let said = app.main.get_notice_announcement();
        assert_eq!(
            said,
            "The range is \u{2212}2000 to +20000 ms. Enter applies +20000."
        );
    }

    /// No AirPlay speaker found, and Connected Speakers lists the desk
    /// (`with_desk`) or nothing.
    fn app_with_the_no_airplay_note(with_desk: bool) -> Rc<App> {
        let mut backend = MockBackend::new();
        backend.set_airplay_none_found(true);
        backend.retain_speakers(|s| {
            !s.id.starts_with("airplay:")
                && s.id != "local:bt"
                && (with_desk || s.id != "local:desk")
        });
        let app = app_over(backend);
        draw_frames();
        app
    }

    #[test]
    fn a_click_on_the_no_airplay_note_under_a_list_works_like_one_on_the_last_row() {
        let app = app_with_the_no_airplay_note(true);
        click(app.main.window(), 600.0, 525.0);
        assert!(
            app.ui.borrow().sync_open.contains("local:desk"),
            "the desk's controls opened"
        );
        app.main
            .invoke_delay_edited("local:desk".into(), "400".into());
        // The open controls push the note below the list's bottom edge;
        // scrolled to the end, it sits at y 522 to 540.
        scroll(app.main.window(), 640.0, 600.0, -300.0);
        click(app.main.window(), 420.0, 530.0);
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(400.0),
            "closing applied it"
        );
        assert!(app.ui.borrow().sync_open.is_empty());
    }

    /// While AirPlay discovery has listed nothing yet, the main window's
    /// empty Connected Speakers list says it looks, as the flyout does.
    #[test]
    fn the_main_window_and_the_flyout_both_look_while_airplay_searches() {
        let mut backend = MockBackend::new();
        backend.set_airplay_searching(true);
        let app = app_over(backend);
        assert!(app.main.get_airplay_searching());
        assert!(app.flyout.get_airplay_searching());

        let app = app_over(MockBackend::new());
        assert!(!app.main.get_airplay_searching());
    }

    #[test]
    fn streaming_with_every_speaker_off_says_the_first_one_on_plays() {
        let mut backend = MockBackend::new();
        backend.set_no_speaker_on(true);
        let app = app_over(backend);
        assert!(app.main.get_no_speaker_on());
        assert!(app.flyout.get_no_speaker_on());

        let app = app_over(MockBackend::new());
        assert!(!app.main.get_no_speaker_on());
        assert!(!app.flyout.get_no_speaker_on());
    }

    #[test]
    fn a_click_on_the_no_airplay_note_alone_is_outside_every_row() {
        let app = app_with_the_no_airplay_note(false);
        app.main.invoke_airplay_delay_edited("2100".into());
        click(app.main.window(), 420.0, 465.0);
        assert_eq!(app.snapshot().airplay_delay_ms, Some(2100.0), "it applied");
        assert!(app.ui.borrow().sync_open.is_empty(), "and opened nothing");
    }

    #[test]
    fn a_click_on_the_blank_space_of_open_sync_controls_closes_them_and_applies_the_typing() {
        let app = app_with_the_no_airplay_note(true);
        // The open controls fill the desk's row from y 600 to 680: right of
        // the steps, then right of the line under them.
        for (x, y, ms) in [(900.0, 619.0, 400.0), (600.0, 649.0, 300.0)] {
            app.main.invoke_sync_toggled("local:desk".into());
            draw_frames();
            app.main
                .invoke_delay_edited("local:desk".into(), format!("{ms}").into());
            click(app.main.window(), x, y);
            assert!(
                app.ui.borrow().sync_open.is_empty(),
                "the controls closed at {x}, {y}"
            );
            assert_eq!(
                delay_of(&app, "local:desk"),
                Some(ms),
                "closing applied the typing"
            );
        }
    }

    #[test]
    fn apply_typed_delay_is_enabled_only_while_a_delay_is_typed() {
        let app = app_over(MockBackend::new());
        assert!(!app.main.get_delay_typing(), "nothing is typed");
        app.main.invoke_sync_toggled("local:desk".into());
        assert!(!app.main.get_delay_typing(), "open controls alone");
        app.main
            .invoke_delay_edited("local:desk".into(), "400".into());
        assert!(app.main.get_delay_typing());
        app.main.invoke_delay_apply_requested();
        assert!(!app.main.get_delay_typing(), "applied");
        app.main.invoke_airplay_delay_edited("2100".into());
        assert!(app.main.get_delay_typing(), "the AirPlay field counts too");
        app.main.invoke_airplay_delay_cancelled();
        assert!(!app.main.get_delay_typing(), "dropped");
    }

    #[test]
    fn closing_the_window_without_a_tray_quits_and_drops_every_typed_delay() {
        let app = app_over(MockBackend::new());
        app.set_tray_available(false);
        app.main.invoke_sync_toggled("local:desk".into());
        app.main
            .invoke_delay_edited("local:desk".into(), "400".into());
        app.main.invoke_airplay_delay_edited("2100".into());

        app.close_main();
        let ui = app.ui.borrow();
        assert!(ui.delay_drafts.is_empty() && ui.airplay_delay_draft.draft.is_none());
        assert!(ui.sync_open.is_empty());
        let state = app.snapshot();
        assert_eq!(state.speaker("local:desk").unwrap().delay_ms, Some(120.0));
        assert_eq!(state.airplay_delay_ms, Some(1750.0));
    }

    #[test]
    fn closing_the_window_with_a_tray_applies_every_typed_delay() {
        let app = app_over(MockBackend::new());
        app.set_tray_available(true);
        app.main.invoke_sync_toggled("local:desk".into());
        app.main
            .invoke_delay_edited("local:desk".into(), "400".into());
        app.main.invoke_airplay_delay_edited("2100".into());

        app.close_main();
        assert!(app.ui.borrow().sync_open.is_empty());
        let state = app.snapshot();
        assert_eq!(state.speaker("local:desk").unwrap().delay_ms, Some(400.0));
        assert_eq!(state.airplay_delay_ms, Some(2100.0));
    }

    #[test]
    fn a_range_line_said_with_a_new_notice_keeps_the_notice() {
        let mut backend = MockBackend::new();
        backend.post("source", NoticeMessage::CaptureLost, "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        let app = app_over(backend);
        let strings = app.strings();
        let notice =
            strings.invoke_notice_announcement(notice_title(&strings, &NoticeMessage::NoSpeaker));
        app.main.invoke_sync_toggled("local:desk".into());
        // As if the second notice came with the keystroke.
        *app.notice_announcer.borrow_mut() = NoticeAnnouncer {
            seen: Some(1),
            ..NoticeAnnouncer::default()
        };
        app.main
            .invoke_delay_edited("local:desk".into(), "+30000".into());
        let range = "The range is \u{2212}2000 to +20000 ms. Enter applies +20000.";
        assert_eq!(
            app.main.get_notice_announcement(),
            format!("{notice}. {range}")
        );
    }

    #[test]
    fn a_delay_field_takes_only_digits_and_marks_a_number_out_of_range() {
        let app = app_over(MockBackend::new());
        app.main.invoke_sync_toggled("local:desk".into());
        app.main
            .invoke_delay_edited("local:desk".into(), "1".into());
        app.main
            .invoke_delay_edited("local:desk".into(), "1a".into());
        assert_eq!(desk_delay_text(&app), "1", "the letter is refused");
        app.main
            .invoke_delay_edited("local:desk".into(), "12".into());
        assert_eq!(desk_delay_text(&app), "12", "the letter was dropped");
        assert_eq!(desk_row(&app).delay_range_hint, "");

        app.main
            .invoke_delay_edited("local:desk".into(), "+120910".into());
        assert_eq!(
            desk_row(&app).delay_range_hint,
            "The range is \u{2212}2000 to +20000 ms. Enter applies +20000."
        );
        app.main.invoke_delay_committed("local:desk".into());
        assert_eq!(delay_of(&app, "local:desk"), Some(20000.0));
        assert_eq!(desk_row(&app).delay_range_hint, "", "nothing is typed now");

        app.main.invoke_airplay_delay_edited("-5".into());
        assert_eq!(
            app.main.get_airplay_delay_range_hint(),
            "The range is 0 to 20000 ms. Enter applies 0."
        );
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
        press(window, "x");
        assert_eq!(desk_delay_text(&app), "+1209", "a letter can't be typed");
        press(window, Key::Return);
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(1209.0),
            "Enter applied it"
        );
        assert!(
            app.ui.borrow().sync_open.contains("local:desk"),
            "and kept it open"
        );
        assert_eq!(
            focus(),
            Some(("local:desk".into(), RowPart::Sync)),
            "the field keeps it"
        );

        press(window, "1");
        ctrl_s(window);
        assert_eq!(
            delay_of(&app, "local:desk"),
            Some(12091.0),
            "Ctrl+S applied it"
        );
        assert!(app.ui.borrow().sync_open.contains("local:desk"));
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

    // covers: S315
    #[cfg(windows)]
    #[test]
    fn the_end_of_a_windows_session_gives_a_removed_exe_its_bak_back_and_keeps_the_record() {
        let backend = MockBackend::new();
        let saved_views = backend.saved_views.clone();
        let app = app_over(backend);
        let (updater, exe) = crate::updater::tests::swapped_exe_gone("session-end");
        *app.updater.borrow_mut() = Some(updater);
        app.at_session_end();
        assert!(exe.exists(), "the .bak is audiosync.exe again");
        let saved: Vec<_> = saved_views
            .borrow()
            .iter()
            .map(|view| view.update_record.clone())
            .collect();
        assert_eq!(
            saved,
            [Some(ui_model::UpdateRecord {
                version: "0.9.0".to_owned(),
                reason: ui_model::UpdateRecordReason::Blocked,
            })],
            "the one save already holds the record"
        );
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
    fn quitting_applies_an_open_rename() {
        let app = app_over(MockBackend::new());
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "Desk left".into());
        app.save_settings();
        assert_eq!(name_of(&app, "local:desk"), "Desk left");
        assert!(app.ui.borrow().rename.is_none());
    }

    #[test]
    fn quitting_applies_an_open_zone_rename_to_the_zone() {
        let app = app_over(MockBackend::zones_preview());
        app.main.invoke_zone_rename_requested("zone:office".into());
        app.main
            .invoke_zone_rename_edited("zone:office".into(), "Study".into());
        app.save_settings();
        let state = app.snapshot();
        let office = state.zones.iter().find(|z| z.id == "zone:office");
        assert_eq!(office.unwrap().name, "Study");
        assert!(app.ui.borrow().rename.is_none());
    }

    fn name_of(app: &App, id: &str) -> String {
        app.snapshot().speaker(id).unwrap().name.clone()
    }

    #[test]
    fn the_waiting_notices_name_the_sender_limit_and_what_to_do() {
        let app = app_over(MockBackend::new());
        let strings = app.main.global::<Strings>();
        assert_eq!(
            strings.invoke_notice_airplay(AirPlayProblem::TooManyAirplay2Speakers, "Loft".into()),
            format!(
                "AudioSync plays up to {} AirPlay 2 speakers at once, so Loft waits. \
                 It starts once one of them stops.",
                audiosync_airplay::sender::AIRPLAY2_SESSIONS
            )
        );
        // The number comes from the sender, not from the strings file.
        strings.set_airplay2_sessions(2);
        assert_eq!(
            strings.invoke_notice_airplay(AirPlayProblem::TooManyAirplay2Speakers, "Loft".into()),
            "AudioSync plays up to 2 AirPlay 2 speakers at once, so Loft waits. \
             It starts once one of them stops."
        );
        assert_eq!(
            strings.invoke_notice_airplay(AirPlayProblem::PtpOtherNetwork, "Loft".into()),
            "Loft waits while AudioSync plays to AirPlay 2 speakers on another network. \
             Turn those speakers off and Loft starts."
        );
    }

    #[test]
    fn the_code_notes_carry_the_macs_tooltips() {
        let app = app_over(MockBackend::new());
        let strings = app.main.global::<Strings>();
        let help = |a, every_time, state| {
            strings
                .invoke_speaker_availability_help(a, every_time, state)
                .to_string()
        };
        let every_time = "This speaker asks for a code each time it connects, so \
                          auto-connect leaves it off. AudioSync asks you for the code \
                          when you turn the speaker on.";
        assert_eq!(
            help(Availability::NeedsCode, true, RowState::Off),
            every_time
        );
        assert_eq!(help(Availability::Open, true, RowState::Off), every_time);
        assert_eq!(
            help(Availability::NeedsCode, false, RowState::Off),
            "This speaker asks for a PIN or password. AudioSync asks you for it when you \
             turn the speaker on."
        );
        // No note, no tooltip.
        assert_eq!(help(Availability::Open, true, RowState::Playing), "");
        assert_eq!(help(Availability::Pairing, false, RowState::Off), "");
    }

    #[test]
    fn hide_speaker_takes_the_row_out_and_hidden_speakers_brings_it_back() {
        let app = app_over(MockBackend::new());
        let strings = app.main.global::<Strings>();
        let reference = app
            .snapshot()
            .speakers
            .iter()
            .find(|speaker| speaker.is_reference())
            .map(|speaker| speaker.id.clone())
            .expect("the mock has a reference");
        assert!(!row_of(&app, &reference).hideable, "no Hide Speaker");
        assert!(row_of(&app, "local:desk").hideable);
        assert_eq!(app.main.get_hidden_speakers().row_count(), 0);
        let name = name_of(&app, "local:desk");

        app.main.invoke_speaker_hide_requested("local:desk".into());
        assert!(app.snapshot().speaker("local:desk").is_none());
        assert!(
            !app.models
                .connected
                .iter()
                .any(|row| row.id == "local:desk"),
            "its row is gone"
        );
        assert_eq!(app.main.get_notice_announcement(), format!("{name} hidden"));
        let entries = app.main.get_hidden_speakers();
        assert_eq!(entries.row_count(), 1);
        let entry = entries.row_data(0).unwrap();
        assert_eq!(entry.id, "local:desk");
        assert!(entry.present);
        let usb = strings.invoke_speaker_kind(SpeakerKind::Usb);
        assert_eq!(entry.kind, usb);
        assert_eq!(
            strings.invoke_hidden_speaker_entry(entry.name.clone(), entry.kind.clone(), true),
            format!("{name} ({usb})")
        );
        assert_eq!(
            strings.invoke_hidden_speaker_entry(entry.name, entry.kind, false),
            format!("{name} ({usb}, not found)")
        );
        assert!(app.main.get_connected_all_hidden());
        assert!(!app.main.get_this_pc_all_hidden());

        app.main
            .invoke_speaker_hide_requested(reference.as_str().into());
        assert!(app.snapshot().speaker(&reference).is_some());

        app.main
            .invoke_speaker_unhide_requested("local:desk".into());
        assert!(!app.snapshot().speaker("local:desk").unwrap().enabled);
        assert!(row_of(&app, "local:desk").hideable);
        assert_eq!(app.main.get_notice_announcement(), format!("{name} shown"));
        assert_eq!(app.main.get_hidden_speakers().row_count(), 0);
        assert!(!app.main.get_connected_all_hidden());
    }

    #[test]
    fn a_rename_applies_on_enter_and_escape_keeps_the_old_name() {
        let app = app_over(MockBackend::new());
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        let row = row_of(&app, "local:desk");
        assert!(row.renaming && row.name_text == "Desk Speaker");
        assert_eq!(
            app.main.get_focus_speaker_part(),
            RowPart::None,
            "the focus waits for the menu to close"
        );
        slint::platform::update_timers_and_animations();
        assert_eq!(app.main.get_focus_speaker_part(), RowPart::Name);

        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "  Desk left ".into());
        assert_eq!(
            name_of(&app, "local:desk"),
            "Desk Speaker",
            "typing applies nothing"
        );
        app.main
            .invoke_speaker_rename_committed("local:desk".into());
        assert_eq!(name_of(&app, "local:desk"), "Desk left");
        let row = row_of(&app, "local:desk");
        assert!(!row.renaming);
        assert_eq!(
            row.system_name, "Desk Speaker",
            "the tooltip keeps the device name"
        );

        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "Other".into());
        app.main
            .invoke_speaker_rename_cancelled("local:desk".into());
        assert_eq!(name_of(&app, "local:desk"), "Desk left", "Escape keeps it");
        assert!(app.ui.borrow().rename.is_none());

        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "".into());
        app.main
            .invoke_speaker_rename_committed("local:desk".into());
        assert_eq!(
            name_of(&app, "local:desk"),
            "Desk Speaker",
            "empty brings it back"
        );
        assert_eq!(row_of(&app, "local:desk").system_name, "");
    }

    #[test]
    fn one_rename_at_a_time_and_a_click_outside_applies_it() {
        let app = app_over(MockBackend::new());
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "Desk".into());
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        assert_eq!(
            app.ui.borrow().rename,
            Some(("local:desk".to_owned(), "Desk".to_owned())),
            "asked again, it keeps the typing"
        );

        app.main.invoke_speaker_rename_requested("local:bt".into());
        assert_eq!(
            name_of(&app, "local:desk"),
            "Desk",
            "the open rename applied first"
        );
        assert!(!row_of(&app, "local:desk").renaming);
        assert!(row_of(&app, "local:bt").renaming);

        app.main
            .invoke_speaker_rename_edited("local:bt".into(), "Headset".into());
        app.main.invoke_blank_clicked();
        assert_eq!(name_of(&app, "local:bt"), "Headset");
        assert!(app.ui.borrow().rename.is_none());
    }

    #[test]
    fn a_click_on_the_row_being_renamed_only_applies_the_name() {
        let app = app_over(MockBackend::new());
        draw_frames();
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "Desk".into());
        draw_frames();
        click(app.main.window(), 600.0, 516.0);
        assert_eq!(name_of(&app, "local:desk"), "Desk");
        assert!(app.ui.borrow().rename.is_none());
        assert!(app.ui.borrow().sync_open.is_empty(), "no delay panel");

        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        app.main.invoke_sync_toggled("local:desk".into());
        assert!(app.ui.borrow().rename.is_none());
        assert!(app.ui.borrow().sync_open.is_empty(), "no delay panel");
    }

    #[test]
    fn the_name_field_stops_at_sixty_characters_while_typing() {
        let app = app_over(MockBackend::new());
        app.main.show().unwrap();
        draw_frames();
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        draw_frames();
        for _ in 0..65 {
            press(app.main.window(), "a");
        }
        let sixty = "a".repeat(60);
        assert_eq!(
            app.ui.borrow().rename,
            Some(("local:desk".to_owned(), sixty.clone()))
        );
        press(app.main.window(), Key::Return);
        assert_eq!(name_of(&app, "local:desk"), sixty);
    }

    #[test]
    fn a_speaker_gone_mid_rename_closes_its_field() {
        let app = app_over(MockBackend::new());
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "Desk".into());
        let mut unplugged = MockBackend::new();
        unplugged.retain_speakers(|s| s.id != "local:desk");
        *app.backend.borrow_mut() = Box::new(unplugged);
        app.refresh();
        assert!(app.ui.borrow().rename.is_none());

        *app.backend.borrow_mut() = Box::new(MockBackend::new());
        app.refresh();
        assert!(
            !row_of(&app, "local:desk").renaming,
            "back, its field stays closed"
        );
    }

    /// A point on the name of speaker `id`, found by double-clicking around
    /// where it sits: its place moves with the system font's line heights
    /// and widths. Leaves no field open and no sync controls shown.
    fn name_point(app: &App, id: &str) -> (f32, f32) {
        let window = app.main.window();
        let mut others = Vec::new();
        // 12 px apart, so Slint never counts two tries as one double-click.
        for x in (380..=476).step_by(12) {
            for y in (400..=716).step_by(12) {
                let (x, y) = (x as f32, y as f32);
                click(window, x, y);
                click(window, x, y);
                let renamed = app.ui.borrow().rename.as_ref().map(|(id, _)| id.clone());
                if let Some(renamed) = &renamed {
                    app.main.invoke_speaker_rename_cancelled(renamed.into());
                }
                app.ui.borrow_mut().sync_open.clear();
                app.refresh();
                draw_frames();
                match renamed {
                    Some(renamed) if renamed == id => return (x, y),
                    Some(renamed) => others.push((x, y, renamed)),
                    None => {}
                }
            }
        }
        panic!("no double-click renames {id}; these rename others: {others:?}");
    }

    #[test]
    fn a_double_click_on_a_name_renames_and_a_single_one_is_a_click_on_the_row() {
        hold_clock();
        let app = app_over(MockBackend::new());
        draw_frames();
        let window = app.main.window();
        let (x, y) = name_point(&app, "local:desk");
        click(window, x, y);
        click(window, x, y);
        assert!(
            row_of(&app, "local:desk").renaming,
            "the double-click renames"
        );
        advance_clock(Duration::from_millis(600));
        draw_frames();
        assert!(
            app.ui.borrow().sync_open.is_empty(),
            "and doesn't open the sync controls"
        );

        app.main
            .invoke_speaker_rename_cancelled("local:desk".into());
        draw_frames();
        click(window, x, y);
        assert!(
            app.ui.borrow().sync_open.is_empty(),
            "one click waits for a second"
        );
        advance_clock(Duration::from_millis(600));
        draw_frames();
        assert!(
            app.ui.borrow().sync_open.contains("local:desk"),
            "then works like a click on the row"
        );
    }

    #[test]
    fn a_click_on_a_name_waits_the_systems_double_click_time() {
        hold_clock();
        let app = app_over(MockBackend::new());
        let field = app.main.global::<NameField>();
        assert_eq!(
            field.get_double_click(),
            crate::platform::double_click_ms(),
            "set at startup"
        );
        draw_frames();
        let window = app.main.window();
        let (x, y) = name_point(&app, "local:desk");

        field.set_double_click(150);
        click(window, x, y);
        advance_clock(Duration::from_millis(250));
        draw_frames();
        assert!(
            app.ui.borrow().sync_open.contains("local:desk"),
            "a short setting answers a single click sooner than 500 ms"
        );

        app.main.invoke_sync_toggled("local:desk".into());
        assert!(app.ui.borrow().sync_open.is_empty());
        // 700 ms is past Slint's fixed 500 ms.
        field.set_double_click(3000);
        click(window, x, y);
        advance_clock(Duration::from_millis(700));
        draw_frames();
        click(window, x, y);
        assert!(
            row_of(&app, "local:desk").renaming,
            "a long setting still renames on a slow second click"
        );
        assert!(app.ui.borrow().sync_open.is_empty());
    }

    /// Double-clicks around the main window's right column, 12 px apart,
    /// until `found` says the double-click did what the test looks for;
    /// each try is undone, the zones preview put back.
    fn double_click_point(app: &App, found: impl Fn(&App) -> bool) -> (f32, f32) {
        let window = app.main.window();
        for x in (380..=440).step_by(12) {
            for y in (200..=716).step_by(12) {
                let (x, y) = (x as f32, y as f32);
                click(window, x, y);
                click(window, x, y);
                let hit = found(app);
                let _ = app.zone_dialog.hide();
                app.blank_click();
                *app.backend.borrow_mut() = Box::new(MockBackend::zones_preview());
                {
                    let mut ui = app.ui.borrow_mut();
                    ui.sync_open.clear();
                    ui.collapsed.clear();
                }
                app.refresh();
                draw_frames();
                if hit {
                    return (x, y);
                }
            }
        }
        panic!("no double-click there does it");
    }

    fn in_zones_row(app: &App, id: &str) -> SpeakerItem {
        app.models
            .this_pc_zoned
            .iter()
            .chain(app.models.connected_zoned.iter())
            .find(|s| s.id == id)
            .expect("the speaker has an In zones row")
    }

    fn zone_member_row(app: &App, id: &str) -> SpeakerItem {
        app.models
            .zones
            .iter()
            .flat_map(|zone| zone.members.iter().collect::<Vec<_>>())
            .find(|s| s.id == id)
            .expect("the speaker has a row under its zone")
    }

    fn zone_name(app: &App, id: &str) -> String {
        let state = app.snapshot();
        state
            .zones
            .iter()
            .find(|z| z.id == id)
            .unwrap()
            .name
            .clone()
    }

    #[test]
    fn a_double_click_on_a_zone_name_renames_it_in_its_header_and_a_single_one_folds_it() {
        hold_clock();
        let app = app_over(MockBackend::zones_preview());
        draw_frames();
        let window = app.main.window();
        let (x, y) = double_click_point(&app, |app| {
            press(app.main.window(), "Q");
            press(app.main.window(), Key::Return);
            zone_name(app, "zone:living") == "Q"
        });
        click(window, x, y);
        click(window, x, y);
        assert!(
            !app.zone_dialog.window().is_visible(),
            "the name is edited in the header, not in a dialog"
        );
        for key in ["D", "e", "n"] {
            press(window, key);
        }
        press(window, Key::Return);
        assert_eq!(zone_name(&app, "zone:living"), "Den");
        advance_clock(Duration::from_millis(600));
        draw_frames();
        assert!(
            !app.ui.borrow().collapsed.contains("zone:living"),
            "the double-click doesn't fold the zone"
        );

        press(window, Key::F2);
        press(window, "X");
        press(window, Key::Escape);
        assert_eq!(
            zone_name(&app, "zone:living"),
            "Den",
            "F2 on the zone's switch renames in place too; Escape keeps the name"
        );
        assert!(!app.zone_dialog.window().is_visible());

        click(window, x, y);
        assert!(
            !app.ui.borrow().collapsed.contains("zone:living"),
            "one click waits for a second"
        );
        advance_clock(Duration::from_millis(600));
        draw_frames();
        assert!(
            app.ui.borrow().collapsed.contains("zone:living"),
            "then folds the zone"
        );
    }

    #[test]
    fn a_double_click_on_an_in_zones_name_renames_the_speaker_in_that_row() {
        hold_clock();
        let app = app_over(MockBackend::zones_preview());
        draw_frames();
        let window = app.main.window();
        scroll(window, 650.0, 400.0, -3000.0);
        // In its In zones row only: a double-click on its row under Upstairs
        // renames it there.
        let renames_bedroom = |app: &App| {
            in_zones_row(app, "airplay:bedroom").renaming
                && !zone_member_row(app, "airplay:bedroom").renaming
        };
        let (x, y) = double_click_point(&app, renames_bedroom);
        click(window, x, y);
        click(window, x, y);
        let row = in_zones_row(&app, "airplay:bedroom");
        assert!(
            row.renaming && row.name_text == "Bedroom",
            "the double-click renames it in its In zones row"
        );
        assert!(
            !zone_member_row(&app, "airplay:bedroom").renaming,
            "and not under its zone as well"
        );

        for key in ["N", "o", "o", "k"] {
            press(window, key);
        }
        press(window, Key::Return);
        assert_eq!(name_of(&app, "airplay:bedroom"), "Nook");
        assert!(!in_zones_row(&app, "airplay:bedroom").renaming);

        press(window, Key::F2);
        assert!(
            in_zones_row(&app, "airplay:bedroom").renaming,
            "the focus went back to the row's switch, where F2 renames"
        );
        press(window, Key::Escape);
        assert!(!in_zones_row(&app, "airplay:bedroom").renaming);
        assert_eq!(name_of(&app, "airplay:bedroom"), "Nook");

        app.main
            .invoke_speaker_rename_requested("airplay:bedroom".into());
        assert!(zone_member_row(&app, "airplay:bedroom").renaming);
        assert!(
            !in_zones_row(&app, "airplay:bedroom").renaming,
            "a rename under the zone stays there"
        );
    }

    #[test]
    fn a_renamed_row_shows_its_device_name_under_the_new_one() {
        let app = app_over(MockBackend::new());
        let strings = app.main.global::<Strings>();
        assert_eq!(
            row_of(&app, "local:desk").system_name,
            "",
            "no caption before a rename"
        );

        for (typed, caption) in [("Desk left", true), ("Desk Speaker", false)] {
            app.main
                .invoke_speaker_rename_requested("local:desk".into());
            app.main
                .invoke_speaker_rename_edited("local:desk".into(), typed.into());
            app.main
                .invoke_speaker_rename_committed("local:desk".into());
            let row = row_of(&app, "local:desk");
            assert_eq!(!row.system_name.is_empty(), caption, "after {typed:?}");
        }

        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "Desk left".into());
        app.main
            .invoke_speaker_rename_committed("local:desk".into());
        let row = row_of(&app, "local:desk");
        assert_eq!(
            strings.invoke_speaker_device_caption(row.system_name.clone()),
            "Device name: Desk Speaker"
        );
        let flyout_row = app
            .flyout
            .get_speakers()
            .iter()
            .find(|s| s.id == "local:desk")
            .unwrap();
        assert_eq!(
            flyout_row.system_name, "Desk Speaker",
            "the tray flyout shows it too"
        );
        let label = strings.invoke_speaker_label(
            row.name.clone(),
            row.kind,
            row.show_kind,
            row.system_name,
        );
        assert!(
            label.starts_with("Desk left,"),
            "Narrator reads the new name first: {label}"
        );
    }

    #[test]
    fn rename_in_the_menu_opens_the_name_field_with_the_focus() {
        let app = app_over(MockBackend::new());
        app.main.show().unwrap();
        draw_frames();
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        draw_frames();
        assert_eq!(
            app.main_focus.borrow().speaker,
            Some(("local:desk".into(), RowPart::Name)),
            "the field took the focus"
        );
        press(app.main.window(), Key::Escape);
        assert!(app.ui.borrow().rename.is_none(), "Escape closed it");
        draw_frames();
        assert_eq!(
            app.main_focus.borrow().speaker,
            Some(("local:desk".into(), RowPart::Toggle)),
            "the focus went to the row's toggle"
        );
    }

    #[test]
    fn leaving_the_name_field_for_another_control_applies_the_name() {
        let app = app_over(MockBackend::new());
        app.main.show().unwrap();
        draw_frames();
        app.main
            .invoke_speaker_rename_requested("local:desk".into());
        draw_frames();
        app.main
            .invoke_speaker_rename_edited("local:desk".into(), "Desk".into());

        let window = app.main.window();
        window.dispatch_event(WindowEvent::WindowActiveChanged(false));
        draw_frames();
        window.dispatch_event(WindowEvent::WindowActiveChanged(true));
        draw_frames();
        assert!(
            app.ui.borrow().rename.is_some(),
            "another window taking the focus applies nothing"
        );

        press(window, Key::Tab);
        draw_frames();
        assert!(app.ui.borrow().rename.is_none(), "Tab left the field");
        assert_eq!(name_of(&app, "local:desk"), "Desk");
    }

    #[test]
    fn leaving_the_name_field_for_the_master_volume_or_airplay_delay_applies_the_name() {
        let app = app_over(MockBackend::new());
        app.main.show().unwrap();
        draw_frames();
        let window = app.main.window();
        let rename = |name: &str| {
            app.main
                .invoke_speaker_rename_requested("local:desk".into());
            draw_frames();
            app.main
                .invoke_speaker_rename_edited("local:desk".into(), name.into());
        };

        rename("Desk");
        // The master volume's slider, in the left column, below the source
        // card's level meter row.
        click(window, 150.0, 289.0);
        assert_eq!(
            app.main_focus.borrow().control,
            Some(MainControl::MasterVolume)
        );
        assert!(
            app.ui.borrow().rename.is_none(),
            "the volume took the focus"
        );
        assert_eq!(name_of(&app, "local:desk"), "Desk");

        rename("Desk left");
        // The AirPlay delay's field, under the lists scrolled to the end.
        scroll(window, 640.0, 600.0, -2000.0);
        click(window, 470.0, 620.0);
        assert_eq!(
            app.main_focus.borrow().control,
            Some(MainControl::AirplayDelay)
        );
        assert!(app.ui.borrow().rename.is_none(), "the delay field took it");
        assert_eq!(name_of(&app, "local:desk"), "Desk left");
    }

    #[test]
    fn an_idle_cast_speakers_menu_button_works_for_rename() {
        let mut backend = MockBackend::new();
        let mut speaker = backend.snapshot().speaker("local:desk").unwrap().clone();
        speaker.id = "cc:attic".into();
        speaker.name = "Attic".into();
        speaker.system_name = "Attic".into();
        speaker.kind = ui_model::SpeakerKind::Cast;
        speaker.enabled = false;
        speaker.cast = Some(ui_model::CastRow {
            state: ui_model::CastState::Idle,
            muted: false,
            volume_fixed: false,
            volume_step: 1.0,
            group: None,
            added_by_address: false,
            offline: false,
        });
        backend.add_speaker(speaker);
        let app = app_over(backend);
        app.main.show().unwrap();
        draw_frames();
        app.main.set_focus_speaker_part(RowPart::Options);
        app.main.set_focus_speaker_id("cc:attic".into());
        draw_frames();
        assert_eq!(
            app.main_focus.borrow().speaker,
            Some(("cc:attic".into(), RowPart::Options)),
            "the … button takes the focus: its menu has Rename"
        );
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

    // covers: S220
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

    // covers: S220
    #[test]
    fn an_airplay_delay_over_its_range_applies_clamped_once() {
        let backend = MockBackend::new();
        let applied = backend.airplay_delays.clone();
        let app = app_over(backend);
        let shown = || app.main.get_airplay_delay_text().to_string();
        app.main.invoke_airplay_delay_edited("25000".into());
        assert_eq!(app.snapshot().airplay_delay_ms, Some(1750.0));

        app.main.invoke_airplay_delay_committed();
        assert_eq!(
            app.snapshot().airplay_delay_ms,
            Some(ui_model::MAX_DELAY_MS)
        );
        assert_eq!(
            shown(),
            format_ms(ui_model::MAX_DELAY_MS),
            "the field shows it clamped"
        );
        assert!(!app.main.get_airplay_delay_pending());
        assert!(
            app.ui.borrow().airplay_delay_draft.draft.is_none(),
            "a focus loss after it has nothing to apply"
        );

        app.main.invoke_airplay_delay_committed();
        assert_eq!(
            applied.borrow().len(),
            1,
            "a focus loss after Return applies nothing"
        );
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
    fn a_main_window_on_a_monitor_that_is_gone_shows_on_the_primary_one() {
        let primary = crate::placement::Bounds {
            x: 0.0,
            y: 0.0,
            width: 1366.0,
            height: 728.0,
        };
        let frame = WindowFrame {
            x: 2200,
            y: 100,
            width: 800.0,
            height: 500.0,
            maximized: false,
        };
        let centered = fitted_main_frame(frame, primary, 1.0, false);
        assert_eq!(
            place_to_show(frame, false, false, false, Some((primary, 1.0))),
            Some(centered)
        );
        assert_eq!(
            place_to_show(frame, true, false, false, Some((primary, 1.0))),
            None
        );
        assert_eq!(place_to_show(frame, false, false, false, None), None);
        // Windows parks a minimized window at (-32000, -32000), on no monitor.
        let minimized = WindowFrame {
            x: -32000,
            y: -32000,
            ..frame
        };
        assert_eq!(
            place_to_show(minimized, false, true, false, Some((primary, 1.0))),
            None
        );
        // A window the user dragged partly off the screen stays where it is.
        let dragged = WindowFrame { x: -250, ..frame };
        assert_eq!(
            place_to_show(dragged, false, false, true, Some((primary, 1.0))),
            None
        );
    }

    /// Windows makes the PIN dialog at the height its layout has before the
    /// message wraps, and keeps it there when the message grows (after a
    /// wrong PIN): the buttons were cut off. The dialog now grows to fit.
    #[test]
    fn the_pin_dialog_grows_to_show_its_buttons_after_a_wrong_pin() {
        let app = app_over(MockBackend::new());
        app.preview_pin(false);
        let dialog = &app.pin_dialog;
        // About the client height Windows gave it: too short for the
        // wrong-PIN message at this width.
        dialog
            .window()
            .set_size(slint::LogicalSize::new(380.0, 150.0));
        dialog.set_wrong_pin(true);
        app.fit_pin_dialog();
        let size = dialog
            .window()
            .size()
            .to_logical(dialog.window().scale_factor());
        assert_eq!(size.width, 380.0);
        let fit = dialog.get_fit_height();
        assert!(
            size.height >= fit,
            "window {} high, layout needs {fit}",
            size.height
        );
        let bottom = dialog.get_buttons_bottom();
        assert!(
            bottom > 0.0 && bottom <= size.height,
            "buttons end at {bottom}, window {} high",
            size.height
        );
    }

    #[test]
    fn a_usb_device_with_a_form_still_reads_as_usb() {
        let app = app_over(MockBackend::new());
        for kind in [
            ui_model::SpeakerKind::Usb,
            ui_model::SpeakerKind::UsbHeadphones,
            ui_model::SpeakerKind::UsbSpeakers,
        ] {
            let label = app.strings().invoke_speaker_kind(speaker_kind(kind));
            assert_eq!(label.as_str(), "USB", "{kind:?}");
        }
    }

    #[test]
    fn an_airplay_row_names_which_airplay() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        for (kind, label) in [
            (ui_model::SpeakerKind::AirPlay, "AirPlay"),
            (ui_model::SpeakerKind::AirPlay2, "AirPlay 2"),
        ] {
            assert_eq!(
                strings.invoke_speaker_kind(speaker_kind(kind)).as_str(),
                label
            );
        }
    }

    // covers: S308 (its words; Tests/updatertexttest.sh checks how the .slint files use them)
    #[test]
    fn the_updater_texts() {
        use ui_model::UpdaterNotice as N;
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let notices = [
            N::Available {
                new: "0.9.0".to_owned(),
                running: "0.8.0".to_owned(),
                last: None,
            },
            N::Available {
                new: "0.9.0".to_owned(),
                running: "0.8.0".to_owned(),
                last: Some(ui_model::UpdateRecordReason::Blocked),
            },
            N::Available {
                new: "0.9.0".to_owned(),
                running: "0.8.0".to_owned(),
                last: Some(ui_model::UpdateRecordReason::FailedStart),
            },
            N::Checking,
            N::Downloading { percent: Some(42) },
            N::Downloading { percent: None },
            N::Installed {
                version: "0.9.0".to_owned(),
            },
            N::UpToDate {
                running: "0.8.0".to_owned(),
            },
            N::Unreachable,
            N::DownloadFailed,
            N::BadSignature,
            N::BrokenDownload,
            N::DiskFull,
            N::ReadOnly,
            N::InUse,
            N::Failed,
            N::RestoreFailed {
                renames: vec![("audiosync.exe.bak".to_owned(), "audiosync.exe".to_owned())],
            },
            N::RelaunchFailed,
            N::Blocked,
        ];
        let mut texts = HashSet::new();
        for notice in notices {
            let message = NoticeMessage::Update(notice.clone());
            let text = notice_text(&strings, &message);
            assert!(
                !text.is_empty() && !text.contains("{}"),
                "{notice:?}: {text}"
            );
            assert!(
                texts.insert(text.to_string()),
                "{notice:?} has its own text"
            );
            assert_eq!(notice_title(&strings, &message).as_str(), "update");
            let item = notice_item(
                &strings,
                &ui_model::Notice {
                    id: UPDATER_NOTICE_ID,
                    subject: "update".to_owned(),
                    message,
                    time: String::new(),
                },
            );
            assert_eq!(item.update_buttons.row_count(), notice.buttons().len());
            for button in item.update_buttons.iter() {
                assert!(!strings.invoke_update_button(button).is_empty());
            }
        }
        assert_eq!(
            notice_text(
                &strings,
                &NoticeMessage::Update(N::Available {
                    new: "0.9.0".to_owned(),
                    running: "0.8.0".to_owned(),
                    last: None,
                })
            )
            .as_str(),
            "AudioSync 0.9.0 is available. You have 0.8.0."
        );
        // covers: S314, S315, S316
        for (notice, buttons) in [
            (N::Blocked, [ui_model::UpdaterButton::OpenReleasePage]),
            (
                N::RelaunchFailed,
                [ui_model::UpdaterButton::OpenReleasePage],
            ),
            (N::BadSignature, [ui_model::UpdaterButton::OpenReleasePage]),
            (N::Failed, [ui_model::UpdaterButton::OpenReleasePage]),
        ] {
            assert_eq!(notice.buttons(), buttons, "{notice:?}");
        }
        assert_eq!(
            notice_text(
                &strings,
                &NoticeMessage::Update(N::Downloading { percent: Some(42) })
            )
            .as_str(),
            "Downloading the update: 42%"
        );
        // Every pair the restore left, the exe first, each with its own name.
        assert_eq!(
            notice_text(
                &strings,
                &NoticeMessage::Update(N::RestoreFailed {
                    renames: vec![
                        ("audiosync.exe.bak".to_owned(), "audiosync.exe".to_owned()),
                        (
                            "THIRD-PARTY-NOTICES.txt.bak".to_owned(),
                            "THIRD-PARTY-NOTICES.txt".to_owned()
                        ),
                    ]
                })
            )
            .as_str(),
            "The update failed and AudioSync couldn't put its old version back. In the \
             AudioSync folder, rename audiosync.exe.bak to audiosync.exe and \
             THIRD-PARTY-NOTICES.txt.bak to THIRD-PARTY-NOTICES.txt."
        );
        let labels: HashSet<String> = [
            UpdateButton::Download,
            UpdateButton::NotNow,
            UpdateButton::Cancel,
            UpdateButton::TryAgain,
            UpdateButton::OpenReleasePage,
            UpdateButton::RestartNow,
        ]
        .into_iter()
        .map(|button| strings.invoke_update_button(button).to_string())
        .collect();
        assert_eq!(labels.len(), 6);
        assert!(labels.contains("Download and Install") && labels.contains("Restart Now"));
        assert_eq!(
            strings.get_check_updates_automatically().as_str(),
            "Check for Updates Automatically"
        );
        assert_eq!(
            strings.get_menu_check_for_updates().as_str(),
            "Check for Updates…"
        );
        // Local and test builds have no updater, so its menu items stay hidden.
        assert!(!app.main.get_updater_available());
    }

    // covers: S308
    /// Tab reaches each button of the updater's notice, Space or Enter
    /// presses it, and the notice reports its focus.
    #[test]
    fn the_updater_notice_buttons_take_tab_then_space_or_enter() {
        let app = main_window_over(MockBackend::new());
        let item = notice_item(
            &app.strings(),
            &ui_model::Notice {
                id: UPDATER_NOTICE_ID,
                subject: "update".to_owned(),
                message: NoticeMessage::Update(ui_model::UpdaterNotice::Available {
                    new: "0.9.0".to_owned(),
                    running: "0.8.0".to_owned(),
                    last: None,
                }),
                time: String::new(),
            },
        );
        assert_eq!(
            item.update_buttons.iter().collect::<Vec<_>>(),
            [UpdateButton::Download, UpdateButton::NotNow]
        );
        let id = item.id;
        // The test's own handlers: the app's would refresh the notices away.
        let pressed = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::clone(&pressed);
        app.main
            .on_notice_update_button_pressed(move |button| log.borrow_mut().push(button));
        let focused = Rc::new(Cell::new(false));
        let seen = Rc::clone(&focused);
        app.main.on_notice_focus_changed(move |notice, now| {
            if notice == id {
                seen.set(now);
            }
        });
        app.main
            .set_notices(ModelRc::from(Rc::new(VecModel::from(vec![item]))));
        draw_frames();
        let window = app.main.window();
        let mut steps = 0;
        while !focused.get() {
            assert!(steps < 60, "Tab never reached the updater's notice");
            tab(window, 1);
            steps += 1;
        }
        press(window, " ");
        assert_eq!(*pressed.borrow(), [UpdateButton::Download], "Space");
        tab(window, 1);
        assert!(focused.get(), "Not Now is the next stop");
        press(window, Key::Return);
        assert_eq!(
            *pressed.borrow(),
            [UpdateButton::Download, UpdateButton::NotNow],
            "Enter"
        );
    }

    #[test]
    fn a_cast_row_names_its_kind_and_says_where_it_is() {
        let app = app_over(MockBackend::new());
        let state = MockBackend::new().snapshot();
        let mut speaker = state.speaker("local:desk").unwrap().clone();
        speaker.kind = ui_model::SpeakerKind::Cast;
        speaker.cast = Some(ui_model::CastRow {
            state: ui_model::CastState::Connecting,
            muted: true,
            volume_fixed: false,
            volume_step: crate::audio::cast::volume_step(Some(
                &audiosync_cast::status::DeviceVolume {
                    step_interval: Some(0.05),
                    ..Default::default()
                },
            )),
            group: None,
            added_by_address: false,
            offline: false,
        });
        let row = speaker_item(&state, &speaker, true, false, None);
        assert_eq!(row.kind, SpeakerKind::Cast);
        assert!(row.cast && row.muted && !row.volume_fixed);
        assert_eq!(row.volume_step, 5.0);
        assert_eq!(row.cast_state, CastState::Connecting);
        let strings = app.strings();
        assert_eq!(
            strings.invoke_speaker_kind(row.kind).as_str(),
            "Google Cast"
        );
        assert_eq!(
            strings
                .invoke_cast_note(row.cast_state, row.muted, "".into())
                .as_str(),
            "Connecting…"
        );
        assert_eq!(
            strings
                .invoke_cast_note(CastState::Playing, true, "".into())
                .as_str(),
            "Muted on the speaker"
        );

        let busy = NoticeMessage::CastCantPlay {
            speaker: "Kitchen".to_owned(),
            problem: ui_model::CastProblem::Busy,
        };
        assert!(notice_text(&strings, &busy).starts_with("Only one Google Cast speaker"));
        assert_eq!(notice_title(&strings, &busy).as_str(), "Kitchen can't play");
        let ahead = NoticeMessage::AirPlayAheadOfCast {
            speaker: "Kitchen".to_owned(),
            tenths: 15,
        };
        assert_eq!(
            notice_text(&strings, &ahead).as_str(),
            "AirPlay speakers can wait at most 20 s for a Google Cast speaker, so they play \
             about 1.5 s ahead of Kitchen. Lower its offset to keep them together."
        );
        assert_eq!(
            notice_title(&strings, &ahead).as_str(),
            "AirPlay ahead of Kitchen"
        );

        speaker.cast = None;
        let row = speaker_item(&state, &speaker, true, false, None);
        assert!(!row.cast);
        assert_eq!(row.volume_step, 1.0);
    }

    // covers: C68
    #[test]
    fn a_cast_row_with_a_fixed_volume_says_it_cannot_be_changed() {
        let app = app_over(MockBackend::new());
        let state = MockBackend::new().snapshot();
        let mut speaker = state.speaker("local:desk").unwrap().clone();
        speaker.kind = ui_model::SpeakerKind::Cast;
        speaker.cast = Some(ui_model::CastRow {
            state: ui_model::CastState::Playing,
            muted: false,
            volume_fixed: true,
            volume_step: 1.0,
            group: None,
            added_by_address: false,
            offline: false,
        });
        let row = speaker_item(&state, &speaker, true, false, None);
        // The slider's `enabled` and the hint in widgets.slint key off this flag.
        assert!(row.cast && row.volume_fixed);
        assert_eq!(
            app.strings().get_cast_volume_fixed().as_str(),
            "This speaker's volume can't be changed from AudioSync."
        );

        speaker.cast.as_mut().unwrap().volume_fixed = false;
        let row = speaker_item(&state, &speaker, true, false, None);
        assert!(row.cast && !row.volume_fixed);
    }

    #[test]
    fn a_cast_group_row_names_its_members_or_says_they_are_unknown() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let state = MockBackend::new().snapshot();
        let mut speaker = state.speaker("local:desk").unwrap().clone();
        speaker.name = "Attic".to_owned();
        speaker.kind = ui_model::SpeakerKind::Cast;
        let group = |members| Some(ui_model::CastGroup { members });
        speaker.cast = Some(ui_model::CastRow {
            state: ui_model::CastState::Idle,
            muted: false,
            volume_fixed: false,
            volume_step: 1.0,
            group: group(ui_model::CastMembers::NotReported),
            added_by_address: false,
            offline: false,
        });
        let row = speaker_item(&state, &speaker, true, false, None);
        assert!(row.cast && row.cast_group);
        let note = |speaker: &ui_model::Speaker| cast_group_note(&strings, speaker);
        let label = |note: SharedString| strings.invoke_cast_group_label("Attic".into(), note);
        assert_eq!(
            note(&speaker).as_str(),
            "Google Cast group",
            "before it plays"
        );
        assert_eq!(label(note(&speaker)).as_str(), "Attic, Google Cast group");

        speaker.cast.as_mut().unwrap().group = group(ui_model::CastMembers::Unknown);
        assert_eq!(
            note(&speaker).as_str(),
            "Google Cast group, members unknown"
        );
        assert_eq!(
            label(note(&speaker)).as_str(),
            "Attic, Google Cast group, members unknown"
        );

        speaker.cast.as_mut().unwrap().group = group(ui_model::CastMembers::Known(vec![
            "Kitchen".to_owned(),
            "Living Room".to_owned(),
        ]));
        assert_eq!(
            note(&speaker).as_str(),
            "Google Cast group: Kitchen and Living Room"
        );
        assert_eq!(
            label(note(&speaker)).as_str(),
            "Attic, Google Cast group: Kitchen and Living Room"
        );
        let cast_note = |state, muted| strings.invoke_cast_note(state, muted, note(&speaker));
        assert_eq!(
            cast_note(CastState::Connecting, false).as_str(),
            "Connecting…",
            "where it is comes first"
        );
        assert_eq!(
            cast_note(CastState::Playing, true).as_str(),
            "Muted on the group"
        );
        assert_eq!(
            cast_note(CastState::Playing, false).as_str(),
            "Google Cast group: Kitchen and Living Room"
        );

        let single = state.speaker("local:desk").unwrap();
        assert!(!speaker_item(&state, single, true, false, None).cast_group);
        assert_eq!(cast_group_note(&strings, single).as_str(), "");
    }

    #[test]
    fn a_member_held_by_its_target_names_the_target() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let notice = NoticeMessage::AirPlayPlaysInGroup {
            speaker: "Right".to_owned(),
            target: "Living Room".to_owned(),
        };
        assert_eq!(
            notice_text(&strings, &notice).as_str(),
            "Right plays as part of Living Room, so it stays off by itself."
        );
        assert_eq!(notice_title(&strings, &notice).as_str(), "Right can't play");
    }

    #[test]
    fn a_refusal_before_any_pairing_step_names_no_code() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let notice = NoticeMessage::AirPlayCantPlay {
            speaker: "Samsung".to_owned(),
            problem: ui_model::AirPlayProblem::NotAccepted,
        };
        let text = notice_text(&strings, &notice);
        assert_eq!(
            text.as_str(),
            "Samsung didn't accept the connection, so it's off. Turn it on again to try once more."
        );
        assert!(
            !["PIN", "code", "password"]
                .iter()
                .any(|word| text.contains(word))
        );
    }

    #[test]
    fn a_target_whose_member_asks_for_a_password_names_both() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let notice = NoticeMessage::AirPlayMemberNeedsPassword {
            target: "Living Room".to_owned(),
            member: "Right".to_owned(),
        };
        assert_eq!(
            notice_text(&strings, &notice).as_str(),
            "Right in Living Room asks for a password, so Living Room is off. Turn Living Room \
             on again to enter it."
        );
        assert_eq!(
            notice_title(&strings, &notice).as_str(),
            "Living Room can't play"
        );
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

    // covers: S181 (the hint beside the delay fields)
    #[test]
    fn the_sync_hint_names_the_reference_and_says_which_way_to_move_a_value() {
        let app = app_over(MockBackend::new());
        let strings = app.main.global::<Strings>();
        assert_eq!(
            strings.invoke_sync_hint("Speakers".into()),
            "Relative to Speakers. Heard this speaker first? Raise the value. Heard it late? Lower it."
        );
        assert_eq!(
            strings.invoke_sync_hint("".into()),
            "The speaker with the lowest value plays first, and the others wait by the difference. Heard this speaker first? Raise the value. Heard it late? Lower it."
        );

        let mut state = MockBackend::new().snapshot();
        let desk = state.speaker("local:desk").unwrap().clone();
        assert_eq!(
            speaker_item(&state, &desk, true, true, None).reference_name,
            "Speakers"
        );
        state.reference_name = None;
        assert_eq!(
            speaker_item(&state, &desk, true, true, None).reference_name,
            "",
            "without a reference the row asks for the no-reference hint"
        );

        let widgets = include_str!("../ui/widgets.slint");
        let controls = &widgets[widgets
            .find("export component SyncControls")
            .expect("the sync controls")..];
        let field = controls
            .find("text: root.delay-text;")
            .expect("the delay field");
        let hint = controls
            .find("text: root.cast ? Strings.cast-sync-hint : Strings.sync-hint(root.reference-name);")
            .expect("the hint beside the delay field");
        let end = controls[1..]
            .find("export component")
            .map_or(controls.len(), |at| at + 1);
        assert!(field < end && hint < end, "both in the sync controls");
        assert!(widgets.contains("reference-name: root.item.reference-name;"));
    }

    // covers: S181 (other input is ignored and the stored value shows again)
    #[test]
    fn a_delay_that_is_not_a_signed_whole_number_applies_nothing_and_shows_the_stored_value() {
        let app = app_over(MockBackend::new());
        app.main.invoke_sync_toggled("local:desk".into());
        assert_eq!(delay_of(&app, "local:desk"), Some(120.0));
        for typed in ["", " ", "+", "\u{2212}", "-", "+x", "\u{2212}ms", "abc"] {
            app.main
                .invoke_delay_edited("local:desk".into(), typed.into());
            app.main.invoke_delay_committed("local:desk".into());
            assert_eq!(
                delay_of(&app, "local:desk"),
                Some(120.0),
                "{typed:?} applies nothing"
            );
            assert_eq!(desk_delay_text(&app), "+120", "{typed:?}: the stored value");
            assert!(!desk_row(&app).delay_pending, "{typed:?} leaves no draft");
        }
    }

    // covers: S181 (other input is ignored and the stored value shows again)
    #[test]
    fn a_paste_holding_anything_but_a_signed_whole_number_is_blocked_whole() {
        let app = app_over(MockBackend::new());
        app.main.invoke_sync_toggled("local:desk".into());
        for pasted in ["1,5", "12a", "1.5", "\u{2212}2x"] {
            app.main
                .invoke_delay_edited("local:desk".into(), pasted.into());
            assert_eq!(desk_delay_text(&app), "+120", "{pasted:?}: no digits kept");
            assert!(!desk_row(&app).delay_pending, "{pasted:?} leaves no draft");
            app.main.invoke_delay_committed("local:desk".into());
            assert_eq!(delay_of(&app, "local:desk"), Some(120.0), "{pasted:?}");
        }

        app.main
            .invoke_delay_edited("local:desk".into(), "3".into());
        app.main
            .invoke_delay_edited("local:desk".into(), "31,5".into());
        assert_eq!(desk_delay_text(&app), "3", "the typing before it stays");

        let airplay = app.main.get_airplay_delay_text();
        app.main.invoke_airplay_delay_edited("1,5".into());
        assert_eq!(app.main.get_airplay_delay_text(), airplay);
    }

    // covers: S199 (the Windows app writes "13%" in every locale)
    #[test]
    fn the_device_volume_percent_reads_n_percent_without_a_space_in_every_locale() {
        // No translation catalog is built in, so no locale can turn "{}%"
        // into "13 %"; Slint writes an int the same way in every locale.
        let manifest = include_str!("../Cargo.toml");
        assert!(!manifest.contains("\"gettext\""), "no gettext catalogs");
        assert!(
            !include_str!("../build.rs").contains("translations"),
            "no bundled translations"
        );

        let app = app_over(MockBackend::new());
        let strings = app.main.global::<Strings>();
        let system = if cfg!(target_os = "linux") {
            "PipeWire"
        } else {
            "Windows"
        };
        for n in [0, 5, 13, 25] {
            assert_eq!(
                strings.invoke_device_volume_hint(DeviceVolumeHint::Low, n),
                format!("{system} volume is {n}%")
            );
            assert_eq!(
                strings.invoke_device_volume_hint(DeviceVolumeHint::MutedLow, n),
                format!("Muted in {system}, volume {n}%")
            );
            assert_eq!(
                strings.invoke_device_volume_hint_label(
                    "Speakers".into(),
                    DeviceVolumeHint::Low,
                    n
                ),
                format!("Speakers: {system} volume is {n}%")
            );
        }
        assert_eq!(
            strings.invoke_device_volume_fix(DeviceVolumeHint::Low),
            "Set to 75%"
        );
        assert_eq!(
            strings.invoke_device_volume_fix(DeviceVolumeHint::MutedLow),
            "Unmute at 75%"
        );
        assert_eq!(
            strings.invoke_device_volume_fix_label("Speakers".into(), DeviceVolumeHint::MutedLow),
            "Unmute at 75%, Speakers"
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

    // covers: S220
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

    // covers: S220
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

    /// Windows made the flyout at Slint's preferred height, which counts the
    /// zones notice's wrapped text as one line: the Open and Exit buttons
    /// were cut off at its bottom edge.
    #[test]
    fn the_flyout_shows_its_buttons_under_a_wrapped_notice() {
        let app = app_over(MockBackend::zones_preview());
        app.refresh();
        draw_frames();
        let flyout = &app.flyout;
        let window = flyout.window();
        let preferred = flyout.get_made_height();
        window.set_size(slint::LogicalSize::new(340.0, preferred));
        let height = app.fit_flyout(1000.0);
        let size = window.size().to_logical(window.scale_factor());
        assert_eq!(size.height, height);
        let bottom = flyout.get_footer_bottom();
        assert!(
            bottom + 16.0 <= height,
            "buttons end at {bottom}, window {height} high (Slint asked for {preferred})"
        );
        assert!(height <= bottom + 16.5, "no room left under the buttons");
    }

    /// On a screen too short for the whole flyout, the list scrolls and the
    /// buttons still show.
    #[test]
    fn a_short_screen_shortens_the_flyout_list_not_the_buttons() {
        let app = app_over(MockBackend::zones_preview());
        app.refresh();
        draw_frames();
        let flyout = &app.flyout;
        let window = flyout.window();
        window.set_size(slint::LogicalSize::new(340.0, 400.0));
        let full = app.fit_flyout(1000.0);
        let room = full - 40.0;
        let height = app.fit_flyout(room);
        assert_eq!(height, room);
        let bottom = flyout.get_footer_bottom();
        assert!(bottom + 16.0 <= room, "buttons end at {bottom} in {room}");
        assert_eq!(app.fit_flyout(1000.0), full, "the list gets its room back");
    }

    /// The refresh refits the flyout every 500 ms: a cut that still fits
    /// stays, and a change in content or room starts over.
    #[test]
    fn the_flyout_keeps_its_cuts_while_they_still_fit() {
        assert!(flyout_fit_holds(300.0, 300.0, true), "cut, fills the room");
        assert!(flyout_fit_holds(300.2, 300.0, true), "rounding");
        assert!(!flyout_fit_holds(340.0, 300.0, true), "the content grew");
        assert!(
            !flyout_fit_holds(260.0, 300.0, true),
            "it shrank or the room grew"
        );
        assert!(flyout_fit_holds(260.0, 300.0, false), "no cut, fits");
        assert!(!flyout_fit_holds(340.0, 300.0, false), "no cut, too tall");

        let app = app_over(MockBackend::zones_preview());
        app.refresh();
        draw_frames();
        let flyout = &app.flyout;
        flyout
            .window()
            .set_size(slint::LogicalSize::new(340.0, 400.0));
        let room = app.fit_flyout(1000.0) - 40.0;
        assert_eq!(app.fit_flyout(room), room);
        let cuts = (flyout.get_list_cut(), flyout.get_body_cut());
        assert!(cuts.0 > 0.0, "the list is cut");
        app.refresh();
        draw_frames();
        assert_eq!(app.fit_flyout(room), room);
        assert_eq!(
            (flyout.get_list_cut(), flyout.get_body_cut()),
            cuts,
            "nothing changed, the cuts stay"
        );

        let shorter = room - 40.0;
        assert_eq!(app.fit_flyout(shorter), shorter);
        let grown = flyout.get_list_cut() + flyout.get_body_cut();
        assert!(grown > cuts.0 + cuts.1, "less room, a deeper cut");
        assert!(
            flyout.get_footer_bottom() + 16.0 <= shorter + 0.5,
            "the buttons still show"
        );
        assert_eq!(app.fit_flyout(room), room);
        assert_eq!(
            (flyout.get_list_cut(), flyout.get_body_cut()),
            cuts,
            "the room back, the cuts as before"
        );
    }

    /// A flyout with two notices, presets and an input's channel pairs, on a
    /// screen as short as 1366x768 at 150%: the list stops shrinking at
    /// three rows, or there is none, and the buttons still show.
    fn crowded_flyout_keeps_its_buttons(mut backend: MockBackend, case: &str) {
        backend.post("source", NoticeMessage::CaptureLost, "09.40");
        backend.add_preset("evening", "Evening", &["local:desk"]);
        backend.add_preset("party", "Party", &["local:desk"]);
        let app = app_over(backend);
        let line_in = app
            .snapshot()
            .sources
            .iter()
            .position(|source| source.id == "input:line")
            .expect("listed");
        app.flyout.invoke_source_selected(line_in as i32);
        app.refresh();
        draw_frames();
        let flyout = &app.flyout;
        assert!(
            flyout.get_input_pairs().row_count() > 0,
            "{case}: pairs show"
        );
        assert!(flyout.get_presets().row_count() > 0, "{case}: presets show");
        flyout
            .window()
            .set_size(slint::LogicalSize::new(340.0, 400.0));
        let full = app.fit_flyout(1000.0);
        let room = 300.0;
        assert!(full > room + 100.0, "{case}: {full} fits {room} anyway");
        let height = app.fit_flyout(room);
        assert_eq!(height, room, "{case}");
        let bottom = flyout.get_footer_bottom();
        assert!(
            bottom + 16.0 <= room + 0.5,
            "{case}: buttons end at {bottom} in {room}"
        );
        assert_eq!(app.fit_flyout(1000.0), full, "{case}: the room comes back");
    }

    #[test]
    fn a_crowded_flyout_on_a_short_screen_keeps_its_buttons() {
        crowded_flyout_keeps_its_buttons(MockBackend::zones_preview(), "zones");
        let mut bare = MockBackend::new();
        bare.retain_speakers(|s| s.kind.is_this_pc());
        crowded_flyout_keeps_its_buttons(bare, "no list");
    }

    #[test]
    fn a_zone_lists_its_members_and_the_other_lists_only_the_rest() {
        let app = app_over(MockBackend::zones_preview());
        let ids = |model: &VecModel<SpeakerItem>| -> Vec<String> {
            model.iter().map(|row| row.id.to_string()).collect()
        };
        let m = &app.models;
        let zoned = [
            "airplay:kitchen",
            "airplay:pair",
            "cc:attic",
            "local:desk",
            "airplay:bedroom",
            "airplay:left",
        ];
        for list in [ids(&m.this_pc), ids(&m.connected)] {
            assert!(
                list.iter().all(|id| !zoned.contains(&id.as_str())),
                "{list:?}"
            );
        }
        assert!(ids(&m.this_pc).iter().any(|id| id == "local:speakers"));
        assert!(ids(&m.connected).iter().any(|id| id == "airplay:living"));
        assert_eq!(
            ids(&m.all_speakers),
            [ids(&m.this_pc), ids(&m.connected)].concat()
        );
        let living = m.zones.row_data(0).unwrap();
        assert_eq!(
            living
                .members
                .iter()
                .map(|row| row.id.to_string())
                .collect::<Vec<_>>(),
            [
                "airplay:kitchen",
                "airplay:pair",
                "cc:attic",
                "local:desk",
                "airplay:theater:4C2D"
            ],
            "in zone order, the one not found too; the hidden one has no row"
        );
        assert_eq!(
            (living.on, living.total, living.waiting, living.present),
            (2, 5, 1, 4)
        );
        let upstairs = m.zones.row_data(1).unwrap();
        let left = upstairs.members.row_data(1).unwrap();
        assert_eq!(left.zone_note, "Plays as part of Living Room Pair");
        assert!(left.zone_dimmed);
        assert!(!left.zone_movable);
        assert_eq!(
            upstairs.targets.row_data(0).unwrap().member_id,
            "airplay:left"
        );
        assert_eq!(m.zones.row_data(2).unwrap().total, 0, "Office is empty");
        let compact = ids(&m.connected_zoned);
        assert!(
            compact.iter().any(|id| id == "airplay:kitchen"),
            "{compact:?}"
        );
        assert!(
            compact.iter().all(|id| zoned.contains(&id.as_str())),
            "only zone members get a compact row"
        );
        let kitchen = m
            .connected_zoned
            .iter()
            .find(|row| row.id == "airplay:kitchen")
            .unwrap();
        assert_eq!(kitchen.zone_name, "Living Room");
        assert_eq!(kitchen.zone_id, "zone:living");
    }

    // covers: S245 (a zone's rows and its count agree; a hidden member has
    // neither, one not found has a row)
    #[test]
    fn every_zone_member_but_the_hidden_ones_has_a_row_that_its_count_counts() {
        let app = app_over(MockBackend::zones_preview());
        for zone in app.models.zones.iter() {
            assert_eq!(
                zone.members.row_count(),
                zone.total as usize,
                "{}",
                zone.name
            );
            assert_eq!(
                zone.members.iter().filter(|row| !row.missing).count(),
                zone.present as usize,
                "{}",
                zone.name
            );
        }
        let living = app.models.zones.row_data(0).unwrap();
        assert!(living.members.iter().all(|row| row.id != "airplay:den"));
        let theater = living.members.row_data(4).unwrap();
        assert!(theater.missing && theater.zone_dimmed && theater.zone_movable);
        assert_eq!(theater.name, "Home Theater");
        assert_eq!(theater.zone_note, "Not found");
        assert_eq!(theater.zone_id, "zone:living");

        // Remove from Zone still reaches it.
        app.main
            .invoke_speaker_remove_from_zone_requested("airplay:theater:4C2D".into());
        let living = app.models.zones.row_data(0).unwrap();
        assert_eq!((living.members.row_count(), living.total), (4, 4));
    }

    // covers: S246 (a Cast member a zone click left off says why), S247 (a
    // pair needs AirPlay 2)
    #[test]
    fn zone_member_rows_say_why_they_dont_play() {
        let app = app_over(MockBackend::zones_preview());
        let living = || app.models.zones.row_data(0).unwrap();
        let attic = living().members.row_data(2).unwrap();
        assert_eq!(attic.id, "cc:attic");
        assert_eq!(
            attic.zone_note, "Stayed off, since one Cast speaker plays at a time",
            "another member is on"
        );
        assert!(!attic.zone_dimmed);
        let pair = living().members.row_data(1).unwrap();
        assert_eq!(pair.zone_note, "", "AirPlay 2 can play it");

        let strings = app.strings();
        assert_eq!(
            zone_note(&strings, ui_model::ZoneNote::NeedsAirPlay2),
            "Needs AirPlay 2 (Experimental)"
        );
        let mut state = app.snapshot();
        let zone = state.zones[0].clone();
        let pair = state
            .speakers
            .iter_mut()
            .find(|s| s.id == "airplay:pair")
            .unwrap();
        pair.availability = ui_model::Availability::AirPlay2NotYet;
        let pair = state.speaker("airplay:pair").unwrap();
        assert_eq!(
            state.zone_note(&zone, ui_model::ZoneRow::Listed(pair)),
            Some(ui_model::ZoneNote::NeedsAirPlay2)
        );
        // One look for the note, listed or not.
        assert!(zone_row_dimmed(
            ui_model::ZoneRow::Listed(pair),
            ui_model::ZoneNote::NeedsAirPlay2
        ));
        assert!(zone_row_dimmed(
            ui_model::ZoneRow::NotFound("airplay:theater:4C2D"),
            ui_model::ZoneNote::NeedsAirPlay2
        ));
        for speaker in &mut state.speakers {
            if speaker.id != "cc:attic" {
                speaker.enabled = false;
            }
        }
        let attic = state.speaker("cc:attic").unwrap();
        assert_eq!(
            state.zone_note(&zone, ui_model::ZoneRow::Listed(attic)),
            None,
            "with nothing else on, a Cast speaker that is off is just off"
        );
    }

    /// As on the Mac, the Cast note needs a Cast speaker on: a zone of the
    /// PC's speakers and a Cast speaker that is off has no note.
    // covers: S246
    #[test]
    fn a_cast_member_off_beside_a_local_speaker_has_no_note() {
        let mut backend = MockBackend::zones_preview();
        backend.add_zone("zone:pc", "PC", &["local:speakers", "cc:attic"]);
        backend.retain_speakers(|s| s.id != "cc:porch");
        let app = app_over(backend);
        let state = app.snapshot();
        let zone = state.zones.iter().find(|z| z.id == "zone:pc").unwrap();
        assert!(state.speaker("local:speakers").unwrap().enabled);
        let attic = state.speaker("cc:attic").unwrap();
        assert!(!attic.enabled);
        assert_eq!(
            state.zone_note(zone, ui_model::ZoneRow::Listed(attic)),
            None,
            "no Cast speaker is on, so nothing kept this one off"
        );
    }

    #[test]
    fn a_zone_header_counts_its_speakers() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        assert_eq!(
            strings.invoke_zone_count(2, 5, 1, 4),
            "2 of 5 on, 1 waiting"
        );
        assert_eq!(strings.invoke_zone_count(1, 3, 0, 2), "1 of 3 on");
        assert_eq!(strings.invoke_zone_count(0, 3, 0, 0), "None of 3 found");
        assert_eq!(
            strings.invoke_zone_value(2, 5, 1),
            "2 of 5 speakers on, 1 waiting"
        );
        assert_eq!(strings.invoke_zone_value(1, 1, 0), "1 of 1 speaker on");
        assert_eq!(
            strings.invoke_zone_value(1, 1, 1),
            "1 of 1 speaker on, 1 waiting"
        );
        assert_eq!(
            strings.invoke_zone_label("Living Room".into()),
            "Living Room, zone"
        );
    }

    #[test]
    fn go_to_zone_with_no_member_found_lands_on_the_arrow_which_folds_it() {
        let mut backend = MockBackend::new();
        backend.add_zone("zone:garage", "Garage", &["airplay:not-here"]);
        let app = app_over(backend);
        app.main.show().unwrap();
        draw_frames();
        let expanded = || {
            app.models
                .zones
                .iter()
                .find(|zone| zone.id == "zone:garage")
                .expect("the zone is listed")
                .expanded
        };
        assert!(expanded());

        app.main.set_focus_zone_part(RowPart::Toggle);
        app.main.set_focus_zone_id("zone:garage".into());
        draw_frames();
        assert_eq!(
            app.main_focus.borrow().zone,
            Some(("zone:garage".into(), RowPart::Toggle)),
            "the arrow stands in for the toggle, which has nothing to switch"
        );
        press(app.main.window(), " ");
        assert!(!expanded(), "Space folded the zone");
        press(app.main.window(), Key::Return);
        assert!(expanded(), "Enter unfolded it");
    }

    #[test]
    fn a_zone_header_folds_and_unfolds_through_its_accessibility_actions() {
        use i_slint_backend_testing::ElementHandle;
        let app = app_over(MockBackend::zones_preview());
        app.main.show().unwrap();
        draw_frames();
        let expanded = || {
            app.models
                .zones
                .iter()
                .find(|zone| zone.id == "zone:living")
                .expect("the zone is listed")
                .expanded
        };
        let element = |label: &str| {
            ElementHandle::find_by_accessible_label(&app.main, label)
                .next()
                .unwrap_or_else(|| panic!("nothing is labeled {label}"))
        };
        assert!(expanded());

        // UI Automation offers Invoke only on a node with no expanded state,
        // and Slint 1.18 passes no collapse action on.
        let arrow = element("Hide speakers in Living Room");
        assert_ne!(
            arrow.accessible_expandable(),
            Some(true),
            "UI Automation can't fold through an expandable arrow"
        );
        arrow.invoke_accessible_default_action();
        draw_frames();
        assert!(!expanded(), "Invoke on the arrow folded the zone");

        let arrow = element("Show speakers in Living Room");
        assert_ne!(arrow.accessible_expandable(), Some(true));
        arrow.invoke_accessible_default_action();
        draw_frames();
        assert!(expanded(), "Invoke on the arrow unfolded it");

        app.main
            .invoke_zone_expanded_changed("zone:living".into(), false);
        draw_frames();
        let toggle = element("Living Room, zone");
        assert_eq!(toggle.accessible_expanded(), Some(false));
        toggle.invoke_accessible_expand_action();
        draw_frames();
        assert!(expanded(), "Expand on the zone's toggle unfolded it");
    }

    #[test]
    fn a_flyout_zone_header_folds_and_unfolds_through_its_accessibility_actions() {
        use i_slint_backend_testing::ElementHandle;
        let app = app_over(MockBackend::zones_preview());
        app.flyout.show().expect("the flyout shows headless");
        draw_frames();
        let expanded = || {
            app.models
                .zones
                .iter()
                .find(|zone| zone.id == "zone:living")
                .expect("the zone is listed")
                .expanded
        };
        let element = |label: &str| {
            ElementHandle::find_by_accessible_label(&app.flyout, label)
                .next()
                .unwrap_or_else(|| panic!("nothing in the flyout is labeled {label}"))
        };
        assert!(expanded());

        let arrow = element("Hide speakers in Living Room");
        assert_ne!(
            arrow.accessible_expandable(),
            Some(true),
            "UI Automation can't fold through an expandable arrow"
        );
        arrow.invoke_accessible_default_action();
        draw_frames();
        assert!(!expanded(), "Invoke on the flyout's arrow folded the zone");

        let arrow = element("Show speakers in Living Room");
        assert_ne!(arrow.accessible_expandable(), Some(true));
        arrow.invoke_accessible_default_action();
        draw_frames();
        assert!(expanded(), "Invoke on the flyout's arrow unfolded it");

        app.flyout
            .invoke_zone_expanded_changed("zone:living".into(), false);
        draw_frames();
        let toggle = element("Living Room, zone");
        assert_eq!(toggle.accessible_expanded(), Some(false));
        toggle.invoke_accessible_expand_action();
        draw_frames();
        assert!(expanded(), "Expand on the flyout zone's toggle unfolded it");
    }

    #[test]
    fn speakers_move_between_zones_and_back_from_their_menus() {
        let app = app_over(MockBackend::zones_preview());
        let main = &app.main;
        main.invoke_speaker_move_to_zone_requested("local:speakers".into(), "zone:office".into());
        let state = app.snapshot();
        assert_eq!(
            state.zone_of("local:speakers").map(|zone| zone.id.as_str()),
            Some("zone:office")
        );
        assert!(
            app.models
                .this_pc
                .iter()
                .all(|row| row.id != "local:speakers")
        );
        let office = app.models.zones.row_data(2).unwrap();
        assert_eq!(office.members.row_data(0).unwrap().zone_id, "zone:office");

        main.invoke_zone_expanded_changed("zone:office".into(), false);
        assert!(!app.models.zones.row_data(2).unwrap().expanded);
        main.invoke_zone_expanded_changed("zone:office".into(), true);
        assert!(app.models.zones.row_data(2).unwrap().expanded);
        // The flyout folds the same zone the main window does, until AudioSync quits.
        app.flyout
            .invoke_zone_expanded_changed("zone:office".into(), false);
        assert!(!app.models.zones.row_data(2).unwrap().expanded);
        app.flyout
            .invoke_zone_expanded_changed("zone:office".into(), true);
        assert!(app.models.zones.row_data(2).unwrap().expanded);

        main.invoke_speaker_remove_from_zone_requested("local:speakers".into());
        assert!(app.snapshot().zone_of("local:speakers").is_none());
        assert!(
            app.models
                .this_pc
                .iter()
                .any(|row| row.id == "local:speakers")
        );

        main.invoke_speaker_new_zone_requested("local:headphones".into());
        assert!(app.zone_dialog.window().is_visible());
        app.confirm_dialog("  ");
        let state = app.snapshot();
        let zone = state
            .zone_of("local:headphones")
            .expect("a zone of its own");
        assert_eq!(zone.name, "Zone 4", "named after the zones there are");

        let id = zone.id.clone();
        main.invoke_zone_deleted(id.as_str().into());
        assert!(app.snapshot().zone_of("local:headphones").is_none());
        assert_eq!(app.models.zones.row_count(), 3);
    }

    #[test]
    fn a_zone_is_renamed_from_its_menu_in_its_header_like_a_speaker() {
        let app = app_over(MockBackend::zones_preview());
        let office = || app.models.zones.row_data(2).unwrap();
        app.main.invoke_zone_rename_requested("zone:office".into());
        assert!(!app.zone_dialog.window().is_visible(), "no dialog");
        assert!(office().renaming && office().name_text == "Office");
        slint::platform::update_timers_and_animations();
        assert_eq!(app.main.get_focus_zone_part(), RowPart::Name);

        app.main
            .invoke_zone_rename_edited("zone:office".into(), "Study".into());
        app.main.invoke_zone_rename_committed("zone:office".into());
        assert_eq!(office().name, "Study");
        assert!(!office().renaming);

        app.main.invoke_zone_rename_requested("zone:office".into());
        app.main
            .invoke_zone_rename_edited("zone:office".into(), "Den".into());
        app.main.invoke_zone_rename_cancelled("zone:office".into());
        assert_eq!(office().name, "Study", "Escape keeps the name");

        app.main.invoke_zone_rename_requested("zone:office".into());
        app.main
            .invoke_zone_rename_edited("zone:office".into(), "  ".into());
        app.main.invoke_zone_rename_committed("zone:office".into());
        assert_eq!(office().name, "Study", "a blank name keeps it too");

        app.main.invoke_zone_rename_requested("zone:office".into());
        app.main
            .invoke_speaker_rename_requested("airplay:living".into());
        assert_eq!(office().name, "Study");
        assert!(
            !office().renaming,
            "a speaker's rename closes the zone's field"
        );
        assert!(row_of(&app, "airplay:living").renaming);
        app.main.invoke_zone_rename_requested("zone:office".into());
        assert!(!row_of(&app, "airplay:living").renaming, "and back");
        assert!(office().renaming);

        app.main.invoke_zone_deleted("zone:office".into());
        assert!(
            app.ui.borrow().rename.is_none(),
            "a deleted zone's field goes"
        );
    }

    #[test]
    fn the_zone_name_field_stops_at_sixty_characters_while_typing() {
        let app = app_over(MockBackend::zones_preview());
        app.main.show().unwrap();
        draw_frames();
        app.main.invoke_zone_rename_requested("zone:office".into());
        draw_frames();
        for _ in 0..65 {
            press(app.main.window(), "a");
        }
        let sixty = "a".repeat(60);
        assert_eq!(
            app.ui.borrow().rename,
            Some(("zone:office".to_owned(), sixty.clone()))
        );
        press(app.main.window(), Key::Return);
        assert_eq!(app.models.zones.row_data(2).unwrap().name.as_str(), sixty);
    }

    #[test]
    fn use_target_instead_swaps_the_member_for_its_pair() {
        let app = app_over(MockBackend::zones_preview());
        app.main
            .invoke_zone_use_target_requested("airplay:left".into());
        let upstairs = app.models.zones.row_data(1).unwrap();
        assert_eq!(upstairs.members.row_count(), 1);
        assert_eq!(upstairs.targets.row_count(), 0);
    }

    // covers: S255
    #[test]
    fn a_zone_run_is_ticked_once_it_starts() {
        let app = app_over(MockBackend::zones_preview());
        app.main
            .invoke_zone_calibrate_requested("zone:living".into());
        assert!(app.snapshot().calibration.busy);
        assert!(
            app.calibration_timer.running(),
            "a run that no tick moves on stalls at its first step"
        );
    }

    // covers: S255
    #[test]
    fn calibrate_zone_is_offered_for_a_zone_with_a_member_here() {
        let app = app_over(MockBackend::zones_preview());
        assert!(app.models.zones.row_data(0).unwrap().can_calibrate);
        assert!(
            !app.models.zones.row_data(2).unwrap().can_calibrate,
            "none of the office's members is here"
        );
    }

    // covers: S255
    #[test]
    fn the_calibrate_all_card_of_a_zone_run_names_the_zone() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let mut state = app.snapshot();
        state.calibrate_all.phase = AllPhase::Failed(Failure::NothingToMeasure);
        state.calibrate_all.zone = Some(ui_model::ZoneSheet {
            name: "Upstairs".into(),
            off_rows: Vec::new(),
            airplay_moves: AirPlayMoves {
                zones: vec!["Kitchen".into(), "Garden".into()],
                no_zone: true,
            },
        });
        let card = calibrate_all_card(&strings, &state);
        assert_eq!(
            card.status,
            "Nothing to measure in “Upstairs”. Turn on a speaker in this zone first. No delay was changed."
        );
        assert!(
            card.zone.starts_with("Bring the PC into “Upstairs”"),
            "{}",
            card.zone
        );
        let moves = |zones: &[&str], no_zone| {
            let moves = AirPlayMoves {
                zones: zones.iter().map(|name| (*name).to_owned()).collect(),
                no_zone,
            };
            airplay_moves_line(&strings, &moves).map(|line| line.to_string())
        };
        let share = "AirPlay speakers share one delay, so a new AirPlay delay applies to ";
        assert_eq!(moves(&[], false), None);
        assert_eq!(
            moves(&[], true).unwrap(),
            format!("{share}the AirPlay speakers in no zone too.")
        );
        assert_eq!(
            moves(&["Kitchen", "Garden"], true).unwrap(),
            format!(
                "{share}the AirPlay speakers in “Kitchen” and “Garden”, and those in no zone too."
            )
        );

        let row = |name: &str, state| SheetRow {
            name: name.into(),
            class: Class::Local,
            old_ms: Some(0.0),
            state,
            can_measure_again: true,
        };
        state.calibrate_all.phase = AllPhase::Idle;
        state.calibrate_all.sheet = Some(SheetState {
            rows: vec![
                row("Speakers", SheetRowState::Reference),
                row("TV", SheetRowState::NotMeasured),
                row("Desk", SheetRowState::NotMeasured),
            ],
            airplay: None,
            ended: None,
            failure: None,
            again_ended: None,
            can_apply: false,
            applied: false,
            can_undo: false,
            undone: false,
        });
        if let Some(zone) = state.calibrate_all.zone.as_mut() {
            zone.off_rows = vec![1];
        }
        let card = calibrate_all_card(&strings, &state);
        assert_eq!(
            card.rows[1].detail,
            "Off, so it wasn't measured. Turn it on and choose Measure Again."
        );
        assert_eq!(
            card.rows[2].detail,
            strings.get_calibrate_all_not_measured()
        );

        state.calibrate_all.zone = None;
        let card = calibrate_all_card(&strings, &state);
        assert!(card.zone.is_empty());
        assert_eq!(
            card.rows[1].detail,
            strings.get_calibrate_all_not_measured()
        );
        assert!(!card.status.contains("Upstairs"), "a whole run's text");
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

    /// The flyout with two notices (it lists the newest), one zone and
    /// three speakers.
    const FLYOUT: FlyoutCounts = FlyoutCounts {
        notices: 2,
        listed_notices: 1,
        zones: 1,
        speakers: 3,
        zone_members: 1,
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
        let zone_added = FlyoutCounts { zones: 2, ..FLYOUT };
        assert!(FLYOUT.removed_by(zone_added).zones, "rows are rebuilt");
        let member_moved = FlyoutCounts {
            zone_members: 2,
            ..FLYOUT
        };
        assert!(
            FLYOUT.removed_by(member_moved).speakers,
            "a zone's member rows are rebuilt"
        );
    }

    #[test]
    fn folding_a_zone_in_the_flyout_removes_its_member_rows() {
        assert_eq!(listed_members(true, 3), 3);
        assert_eq!(listed_members(false, 3), 0);
        let open = FlyoutCounts {
            zone_members: zone_shape([listed_members(true, 2)].into_iter()),
            ..FLYOUT
        };
        let folded = FlyoutCounts {
            zone_members: zone_shape([listed_members(false, 2)].into_iter()),
            ..FLYOUT
        };
        assert!(
            open.removed_by(folded).speakers,
            "a member that had the focus gives it back"
        );
        let empty_open = FlyoutCounts {
            zone_members: zone_shape([listed_members(true, 0)].into_iter()),
            ..FLYOUT
        };
        let empty_folded = FlyoutCounts {
            zone_members: zone_shape([listed_members(false, 0)].into_iter()),
            ..FLYOUT
        };
        assert_eq!(
            empty_open.removed_by(empty_folded),
            FlyoutParts::default(),
            "an empty zone has no rows to remove"
        );
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
                zones: true,
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

    /// The App over the example data with Kitchen asking for its password.
    fn app_asking_password(wrong: bool) -> Rc<App> {
        let mut backend = MockBackend::new();
        backend.ask_password("airplay:kitchen", wrong);
        app_over(backend)
    }

    #[test]
    fn the_password_dialog_pairs_with_what_was_typed() {
        let app = app_asking_password(false);
        let window = app.password_dialog.window();
        assert!(window.is_visible(), "it opens with the prompt");
        assert!(!app.password_dialog.get_wrong());
        press(window, Key::Return);
        assert!(window.is_visible(), "Enter waits for a password");
        for key in ["p", "w"] {
            press(window, key);
        }
        press(window, Key::Return);
        assert!(!window.is_visible());
        assert_eq!(
            app.password_dialog.get_password(),
            "",
            "not kept in the field"
        );
        let state = app.snapshot();
        assert_eq!(state.password_prompt, None);
        assert!(state.speaker("airplay:kitchen").unwrap().enabled);
    }

    /// Moves the main window's focus without running its `changed has-focus`
    /// handlers, as a UI Automation client's focus and toggle in one turn
    /// leave them, then turns `id` on: showing a dialog runs those handlers.
    fn turn_on_with_a_focus_change_pending(app: &App, id: &str) {
        app.main.show().unwrap();
        draw_frames();
        app.main.invoke_focus_streaming_switch();
        app.main.invoke_speaker_toggled(id.into());
    }

    #[test]
    fn a_focus_change_pending_as_the_pin_dialog_opens_is_handled() {
        let app = app_over(MockBackend::new());
        turn_on_with_a_focus_change_pending(&app, "airplay:living");
        assert!(app.pin_dialog.window().is_visible());
    }

    #[test]
    fn a_focus_change_pending_as_the_password_dialog_opens_is_handled() {
        let mut backend = MockBackend::new();
        backend.ask_password_when_on("airplay:kitchen");
        let app = app_over(backend);
        turn_on_with_a_focus_change_pending(&app, "airplay:kitchen");
        assert!(app.password_dialog.window().is_visible());
    }

    #[test]
    fn escape_on_the_password_dialog_cancels_from_the_field_or_a_button() {
        for tabs in [0, 2] {
            let app = app_asking_password(true);
            let window = app.password_dialog.window();
            assert!(app.password_dialog.get_wrong(), "says it was turned down");
            press(window, "x");
            tab(window, tabs);
            press(window, Key::Escape);
            assert!(!window.is_visible(), "Escape closed it ({tabs} tabs)");
            assert_eq!(app.password_dialog.get_password(), "");
            let state = app.snapshot();
            assert_eq!(state.password_prompt, None);
            assert!(!state.speaker("airplay:kitchen").unwrap().enabled);
        }
    }

    fn type_text(window: &slint::Window, text: &str) {
        for character in text.chars() {
            press(window, character.to_string());
        }
    }

    // covers: C18
    #[test]
    fn add_by_address_keeps_a_turned_down_address_open_with_the_reason() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        app.main.invoke_cast_add_address_requested();
        let dialog = &app.cast_address_dialog;
        let window = dialog.window();
        assert!(window.is_visible(), "Speakers > Add by Address… opens it");
        assert_eq!(dialog.get_error(), "");

        type_text(window, "not an address");
        press(window, Key::Return);
        assert!(window.is_visible(), "stays open");
        assert_eq!(dialog.get_error(), strings.get_cast_address_invalid());
        assert_eq!(
            dialog.get_error(),
            "That isn't an IP address or host name. Check it and try again."
        );

        // The field took the cursor back with its text selected.
        type_text(window, "speaker.invalid");
        press(window, Key::Return);
        settle_cast_check(&app);
        assert!(window.is_visible());
        assert_eq!(
            dialog.get_error(),
            "AudioSync couldn't find a speaker at that address. Check that it's on and try again."
        );

        type_text(window, "speaker.local");
        press(window, Key::Return);
        settle_cast_check(&app);
        assert!(!window.is_visible(), "Enter added it");
        assert_eq!(dialog.get_address(), "", "the field is cleared");
        let state = app.snapshot();
        let row = state.speaker("cc:speaker.local").expect("listed");
        assert!(row.cast.as_ref().is_some_and(|cast| cast.added_by_address));
    }

    /// Waits for the address check Add by Address… started, if any, and
    /// takes its answer.
    fn settle_cast_check(app: &App) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while app.cast_check.borrow().is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
            app.take_cast_check();
        }
        draw_frames();
    }

    /// What the dialog's two live regions hold, in order.
    fn cast_address_said(app: &App) -> (String, String) {
        let dialog = &app.cast_address_dialog;
        (dialog.get_said_odd().into(), dialog.get_said_even().into())
    }

    #[test]
    fn every_turned_down_address_is_announced_even_with_the_same_reason() {
        let app = app_over(MockBackend::new());
        let invalid: String = app.strings().get_cast_address_invalid().into();
        let not_found: String = app.strings().get_cast_address_not_found().into();
        app.main.invoke_cast_add_address_requested();
        let window = app.cast_address_dialog.window();
        assert_eq!(cast_address_said(&app), (String::new(), String::new()));

        type_text(window, "not an address");
        press(window, Key::Return);
        assert_eq!(cast_address_said(&app), (invalid.clone(), String::new()));

        // The same reason again moves to the other region, which Narrator
        // reads as new; text that only stayed the same it would skip.
        type_text(window, "still not one");
        press(window, Key::Return);
        assert_eq!(cast_address_said(&app), (String::new(), invalid.clone()));

        press(window, Key::Return);
        assert_eq!(
            cast_address_said(&app),
            (invalid, String::new()),
            "Enter on the same address"
        );

        type_text(window, "speaker.invalid");
        press(window, Key::Return);
        settle_cast_check(&app);
        assert_eq!(cast_address_said(&app), (String::new(), not_found));

        press(window, Key::Escape);
        assert_eq!(cast_address_said(&app), (String::new(), String::new()));
        app.main.invoke_cast_add_address_requested();
        assert_eq!(
            cast_address_said(&app),
            (String::new(), String::new()),
            "reopened with nothing to say"
        );
    }

    #[test]
    fn add_waits_for_an_address_and_enter_on_a_blank_field_does_nothing() {
        let app = app_over(MockBackend::new());
        app.main.invoke_cast_add_address_requested();
        let dialog = &app.cast_address_dialog;
        let window = dialog.window();
        assert!(!dialog.get_can_add(), "Add is off on an empty field");
        press(window, Key::Return);
        type_text(window, "   ");
        assert!(!dialog.get_can_add(), "spaces alone are no address");
        press(window, Key::Return);
        assert!(window.is_visible(), "Enter didn't add or close");
        assert_eq!(dialog.get_error(), "", "no reason for an empty field");
        assert_eq!(cast_address_said(&app), (String::new(), String::new()));

        type_text(window, "speaker.local");
        assert!(dialog.get_can_add());
        press(window, Key::Return);
        settle_cast_check(&app);
        assert!(!window.is_visible(), "Enter added it");
        assert!(app.snapshot().speaker("cc:speaker.local").is_some());
    }

    #[test]
    fn add_by_address_waits_for_the_check_off_the_ui_thread_and_cancel_drops_its_answer() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let speakers = app.snapshot().speakers.len();
        let dialog = &app.cast_address_dialog;
        let window = dialog.window();
        let hold = || {
            MOCK_CAST_HOLD
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };

        let held = hold();
        app.main.invoke_cast_add_address_requested();
        type_text(window, "speaker.held");
        press(window, Key::Return);
        assert!(window.is_visible(), "open while the address is checked");
        assert!(dialog.get_checking());
        assert!(!dialog.get_can_add(), "Add is off during the check");
        assert_eq!(strings.get_cast_address_checking(), "Checking…");
        press(window, Key::Return);
        assert!(app.snapshot().speaker("cc:speaker.held").is_none());
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape closes it during the check");
        assert!(!dialog.get_checking());
        drop(held);
        std::thread::sleep(Duration::from_millis(200));
        draw_frames();
        assert_eq!(
            app.snapshot().speakers.len(),
            speakers,
            "the late answer is dropped"
        );

        let held = hold();
        app.main.invoke_cast_add_address_requested();
        assert!(!dialog.get_checking());
        assert_eq!(dialog.get_error(), "");
        type_text(window, "speaker.held");
        press(window, Key::Return);
        assert!(dialog.get_checking());
        drop(held);
        settle_cast_check(&app);
        assert!(!window.is_visible(), "added once the check answered");
        assert!(!dialog.get_checking());
        assert!(app.snapshot().speaker("cc:speaker.held").is_some());
    }

    #[test]
    fn reopening_add_by_address_during_a_check_drops_the_old_answer() {
        let app = app_over(MockBackend::new());
        let speakers = app.snapshot().speakers.len();
        let window = app.cast_address_dialog.window();
        let held = MOCK_CAST_HOLD
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        app.main.invoke_cast_add_address_requested();
        type_text(window, "speaker.held");
        press(window, Key::Return);
        assert!(app.cast_address_dialog.get_checking());

        app.ask_cast_address();
        drop(held);
        settle_cast_check(&app);
        std::thread::sleep(Duration::from_millis(200));
        draw_frames();
        assert!(window.is_visible(), "the reopened dialog stays open");
        assert!(app.snapshot().speaker("cc:speaker.held").is_none());
        assert_eq!(app.snapshot().speakers.len(), speakers);
    }

    #[test]
    fn escape_on_the_add_by_address_dialog_cancels_from_the_field_or_a_button() {
        for tabs in [0, 1, 2] {
            let app = app_over(MockBackend::new());
            let speakers = app.snapshot().speakers.len();
            app.main.invoke_cast_add_address_requested();
            let window = app.cast_address_dialog.window();
            type_text(window, "speaker.local");
            tab(window, tabs);
            press(window, Key::Escape);
            assert!(!window.is_visible(), "Escape closed it ({tabs} tabs)");
            assert_eq!(app.snapshot().speakers.len(), speakers, "nothing added");
        }
    }

    // covers: C18
    #[test]
    fn a_speaker_added_by_address_says_so_and_can_be_removed() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        app.update(|b| {
            b.add_cast_address("192.0.2.20").expect("added");
        });
        let state = app.snapshot();
        let speaker = state.speaker("cc:192.0.2.20").expect("listed");
        let row = speaker_item(&state, speaker, true, false, None);
        assert!(row.cast && row.cast_added_by_address);
        assert!(!row.cast_group);
        assert_eq!(strings.get_cast_added_by_address(), "Added by address");
        assert_eq!(strings.get_cast_remove(), "Remove");
        let desk = state.speaker("local:desk").unwrap();
        assert!(!speaker_item(&state, desk, true, false, None).cast_added_by_address);

        app.main
            .invoke_cast_remove_requested("cc:192.0.2.20".into());
        assert!(app.snapshot().speaker("cc:192.0.2.20").is_none());
    }

    #[test]
    fn escape_on_the_zone_dialog_buttons_cancels() {
        let app = app_over(MockBackend::new());
        let zones = app.snapshot().zones.len();
        app.ask_new_zone();
        let window = app.zone_dialog.window();
        assert!(window.is_visible());
        assert!(app.zone_dialog.get_show_field(), "a speaker is on");
        // From the name field to Create.
        tab(window, 1);
        press(window, "x");
        assert_eq!(app.zone_dialog.get_name(), "", "the field lost the focus");
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape on Create closed the dialog");
        assert_eq!(app.snapshot().zones.len(), zones, "and made no zone");
    }

    // covers: S270
    #[test]
    fn curves_are_saved_picked_renamed_and_deleted_and_custom_is_no_entry() {
        let mut mock = MockBackend::new();
        mock.add_zone("zone:office", "Office", &[]);
        let app = app_over(mock);
        let eq_window = &app.equalizer;
        let presets = eq::PRESETS.len();
        let names = || eq_window.get_preset_names().iter().collect::<Vec<_>>();
        assert_eq!(names().len(), presets);
        assert_eq!(eq_window.get_preset_index(), 0, "Flat");

        eq_window.invoke_gain_changed(3, 2.5);
        eq_window.invoke_preamp_changed(-3.0);
        assert_eq!(eq_window.get_preset_index(), -1);
        assert_eq!(names().len(), presets, "Custom is not an entry");
        assert_eq!(app.main.get_equalizer_curve(), "Custom");
        assert!(!eq_window.get_saved_curve_shown());

        eq_window.invoke_save_curve();
        assert!(app.zone_dialog.window().is_visible());
        assert_eq!(app.zone_dialog.get_name(), "Curve 1");
        app.zone_dialog.invoke_confirmed("Desk".into());
        let state = app.snapshot();
        assert_eq!(state.equalizer_curves.len(), 1);
        assert_eq!(state.equalizer.preamp_db, -3.0, "a save leaves the preamp");
        assert_eq!(eq_window.get_preset_index(), presets as i32);
        assert_eq!(names()[presets], "Desk");
        slint::platform::update_timers_and_animations();
        assert_eq!(eq_window.get_picker_shown_index(), presets as i32);
        assert_eq!(app.main.get_equalizer_curve(), "Desk");
        assert!(eq_window.get_saved_curve_shown());

        eq_window.invoke_preset_selected(0);
        assert_eq!(eq_window.get_preset_index(), 0);
        eq_window.invoke_preset_selected(presets as i32);
        let state = app.snapshot();
        assert_eq!(state.equalizer.gains_db[3], 2.5);
        assert_eq!(state.equalizer.preamp_db, -3.0, "a curve leaves the preamp");
        assert!(state.equalizer.enabled, "picking a curve turns it on");

        eq_window.invoke_rename_curve();
        assert_eq!(app.zone_dialog.get_name(), "Desk");
        assert_eq!(app.zone_dialog.get_field_label(), "Curve name");
        app.zone_dialog.invoke_confirmed("Office".into());
        assert_eq!(app.snapshot().equalizer_curves[0].name, "Office");

        // The curve dialogs share the zone name dialog.
        app.ask_new_zone();
        assert_eq!(app.zone_dialog.get_field_label(), "Zone name");
        app.zone_dialog.invoke_cancelled();
        eq_window.invoke_save_curve();
        app.zone_dialog.invoke_cancelled();
        let speaker = app.snapshot().speakers[0].id.clone();
        app.ask_new_zone_with(&speaker);
        assert_eq!(app.zone_dialog.get_field_label(), "Zone name");
        app.zone_dialog.invoke_cancelled();

        eq_window.invoke_delete_curve();
        assert!(app.snapshot().equalizer_curves.is_empty());
        assert_eq!(eq_window.get_preset_index(), -1, "the gains stay");
        slint::platform::update_timers_and_animations();
        assert_eq!(
            eq_window.get_picker_shown_index(),
            -1,
            "a shorter list leaves the picker on Custom"
        );
        assert_eq!(names().len(), presets);

        eq_window.invoke_reset();
        let state = app.snapshot();
        assert_eq!(state.equalizer.preamp_db, 0.0);
        assert_eq!(eq_window.get_preset_index(), 0, "Reset reads Flat");
    }

    // covers: S270
    #[test]
    fn a_saved_curve_with_a_presets_gains_can_be_picked_renamed_and_deleted() {
        let app = app_over(MockBackend::new());
        let eq_window = &app.equalizer;
        let presets = eq::PRESETS.len() as i32;
        let rock = eq::PRESETS.iter().position(|p| p.id == "rock").unwrap() as i32;
        eq_window.invoke_preset_selected(rock);
        eq_window.invoke_save_curve();
        app.zone_dialog.invoke_confirmed("Mine".into());
        assert_eq!(
            eq_window.get_preset_index(),
            presets,
            "a save shows the curve"
        );
        eq_window.invoke_save_curve();
        app.zone_dialog.invoke_confirmed("Twin".into());
        assert_eq!(eq_window.get_preset_index(), presets + 1);

        eq_window.invoke_preset_selected(rock);
        assert_eq!(eq_window.get_preset_index(), rock);
        assert!(!eq_window.get_saved_curve_shown());
        eq_window.invoke_preset_selected(presets);
        assert_eq!(eq_window.get_preset_index(), presets);
        assert_eq!(app.main.get_equalizer_curve(), "Mine");
        assert!(eq_window.get_saved_curve_shown());
        eq_window.invoke_rename_curve();
        assert_eq!(app.zone_dialog.get_name(), "Mine");
        app.zone_dialog.invoke_confirmed("Ours".into());
        assert_eq!(
            app.main.get_equalizer_curve(),
            "Ours",
            "a rename keeps it shown"
        );

        eq_window.invoke_delete_curve();
        let names: Vec<_> = app
            .snapshot()
            .equalizer_curves
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["Twin"]);
        assert_eq!(eq_window.get_preset_index(), rock, "the gains stay");

        eq_window.invoke_preset_selected(presets);
        eq_window.invoke_reset();
        assert_eq!(eq_window.get_preset_index(), 0, "Reset reads Flat");
    }

    #[test]
    fn a_preset_recall_lets_go_of_the_picked_curve() {
        let app = app_over(MockBackend::new());
        app.update(|b| b.set_speaker_enabled("local:desk", true));
        app.save_preset("Flat");
        let eq_window = &app.equalizer;
        let rock = eq::PRESETS.iter().position(|p| p.id == "rock").unwrap() as i32;
        eq_window.invoke_preset_selected(rock);
        eq_window.invoke_save_curve();
        app.zone_dialog.invoke_confirmed("Mine".into());
        app.save_preset("Rock");
        let ids = preset_ids(&app);

        app.recall_preset(&ids[0]);
        app.recall_preset(&ids[1]);
        assert_eq!(eq_window.get_preset_index(), rock);
        assert!(!eq_window.get_saved_curve_shown());
    }

    #[test]
    fn a_curve_deleted_while_its_rename_is_open_renames_no_other_curve() {
        let app = app_over(MockBackend::new());
        let eq_window = &app.equalizer;
        let presets = eq::PRESETS.len();
        for (gain, name) in [(1.0, "A"), (2.0, "B"), (3.0, "C")] {
            eq_window.invoke_gain_changed(0, gain);
            eq_window.invoke_save_curve();
            app.zone_dialog.invoke_confirmed(name.into());
        }
        eq_window.invoke_preset_selected(presets as i32 + 1);
        assert_eq!(eq_window.get_preset_index(), presets as i32 + 1, "B");

        eq_window.invoke_rename_curve();
        eq_window.invoke_delete_curve();
        app.zone_dialog.invoke_confirmed("X".into());
        let names: Vec<String> = app
            .snapshot()
            .equalizer_curves
            .into_iter()
            .map(|curve| curve.name)
            .collect();
        assert_eq!(names, ["A", "C"]);
    }

    #[test]
    fn a_zone_name_longer_than_its_field_keeps_the_cursor_in_view() {
        let app = app_over(MockBackend::new());
        app.ask_new_zone();
        let window = app.zone_dialog.window();
        // Wider than the 1000 px the headless windows draw at.
        let name = "Living Room and Kitchen and Bedroom and Office Speakers, ".repeat(4);
        let name = name.trim_end();
        for character in name.chars() {
            press(window, character.to_string());
            assert!(
                app.zone_dialog.get_name_caret_in_view(),
                "the cursor is in view after {:?}",
                app.zone_dialog.get_name()
            );
        }
        assert_eq!(app.zone_dialog.get_name(), name);
        press(window, Key::Home);
        assert!(app.zone_dialog.get_name_caret_in_view(), "at the start");
        press(window, Key::End);
        assert!(app.zone_dialog.get_name_caret_in_view(), "at the end");
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
    fn a_stereo_pair_or_home_theater_row_names_its_members_or_the_missing_one() {
        use crate::audio::backend::AirPlayGroup;
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let mut row = MockBackend::new()
            .snapshot()
            .speakers
            .into_iter()
            .find(|speaker| speaker.id == "airplay:kitchen")
            .expect("the mock lists Kitchen");
        assert_eq!(group_note(&strings, &row), "", "a single speaker");
        let mut note = |kind, members: &[&str], missing: &[&str], incomplete| {
            row.group = Some(AirPlayGroup {
                kind,
                members: members.iter().map(|name| (*name).to_owned()).collect(),
                missing: missing.iter().map(|name| (*name).to_owned()).collect(),
                incomplete,
            });
            group_note(&strings, &row).to_string()
        };
        use AirPlayGroupKind::{HomeTheater, StereoPair};
        assert_eq!(
            note(StereoPair, &["Left", "Right"], &[], false),
            "Stereo pair: Left and Right"
        );
        assert_eq!(
            note(StereoPair, &["Left", "Right"], &["Right"], true),
            "Stereo pair, missing Right"
        );
        assert_eq!(
            note(StereoPair, &["Left"], &[], true),
            "Stereo pair, a speaker is missing",
            "a partner reported unreachable that was never seen"
        );
        assert_eq!(
            note(HomeTheater, &["Left", "Right", "Sub"], &[], false),
            "Home theater group: Left, Right and Sub"
        );
        assert_eq!(
            note(HomeTheater, &["Left", "Right"], &["Left", "Right"], true),
            "Home theater group, missing Left and Right"
        );
        assert_eq!(
            note(StereoPair, &[], &[], false),
            "Stereo pair",
            "no member names known yet"
        );
        assert_eq!(note(HomeTheater, &[], &[], false), "Home theater group");
        // Narrator reads the note before what the toggle does.
        assert_eq!(
            strings
                .invoke_with_group_note("Stereo pair: Left and Right".into(), "Turn on".into())
                .to_string(),
            "Stereo pair: Left and Right. Turn on"
        );
        assert_eq!(
            strings
                .invoke_with_group_note("".into(), "Turn on".into())
                .to_string(),
            "Turn on"
        );
        for (problem, speaker, says) in [
            (
                ui_model::AirPlayProblem::PlaysThroughSpeakers,
                "Den TV",
                "Den TV plays through its home theater speakers",
            ),
            (
                ui_model::AirPlayProblem::AppleTvPlays,
                "Den",
                "AudioSync already plays to Den's Apple TV",
            ),
            (
                ui_model::AirPlayProblem::PlaysInGroup,
                "Right",
                "Right plays as part of its stereo pair or home theater group",
            ),
            (
                ui_model::AirPlayProblem::ReceiverUnverified,
                "Den TV",
                "Den TV didn't answer as the speaker you paired, so it's off. If it's yours, use Forget Pairing and turn it on to pair again with its PIN.",
            ),
        ] {
            let text = notice_text(
                &strings,
                &NoticeMessage::AirPlayCantPlay {
                    speaker: speaker.into(),
                    problem,
                },
            );
            assert!(text.starts_with(says), "{text}");
            assert!(!text.contains('\u{2014}'), "no em dash: {text}");
        }
    }

    // covers: S242
    #[test]
    fn double_audio_from_all_apps_except_some_names_the_source_in_the_uis_words() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        let source = SourceName::AllAppsExcept(vec!["Spotify".into(), "Chrome".into()]);
        let text = notice_text(
            &strings,
            &NoticeMessage::HeardTwice {
                source: Some(source.clone()),
                mute: vec!["TV".into()],
                shared: Vec::new(),
                default_output: None,
            },
        );
        let named = strings.invoke_source_all_except("Spotify and Chrome".into());
        assert_eq!(named, "All apps except Spotify and Chrome");
        assert!(text.contains(named.as_str()), "{text}");
        let guide = Guide {
            source: Some(source.clone()),
            outputs: vec!["TV".into()],
            steps: Vec::new(),
        };
        assert!(
            guide_intro(&strings, &guide).contains(named.as_str()),
            "the guide names it the same way"
        );
        // The steps say what the user does per app, not the menu's name.
        let route = guide_step_text(
            &strings,
            &GuideStep::RouteApp {
                app: source.clone(),
            },
        );
        assert!(
            route
                .contains("find every app other than Spotify and Chrome and set each one's output"),
            "{route}"
        );
        let unmuted = guide_step_text(&strings, &GuideStep::KeepAppUnmuted { app: source });
        let expected = if cfg!(target_os = "linux") {
            "Don't mute or turn down any app other than Spotify and Chrome in a volume control"
        } else {
            "Don't mute any app other than Spotify and Chrome in the Volume mixer"
        };
        assert!(unmuted.starts_with(expected), "{unmuted}");
        let several = SourceName::Apps(vec!["Spotify".into(), "Chrome".into()]);
        let route = guide_step_text(&strings, &GuideStep::RouteApp { app: several });
        assert!(route.contains("find Spotify and Chrome and set"), "{route}");
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
                source: Some(SourceName::Named("Music Player".into())),
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
        assert_eq!(
            dismiss_label(NoticeMessage::AirPlayRetrying {
                speaker: "Kitchen".into(),
                problem: ui_model::AirPlayProblem::TooManyAirPlay2Speakers,
            }),
            "Dismiss: Kitchen waiting"
        );
        // Its notice is titled waiting too, as on the Mac.
        assert_eq!(
            dismiss_label(NoticeMessage::AirPlayRetrying {
                speaker: "Kitchen".into(),
                problem: ui_model::AirPlayProblem::PtpOtherNetwork,
            }),
            "Dismiss: Kitchen waiting"
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
            NoticeMessage::InputNotConnected { source: "A".into() },
            NoticeMessage::InputDisconnected { source: "A".into() },
            NoticeMessage::MicrophoneBlocked { source: "A".into() },
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
            NoticeMessage::PairFirst {
                speaker: "A".into(),
            },
            NoticeMessage::AirPlayCantPlay {
                speaker: "A".into(),
                problem: ui_model::AirPlayProblem::Refused,
            },
            NoticeMessage::AirPlayStopped,
            NoticeMessage::AirPlayPtpPortsTaken,
            NoticeMessage::SettingsNotSaved,
            NoticeMessage::PairingsNotSaved,
            NoticeMessage::SettingsKeptAside { file: "f".into() },
            NoticeMessage::PairingsKeptAside { file: "f".into() },
            NoticeMessage::NoSoundOutput,
            NoticeMessage::LaunchAtLoginFailed,
            NoticeMessage::LaunchAtLoginNotRemoved,
            NoticeMessage::LaunchAtLoginOffInSettings,
            NoticeMessage::AirPlay2NextLaunch {
                on: true,
                prefer: false,
            },
            NoticeMessage::AirPlay2NextLaunch {
                on: false,
                prefer: true,
            },
            NoticeMessage::RemoteOnPublicNetwork,
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
        tab(window, 3);
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
        // Switch, Several apps, source, master volume, Save as Preset…,
        // Equalizer, Calibrate All, then the rows: This PC's, then the
        // connected speakers' by name.
        tab(window, 17);
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
        tab(window, 25);
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
            app.ui.borrow().sync_open.contains("local:desk"),
            "and kept the controls open"
        );
        assert_eq!(
            app.main_focus.borrow().speaker,
            Some(("local:desk".into(), RowPart::Sync)),
            "the desk's field keeps the focus"
        );
    }

    #[test]
    fn the_several_apps_list_opens_from_the_keyboard_and_checks_apps_and_the_mode() {
        let app = app_over(MockBackend::new());
        app.main.show().unwrap();
        draw_frames();
        let window = app.main.window();
        assert!(!app.main.get_mix_open(), "folded until asked for");
        // The main switch, then Several apps.
        tab(window, 2);
        press(window, " ");
        draw_frames();
        assert!(app.main.get_mix_open());
        assert_eq!(app.main.get_mix_apps().row_count(), 2);

        app.main.invoke_mix_app_toggled(1);
        app.main.invoke_mix_app_toggled(0);
        let state = app.snapshot();
        assert!(state.mix_apps.iter().all(|a| a.checked));
        assert!(app.main.get_mix_apps().row_data(1).unwrap().checked);
        app.main.invoke_mix_mode_selected(true);
        assert!(app.snapshot().mix_all_except);
        assert!(app.main.get_mix_all_except());

        // Folded again by the user, it stays folded while the mix plays on.
        press(window, " ");
        draw_frames();
        assert!(!app.main.get_mix_open());
        app.refresh();
        assert!(!app.main.get_mix_open(), "a refresh never reopens it");
    }

    #[test]
    fn the_several_apps_list_opens_by_itself_once_as_the_source_becomes_a_mix() {
        let app = app_over(MockBackend::new());
        app.main.show().unwrap();
        draw_frames();
        assert!(!app.main.get_mix_open());
        app.main.invoke_mix_app_toggled(0);
        assert!(!app.main.get_mix_open(), "one app is no mix yet");
        app.main.invoke_mix_app_toggled(1);
        assert!(app.main.get_mix_open());
        app.main.set_mix_open(false);
        app.main.invoke_mix_app_toggled(1);
        app.main.invoke_mix_app_toggled(1);
        assert!(
            app.main.get_mix_open(),
            "back to a mix after one app opens it again"
        );
        app.main.set_mix_open(false);
        app.refresh();
        assert!(!app.main.get_mix_open());
    }

    #[test]
    fn a_focused_speaker_row_keeps_the_focus_when_its_list_is_rebuilt() {
        let app = main_window_with_listed_cable();
        let window = app.main.window();
        tab(window, 15);
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
    fn the_presets_previews_fill_the_card_and_the_notice() {
        use crate::ui_model::PresetsPreview;
        let app = app_over(MockBackend::presets_preview(PresetsPreview::List));
        assert_eq!(
            preset_rows(&app),
            [
                ("Evening".to_owned(), true),
                ("Movie Night".to_owned(), false),
                ("Whole House".to_owned(), false)
            ]
        );
        assert_eq!(app.main.get_notices().row_count(), 0);

        let app = app_over(MockBackend::presets_preview(PresetsPreview::LeftOff));
        let notice = app.main.get_notices().row_data(0).expect("a notice");
        assert!(
            notice.message.contains("Whole House")
                && notice.message.contains("Den")
                && notice.message.contains("Patio"),
            "{}",
            notice.message
        );

        let app = app_over(MockBackend::presets_preview(PresetsPreview::Empty));
        assert!(
            preset_rows(&app).is_empty(),
            "the card shows its empty line"
        );
    }

    fn preset_rows(app: &App) -> Vec<(String, bool)> {
        app.main
            .get_presets()
            .iter()
            .map(|row| (row.name.to_string(), row.current))
            .collect()
    }

    fn preset_ids(app: &App) -> Vec<String> {
        app.snapshot().presets.into_iter().map(|p| p.id).collect()
    }

    fn ctrl(window: &slint::Window, key: &str) {
        window.dispatch_event(WindowEvent::KeyPressed {
            text: Key::Control.into(),
        });
        press(window, key);
        window.dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
    }

    // covers: S259, S260, S263, S264
    #[test]
    fn the_presets_card_and_the_flyout_list_the_presets_and_mark_the_current_one() {
        let app = app_over(MockBackend::new());
        assert!(
            preset_rows(&app).is_empty(),
            "the card shows its empty line"
        );
        assert_eq!(app.flyout.get_presets().row_count(), 0, "no flyout row");

        app.save_preset("Evening");
        app.update(|b| b.set_master_volume(30.0));
        app.save_preset("  ");
        assert_eq!(
            preset_rows(&app),
            [("Evening".into(), false), ("Preset 2".into(), true)],
            "list order, a blank name numbered, what plays now marked"
        );
        assert_eq!(app.flyout.get_presets().row_count(), 2);
        assert_eq!(app.flyout.get_current_preset(), "Preset 2");

        app.update(|b| b.set_master_volume(31.0));
        assert!(preset_rows(&app).iter().all(|(_, current)| !current));
        assert_eq!(
            app.flyout.get_current_preset(),
            "",
            "the flyout says Choose a Preset"
        );
    }

    // covers: S261, S266
    #[test]
    fn ctrl_and_a_digit_recalls_the_preset_in_that_place_in_every_window() {
        let app = app_over(MockBackend::new());
        app.save_preset("Evening");
        app.update(|b| b.set_master_volume(30.0));
        app.save_preset("Party");
        app.update(|b| b.set_master_volume(55.0));

        app.equalizer.show().expect("the equalizer shows headless");
        draw_frames();
        ctrl(app.equalizer.window(), "1");
        assert_eq!(preset_rows(&app)[0], ("Evening".into(), true));
        assert!(!app.main.window().is_visible(), "a recall opens nothing");
        assert!(!app.zone_dialog.window().is_visible());

        ctrl(app.equalizer.window(), "2");
        assert_eq!(preset_rows(&app)[1], ("Party".into(), true));
        app.update(|b| b.set_master_volume(55.0));
        ctrl(app.equalizer.window(), "3");
        assert!(
            preset_rows(&app).iter().all(|(_, current)| !current),
            "an empty place does nothing"
        );
        assert_eq!(app.snapshot().master_volume, 55.0);

        app.help.invoke_preset_shortcut(1);
        assert_eq!(preset_rows(&app)[0], ("Evening".into(), true));
        app.remote_window.invoke_preset_shortcut(2);
        assert_eq!(preset_rows(&app)[1], ("Party".into(), true));
    }

    // covers: S260, S266
    #[test]
    fn a_recall_from_the_flyout_opens_nothing_and_leaves_the_flyout_as_it_is() {
        let app = app_over(MockBackend::new());
        app.save_preset("Evening");
        app.update(|b| b.set_master_volume(30.0));
        let id = preset_ids(&app)[0].clone();

        app.flyout.invoke_preset_recall_requested(id.into());
        assert_eq!(app.flyout.get_current_preset(), "Evening");
        assert!(!app.main.window().is_visible());
        assert!(!app.zone_dialog.window().is_visible());
        assert_eq!(app.flyout.get_presets().row_count(), 1);
    }

    // covers: S261, S262, S269
    #[test]
    fn save_rename_update_and_delete_ask_nothing_and_say_what_they_did() {
        let app = main_window_over(MockBackend::new());
        app.ask_save_preset();
        assert_eq!(*app.dialog_purpose.borrow(), DialogPurpose::SavePreset);
        assert_eq!(app.zone_dialog.get_heading(), "Save Preset");
        assert_eq!(app.zone_dialog.get_field_label(), "Preset name");
        assert_eq!(app.zone_dialog.get_placeholder(), "Evening, Party…");
        app.confirm_dialog("Evening");
        assert_eq!(preset_rows(&app), [("Evening".into(), true)]);
        assert_eq!(app.main.get_notice_announcement(), "Evening saved");

        let id = preset_ids(&app)[0].clone();
        app.ask_rename_preset(&id);
        assert_eq!(app.zone_dialog.get_heading(), "Rename Preset");
        assert_eq!(app.zone_dialog.get_name(), "Evening");
        app.confirm_dialog("   ");
        assert_eq!(preset_rows(&app)[0].0, "Evening", "a blank name keeps it");
        app.ask_rename_preset(&id);
        app.confirm_dialog("Late");
        assert_eq!(preset_rows(&app)[0].0, "Late");

        app.save_preset("Party");
        app.update(|b| b.set_master_volume(30.0));
        app.update_preset(&id);
        assert!(
            !app.zone_dialog.window().is_visible(),
            "Update asks nothing"
        );
        assert_eq!(preset_ids(&app)[0], id, "it keeps its ID and place");
        assert_eq!(preset_rows(&app)[0], ("Late".into(), true));
        assert_eq!(app.main.get_notice_announcement(), "Late updated");

        app.delete_preset(&id);
        assert!(
            !app.zone_dialog.window().is_visible(),
            "Delete asks nothing"
        );
        assert_eq!(preset_rows(&app), [("Party".into(), false)]);
        assert_eq!(app.main.get_notice_announcement(), "Late deleted");
    }

    // covers: S262
    #[test]
    fn save_as_preset_with_no_speaker_on_says_what_to_do_first() {
        let app = main_window_over(MockBackend::new());
        for speaker in app.snapshot().speakers {
            app.update(|b| b.set_speaker_enabled(&speaker.id, false));
        }
        app.ask_save_preset();
        assert_eq!(*app.dialog_purpose.borrow(), DialogPurpose::Message);
        assert_eq!(
            app.zone_dialog.get_message(),
            "Turn on the speakers you want in the preset first, then save it."
        );
        assert!(preset_rows(&app).is_empty());
    }

    // covers: S267
    #[test]
    fn after_a_delete_the_focus_goes_to_the_next_row_then_to_save_as_preset() {
        let app = main_window_over(MockBackend::new());
        app.save_preset("Evening");
        app.save_preset("Party");
        draw_frames();
        let ids = preset_ids(&app);
        let window = app.main.window();

        app.delete_preset(&ids[0]);
        draw_frames();
        assert_eq!(app.main.get_focus_preset_id(), "", "the row took it");
        press(window, Key::F2);
        assert_eq!(
            *app.dialog_purpose.borrow(),
            DialogPurpose::RenamePreset(ids[1].clone()),
            "F2 on the focused row, the next one, renames it"
        );
        app.zone_dialog.hide().unwrap();

        app.delete_preset(&ids[1]);
        draw_frames();
        press(window, " ");
        assert_eq!(
            *app.dialog_purpose.borrow(),
            DialogPurpose::SavePreset,
            "with no row left, Save as Preset… has the focus"
        );
    }

    // covers: S265
    #[test]
    fn a_recall_that_leaves_a_hidden_speaker_off_says_so_in_one_notice() {
        let app = app_over(MockBackend::new());
        app.update(|b| b.set_speaker_enabled("local:desk", true));
        app.save_preset("Evening");
        app.update(|b| b.hide_speaker("local:desk", "USB"));
        let id = preset_ids(&app)[0].clone();

        app.recall_preset(&id);
        let notices = app.snapshot().notices;
        assert_eq!(notices.len(), 1);
        let text = notice_text(&app.strings(), &notices[0].message);
        assert_eq!(
            text, "“Evening” is on. Stayed off: Desk Speaker (Hidden).",
            "a hidden speaker stays hidden"
        );
        assert_eq!(
            notice_title(&app.strings(), &notices[0].message),
            "some speakers stayed off"
        );

        app.update(|b| b.rename_preset(&id, "Late"));
        let notices = app.snapshot().notices;
        assert_eq!(notices.len(), 1, "renamed in place");
        assert!(notice_text(&app.strings(), &notices[0].message).starts_with("“Late” is on."));

        app.update(|b| b.unhide_speaker("local:desk"));
        app.recall_preset(&id);
        assert!(app.snapshot().notices.is_empty(), "a full recall clears it");
    }

    // covers: S265
    #[test]
    fn a_recall_with_no_speaker_left_changes_nothing_and_says_so() {
        let app = app_over(MockBackend::new());
        app.update(|b| b.set_speaker_enabled("local:desk", true));
        app.update(|b| b.set_speaker_enabled("airplay:kitchen", false));
        app.save_preset("Desk");
        app.update(|b| b.hide_speaker("local:desk", "USB"));
        app.update(|b| b.set_master_volume(40.0));

        app.recall_preset(&preset_ids(&app)[0]);
        let notices = app.snapshot().notices;
        assert_eq!(
            notice_text(&app.strings(), &notices[0].message),
            "Nothing changed: no speaker in “Desk” can play now. Desk Speaker (Hidden)."
        );
        assert_eq!(app.snapshot().master_volume, 40.0);
    }

    // covers: S269
    #[test]
    fn the_preset_texts_come_from_strings() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        assert_eq!(strings.get_presets(), "Presets");
        assert_eq!(strings.get_save_preset(), "Save as Preset…");
        assert_eq!(strings.get_no_presets(), "No Presets");
        assert_eq!(strings.get_choose_preset(), "Choose a Preset");
        assert_eq!(strings.get_preset_current(), "Current");
        assert_eq!(strings.get_preset_recall_hint(), "Recalls this preset");
        assert_eq!(strings.get_update_preset(), "Update to Current Settings");
        assert_eq!(strings.get_rename_preset(), "Rename…");
        assert_eq!(strings.get_delete_preset(), "Delete");
        assert_eq!(strings.invoke_numbered_preset(3), "Preset 3");
        assert_eq!(strings.get_preset_busy(), "In use by another app");
        assert_eq!(
            strings.get_presets_empty(),
            "No presets yet. Turn on the speakers you want, set the volumes and the curve, \
             then save them as a preset."
        );
    }

    #[test]
    fn a_zone_menu_says_rename_as_a_speaker_menu_does() {
        let app = app_over(MockBackend::new());
        let strings = app.strings();
        assert_eq!(strings.get_rename_zone(), "Rename…");
        assert_eq!(strings.get_rename_zone(), strings.get_rename_speaker());
    }

    // covers: S261
    #[test]
    fn ctrl_and_a_digit_recalls_from_the_guide_and_the_flyout_too() {
        let app = app_over(MockBackend::new());
        app.save_preset("Evening");
        app.update(|b| b.set_master_volume(30.0));
        app.save_preset("Party");
        app.update(|b| b.set_master_volume(55.0));

        app.guide.show().expect("the guide shows headless");
        app.guide.invoke_take_focus();
        draw_frames();
        ctrl(app.guide.window(), "1");
        assert_eq!(preset_rows(&app)[0], ("Evening".into(), true));

        app.flyout.show().expect("the flyout shows headless");
        draw_frames();
        app.flyout.invoke_take_focus();
        draw_frames();
        ctrl(app.flyout.window(), "2");
        assert_eq!(preset_rows(&app)[1], ("Party".into(), true));
    }

    // covers: S251
    #[test]
    fn save_as_preset_is_off_at_the_most_presets_the_app_keeps() {
        let mut backend = MockBackend::new();
        for n in 0..MAX_PRESETS {
            Backend::save_preset(&mut backend, &format!("P{n}")).expect("room");
        }
        let app = main_window_over(backend);
        assert!(!app.main.get_can_save_preset());
        app.ask_save_preset();
        assert_eq!(*app.dialog_purpose.borrow(), DialogPurpose::Message);
        assert_eq!(
            app.zone_dialog.get_message(),
            "AudioSync keeps 256 presets at most. Delete one to save another."
        );

        let first = preset_ids(&app)[0].clone();
        app.zone_dialog.hide().unwrap();
        app.delete_preset(&first);
        assert!(app.main.get_can_save_preset());
    }

    // covers: S266
    #[test]
    fn a_recall_leaves_the_keyboard_focus_on_the_row_or_the_notice_that_had_it() {
        let mut backend = MockBackend::new();
        backend.post("source", NoticeMessage::CaptureLost, "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        Backend::save_preset(&mut backend, "Evening").expect("saved");
        Backend::set_master_volume(&mut backend, 30.0);
        Backend::save_preset(&mut backend, "Party").expect("saved");
        let app = main_window_over(backend);
        let window = app.main.window();
        let ids = preset_ids(&app);

        // The newest notice's dismiss button, as with no preset.
        tab(window, 3);
        app.recall_preset(&ids[0]);
        press(window, " ");
        assert_eq!(app.snapshot().notices.len(), 1, "Space dismissed it");

        app.main.set_focus_preset_id(ids[1].clone().into());
        draw_frames();
        app.recall_preset(&ids[0]);
        press(window, Key::F2);
        assert_eq!(
            *app.dialog_purpose.borrow(),
            DialogPurpose::RenamePreset(ids[1].clone()),
            "Party's row kept the focus"
        );
    }

    // covers: S267
    #[test]
    fn the_menu_key_and_shift_f10_open_a_preset_rows_menu() {
        let app = main_window_over(MockBackend::new());
        app.save_preset("Evening");
        draw_frames();
        let id = preset_ids(&app)[0].clone();
        let window = app.main.window();

        // Slint maps Shift+F10 to the menu on Windows only.
        let opens: &[&[Key]] = if cfg!(windows) {
            &[&[Key::Menu], &[Key::Shift, Key::F10]]
        } else {
            &[&[Key::Menu]]
        };
        for &open in opens {
            app.main.set_focus_preset_id(id.clone().into());
            draw_frames();
            for key in open {
                window.dispatch_event(WindowEvent::KeyPressed {
                    text: (*key).into(),
                });
            }
            for key in open.iter().rev() {
                window.dispatch_event(WindowEvent::KeyReleased {
                    text: (*key).into(),
                });
            }
            draw_frames();
            press(window, Key::DownArrow);
            press(window, Key::Return);
            assert_eq!(
                *app.dialog_purpose.borrow(),
                DialogPurpose::RenamePreset(id.clone()),
                "{open:?}: the menu opened, and its first item is Rename…"
            );
            app.zone_dialog.hide().unwrap();
            *app.dialog_purpose.borrow_mut() = DialogPurpose::Message;
        }
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
        let zones = |ids: &[&str]| -> Vec<ZoneRows> {
            ids.iter()
                .map(|&id| ZoneRows {
                    id: id.into(),
                    ..ZoneRows::default()
                })
                .collect()
        };
        let lists = |n: &[i32], g: &[&str]| MainLists {
            notices: notices(n),
            zones: zones(g),
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

        let on_zone = |id: &str, part| MainFocus {
            zone: Some((id.into(), part)),
            ..MainFocus::default()
        };
        assert_eq!(
            on_zone("a", RowPart::Volume).target(&lists(&[], &["a", "b"]), &lists(&[], &["a"])),
            Some(MainFocusTarget::Zone("a".into(), RowPart::Volume)),
            "the same zone, on the same part"
        );
        assert_eq!(
            on_zone("b", RowPart::Toggle).target(&lists(&[], &["a", "b"]), &lists(&[], &["a"])),
            Some(MainFocusTarget::Zone("a".into(), RowPart::Options)),
            "a deleted zone: the options of the one before it"
        );
        assert_eq!(
            on_zone("a", RowPart::Toggle).target(&lists(&[], &["a"]), &lists(&[], &[])),
            Some(MainFocusTarget::StreamingSwitch),
            "the last zone went"
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
        let hiding = |id: &str| MainFocus {
            hiding: Some(id.into()),
            ..on(id, RowPart::Options)
        };
        assert_eq!(
            hiding("b").target(&before, &lists(&["pc"], &["a", "c"])),
            Some(MainFocusTarget::Speaker("c".into(), RowPart::Toggle)),
            "Hide Speaker: the next row's switch"
        );
        assert_eq!(
            hiding("c").target(&before, &lists(&["pc"], &["a", "b"])),
            Some(MainFocusTarget::Speaker("b".into(), RowPart::Toggle)),
            "Hide Speaker on the last row: the previous row's switch"
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
        let with_zone = |this_pc: &[&str], connected: &[&str], members: &[&str]| MainLists {
            zones: vec![ZoneRows {
                id: "g".into(),
                rows: speakers(members),
            }],
            ..lists(this_pc, connected)
        };
        assert_eq!(
            on("pc", RowPart::Volume)
                .target(&with_zone(&["pc"], &[], &[]), &with_zone(&[], &[], &[])),
            Some(MainFocusTarget::Zone("g".into(), RowPart::Options)),
            "no speaker row left: the first zone's options"
        );
        assert_eq!(
            on("b", RowPart::Options).target(
                &with_zone(&["pc"], &["a", "b"], &[]),
                &with_zone(&["pc"], &["a"], &["b"])
            ),
            Some(MainFocusTarget::Speaker("b".into(), RowPart::Options)),
            "moved into a zone, the row there takes the focus"
        );
        assert_eq!(
            on("b", RowPart::Toggle).target(
                &with_zone(&["pc"], &["a"], &["b"]),
                &with_zone(&["pc"], &["a"], &[])
            ),
            Some(MainFocusTarget::Zone("g".into(), RowPart::Options)),
            "a collapsed zone: its header"
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
        // As in the example's order without Bedroom's row:
        // the cable row's toggle.
        tab(window, 15);
        press(window, " ");
        assert!(enabled(&app, "local:cable"), "the cable row has the focus");
        press(window, " ");
        assert!(!enabled(&app, "local:cable"));

        // Off and no longer shown, Connected Speakers is empty: the focus
        // goes to This PC's first row's options. Three Tabs on: its volume,
        // its device volume fix (the mock's speaker device sits at 13 %),
        // then the headphones' toggle, not the source after the streaming
        // switch.
        app.main.invoke_show_virtual_devices_toggled(false);
        draw_frames();
        let headphones = enabled(&app, "local:headphones");
        tab(window, 3);
        press(window, " ");
        assert_ne!(
            enabled(&app, "local:headphones"),
            headphones,
            "after the speakers' volume and its fix, the headphones' toggle"
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
