//! What the interface shows and what it can ask for, independent of how the
//! audio work is done.
//!
//! The UI reads an [`AppState`] snapshot from a [`Backend`] and calls the
//! backend's methods for every user action, then reads a new snapshot. On
//! Windows the backend is `crate::adapter::AudioAdapter` over the WASAPI audio
//! backend; elsewhere [`MockBackend`] stands in with example data, for work on
//! the UI. Nothing here makes a sound or opens an audio device.

use audiosync_core::eq::{self, EqSettings};
#[cfg(any(not(windows), test))]
use audiosync_core::volume;

/// The largest offset the sync controls accept, in milliseconds (the macOS
/// app's limit too).
pub const MAX_DELAY_MS: f64 = 20_000.0;

/// The smallest offset the sync controls accept, in milliseconds: a speaker
/// that sounds up to two seconds late.
pub const MIN_DELAY_MS: f64 = -2000.0;

/// The most notices kept at once; the oldest drops off past it.
#[cfg(test)]
pub const NOTICE_CAPACITY: usize = 4;

/// Where a source's audio comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Everything the PC plays.
    System,
    /// One app's audio.
    App,
    /// An input device (line in, microphone).
    #[cfg_attr(
        all(windows, not(test)),
        expect(dead_code, reason = "the Windows backend offers no input devices yet")
    )]
    Input,
}

/// Something AudioSync can capture.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
}

/// The kind of output a speaker is, shown as a small icon and read by Narrator.
// The mock lists only a few kinds.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerKind {
    AirPlay,
    BuiltIn,
    Headphones,
    Bluetooth,
    Usb,
    Hdmi,
    DisplayPort,
    Virtual,
    Other,
}

impl SpeakerKind {
    /// The PC's own outputs, listed under "This PC" (like "This Mac").
    pub fn is_this_pc(self) -> bool {
        matches!(self, SpeakerKind::BuiltIn | SpeakerKind::Headphones)
    }
}

/// Whether an AirPlay speaker can simply play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Open,
    /// It asks for a PIN, which the user types once when switching it on.
    NeedsCode,
    /// A pairing with it runs: the PIN prompt is open or about to open.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend pairs")
    )]
    Pairing,
    /// It asks for a password, which AudioSync for Windows can't send yet.
    PasswordNotYet,
    /// It needs pairing with a PIN, which AudioSync for Windows can't do yet.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend lists real receivers")
    )]
    PairingNotYet,
    /// Only AirPlay 2 can play it, which AudioSync for Windows doesn't speak
    /// yet.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend lists real receivers")
    )]
    AirPlay2NotYet,
    /// It wants an encryption, codec or format AudioSync can't send.
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the Windows backend lists real receivers")
    )]
    Unsupported,
}

/// What went wrong with an AirPlay speaker, as a notice words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend posts notices")
)]
pub enum AirPlayProblem {
    /// It doesn't answer; AudioSync keeps trying.
    NotResponding,
    /// The connection was lost while it played; AudioSync keeps trying.
    ConnectionLost,
    /// Another device seems to play to it; AudioSync keeps trying.
    Busy,
    /// It turned the stream down; AudioSync tries again.
    Refused,
    /// It asks for a password, which AudioSync for Windows can't send yet.
    PasswordNotYet,
    /// An Apple TV that needs pairing with a PIN first; switching it on
    /// pairs it.
    NeedsPin,
    /// Pairing with an Apple TV failed, so it is off.
    PairingFailed,
    /// It needs pairing, which AudioSync for Windows can't do yet.
    PairingNotYet,
    /// Only AirPlay 2 can play it.
    AirPlay2NotYet,
    /// AudioSync can't play to it.
    Unsupported,
}

/// One output: a local device or an AirPlay speaker.
#[derive(Clone, Debug, PartialEq)]
pub struct Speaker {
    pub id: String,
    pub name: String,
    pub kind: SpeakerKind,
    /// Chosen by the user; it plays while streaming is on.
    pub enabled: bool,
    /// 0 to 100.
    pub volume: f64,
    /// The offset of a local output relative to the reference; the
    /// reference has none, and neither do AirPlay speakers (the AirPlay
    /// sender keeps them in sync, and local outputs follow it).
    pub delay_ms: Option<f64>,
    pub auto_connect: bool,
    pub availability: Availability,
    /// An Apple TV AudioSync keeps pairing credentials for, which the user
    /// can forget.
    pub paired: bool,
}

/// Where a pairing's PIN prompt is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend pairs")
)]
pub enum PinPhase {
    /// The Apple TV is asked to show a PIN.
    Starting,
    /// The Apple TV shows a PIN for the user to type.
    Ready,
    /// The PIN went to the Apple TV, which checks it.
    Checking,
}

/// The PIN prompt of a running pairing: a dialog asks for the PIN an Apple
/// TV shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinPrompt {
    /// The speaker's ID.
    pub speaker_id: String,
    /// The speaker's name.
    pub name: String,
    pub phase: PinPhase,
    /// The Apple TV turned the last PIN down and shows a new one.
    pub wrong_pin: bool,
}

impl Speaker {
    /// Whether the speaker lists show it. A virtual output (a loopback
    /// driver, a voice changer, a meeting app's device) is listed only while
    /// Show Virtual Devices is on or while it is turned on. Unlike the Mac,
    /// which also keeps one listed once it was shown, turning a listed
    /// virtual output off with Show Virtual Devices off removes its row.
    pub fn is_listed(&self, show_virtual: bool) -> bool {
        show_virtual || self.enabled || self.kind != SpeakerKind::Virtual
    }
}

/// A named set of speakers switched together.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
}

/// A problem worth telling the user about. A newer notice about the same
/// subject replaces the older one.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub id: u64,
    pub subject: String,
    pub message: NoticeMessage,
    /// Local time it was posted, in the user's short time format ("09.40",
    /// "9:40 AM").
    pub time: String,
}

/// What a notice says. The UI words it (the notice texts in
/// `ui/strings.slint`), so no backend builds user-facing text.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows backend posts notices")
)]
pub enum NoticeMessage {
    /// Streaming was asked to start with no speaker on.
    NoSpeaker,
    /// The app being captured quit, so streaming stopped.
    SourceQuit { source: String },
    /// The chosen app isn't running, so streaming didn't start.
    SourceNotRunning { source: String },
    /// Capturing the chosen app would capture AudioSync's own sound too, so
    /// streaming didn't start.
    SourceRefused { source: String },
    /// Capture couldn't start.
    CaptureFailed,
    /// Capture stopped working while streaming, so streaming stopped.
    CaptureLost,
    /// A speaker couldn't start, so it was turned off.
    SpeakerFailed { speaker: String },
    /// A speaker's device went away while it played. It stays on and plays
    /// again once the device is back.
    SpeakerLost { speaker: String },
    /// An AirPlay speaker stopped playing; it stays on while AudioSync keeps
    /// trying to reach it.
    AirPlayRetrying {
        speaker: String,
        problem: AirPlayProblem,
    },
    /// An AirPlay speaker can't play from AudioSync, so it is off.
    AirPlayCantPlay {
        speaker: String,
        problem: AirPlayProblem,
    },
    /// AudioSync's AirPlay sender stopped after an internal error, so
    /// AirPlay speakers aren't listed or playing. The notice offers Retry
    /// ([`Backend::retry_airplay`]).
    AirPlayStopped,
    /// The source also plays straight from these outputs, undelayed, so it
    /// is heard twice. `source` is the app's name, or `None` for the whole
    /// system. `mute` names the outputs AudioSync doesn't play to, which the
    /// user may mute; `shared` the ones AudioSync plays to as well, where
    /// only sending the source elsewhere helps. `default_output` is the one
    /// of them that counts because it is Windows' default output (the whole
    /// system only); any other plays the source because an app plays there.
    HeardTwice {
        source: Option<String>,
        mute: Vec<String>,
        shared: Vec<String>,
        default_output: Option<String>,
    },
}

/// How a speaker or group row shows: playing only while streaming is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerState {
    Off,
    /// Chosen, and plays once streaming starts.
    WillPlay,
    Playing,
}

impl SpeakerState {
    pub fn new(enabled: bool, streaming: bool) -> Self {
        match (enabled, streaming) {
            (false, _) => SpeakerState::Off,
            (true, false) => SpeakerState::WillPlay,
            (true, true) => SpeakerState::Playing,
        }
    }

    /// What a click on the row does: anything but playing turns on.
    pub fn click_turns_on(self) -> bool {
        self != SpeakerState::Playing
    }
}

/// Everything the windows show.
#[derive(Clone, Debug, PartialEq)]
pub struct AppState {
    /// Streaming is on.
    pub running: bool,
    pub sources: Vec<Source>,
    pub source_id: String,
    /// 0 to 100.
    pub master_volume: f64,
    pub speakers: Vec<Speaker>,
    pub groups: Vec<Group>,
    pub equalizer: EqSettings,
    /// Newest first.
    pub notices: Vec<Notice>,
    /// The PIN prompt to show, while a pairing asks for a PIN.
    pub pin_prompt: Option<PinPrompt>,
    /// The name of the reference output, which the other outputs' offsets
    /// are relative to; `None` without one (spec S174, S181).
    pub reference_name: Option<String>,
    /// The AirPlay delay field's value L in milliseconds, while an AirPlay
    /// sender runs (spec S178, S181).
    pub airplay_delay_ms: Option<f64>,
}

impl AppState {
    pub fn speaker(&self, id: &str) -> Option<&Speaker> {
        self.speakers.iter().find(|s| s.id == id)
    }

    pub fn speaker_state(&self, speaker: &Speaker) -> SpeakerState {
        SpeakerState::new(speaker.enabled, self.running)
    }

    /// The members of `group` that are present right now.
    pub fn available_members<'a>(&'a self, group: &'a Group) -> impl Iterator<Item = &'a Speaker> {
        group.members.iter().filter_map(|id| self.speaker(id))
    }

    /// A group is on when all its present members are.
    pub fn group_state(&self, group: &Group) -> SpeakerState {
        let mut present = self.available_members(group).peekable();
        let all_on = present.peek().is_some() && present.all(|s| s.enabled);
        SpeakerState::new(all_on, self.running)
    }

    /// Speakers switched on right now.
    pub fn active_speakers(&self) -> usize {
        self.speakers.iter().filter(|s| s.enabled).count()
    }

    /// The speaker lists both windows show: the PC's own outputs in their
    /// order, then every other speaker by name, case-insensitively.
    pub fn layout(&self) -> (Vec<&Speaker>, Vec<&Speaker>) {
        let this_pc = self
            .speakers
            .iter()
            .filter(|s| s.kind.is_this_pc())
            .collect();
        let mut connected: Vec<&Speaker> = self
            .speakers
            .iter()
            .filter(|s| !s.kind.is_this_pc())
            .collect();
        // A stable sort keeps the order of equal names.
        connected.sort_by_key(|s| s.name.to_lowercase());
        (this_pc, connected)
    }
}

/// Why a new group wasn't made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupError {
    /// No speaker is on, so the group would be empty.
    NoActiveSpeakers,
}

/// The audio side, as the UI sees it. Every method is called on the UI
/// thread, and the next snapshot shows what it did. Most calls return at
/// once, but on Windows a call that starts or stops streams waits for them:
/// starting or stopping streaming, picking another source while streaming,
/// turning a speaker or group on or off while streaming, and a poll that
/// brings a speaker back. Each stream takes a moment to open (longer for a
/// Bluetooth device, and starting capture waits up to 5 s for Windows), and
/// the windows don't respond meanwhile.
pub trait Backend {
    fn snapshot(&self) -> AppState;
    fn set_running(&mut self, on: bool);
    fn select_source(&mut self, id: &str);
    fn set_master_volume(&mut self, volume: f64);
    fn set_speaker_enabled(&mut self, id: &str, on: bool);
    fn set_speaker_volume(&mut self, id: &str, volume: f64);
    fn set_speaker_delay(&mut self, id: &str, ms: f64);
    /// The user entered `ms` in the AirPlay delay field.
    fn set_airplay_delay(&mut self, ms: f64);
    fn set_auto_connect(&mut self, id: &str, on: bool);
    /// Turns every present member on, or all of them off when the group is on.
    fn toggle_group(&mut self, id: &str);
    /// Makes a group of the speakers that are on now. The UI names it
    /// ("Group 2") when the user leaves the name empty.
    fn new_group(&mut self, name: &str) -> Result<(), GroupError>;
    fn rename_group(&mut self, id: &str, name: &str);
    fn delete_group(&mut self, id: &str);
    fn set_equalizer(&mut self, settings: EqSettings);
    fn dismiss_notice(&mut self, id: u64);
    fn dismiss_all_notices(&mut self);
    fn refresh_devices(&mut self);

    /// The PIN the user typed for the pairing with speaker `id`.
    fn submit_pin(&mut self, _id: &str, _pin: &str) {}

    /// The user cancelled the pairing with speaker `id`; it stays off.
    fn cancel_pairing(&mut self, _id: &str) {}

    /// Forgets the pairing with speaker `id`: it asks for a PIN again next
    /// time.
    fn forget_pairing(&mut self, _id: &str) {}

    /// Retry on the notice that AirPlay stopped: starts a new AirPlay
    /// sender, and the speakers that were on come back once it lists them.
    fn retry_airplay(&mut self) {}

    /// Takes in what happened since the last call (devices that came or went,
    /// a source that quit, an output that failed) and says whether the
    /// snapshot may have changed. The UI calls it about twice a second.
    fn poll(&mut self) -> bool {
        false
    }

    /// The windows' own settings as last saved: what the UI shows them with
    /// at launch.
    fn view_settings(&self) -> ViewSettings {
        ViewSettings::default()
    }

    /// The windows' own settings changed; they are saved with the rest.
    fn set_view_settings(&mut self, _settings: ViewSettings) {}

    /// Saves the settings now, without waiting for the next poll: the app
    /// is about to quit.
    fn save_settings(&mut self) {}
}

/// Where the main window was and how big: the position of its outer frame
/// in physical pixels, its inner size in logical pixels, both as it last
/// showed normally; and whether it was maximized (a file from before this
/// flag reads as not maximized).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowFrame {
    pub x: i32,
    pub y: i32,
    pub width: f64,
    pub height: f64,
    #[serde(default)]
    pub maximized: bool,
}

/// How the main window shows when its frame is remembered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MainWindowShows {
    /// Hidden or minimized: nothing to learn from it.
    Not,
    /// Shown normally, with this frame.
    Normal(WindowFrame),
    /// Maximized; the frame it covers now.
    Maximized(WindowFrame),
}

/// The main window frame to keep, given how the window `shows` and the frame
/// kept so far (`saved`): a normal window's own frame; for a maximized one,
/// the last normal frame marked maximized (or, with none yet, the maximized
/// frame itself); for a hidden or minimized one, what was kept.
pub fn frame_to_keep(shows: MainWindowShows, saved: Option<WindowFrame>) -> Option<WindowFrame> {
    match shows {
        MainWindowShows::Not => saved,
        MainWindowShows::Normal(frame) => Some(WindowFrame {
            maximized: false,
            ..frame
        }),
        MainWindowShows::Maximized(frame) => Some(WindowFrame {
            maximized: true,
            ..saved.unwrap_or(frame)
        }),
    }
}

/// What the windows themselves keep across restarts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewSettings {
    /// View > Show Virtual Devices.
    pub show_virtual_devices: bool,
    /// The main window's place and size, once it has been shown.
    pub window: Option<WindowFrame>,
}

/// Clamps a typed or stepped offset to what the sync controls accept, in
/// whole milliseconds.
pub fn clamp_delay_ms(ms: f64) -> f64 {
    if ms.is_nan() {
        0.0
    } else {
        ms.clamp(MIN_DELAY_MS, MAX_DELAY_MS).round()
    }
}

/// Parses what the user typed in a milliseconds field ("120", "+120",
/// "−250", "-250", "120,5"), rounded to whole milliseconds; `None` while it
/// isn't a number yet.
pub fn parse_ms(text: &str) -> Option<f64> {
    let value: f64 = text
        .trim()
        .replace(',', ".")
        .replace('\u{2212}', "-")
        .parse()
        .ok()?;
    value.is_finite().then(|| value.round())
}

/// Parses what the user typed in an offset field, clamped to what the sync
/// controls accept; `None` while it isn't a number yet.
pub fn parse_delay_ms(text: &str) -> Option<f64> {
    parse_ms(text).map(clamp_delay_ms)
}

/// Example data for UI work and tests: no device is opened, nothing plays.
#[cfg(any(not(windows), test))]
pub struct MockBackend {
    state: AppState,
    next_id: u64,
}

#[cfg(any(not(windows), test))]
impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(not(windows), test))]
impl MockBackend {
    pub fn new() -> Self {
        let source = |id: &str, name: &str, kind| Source {
            id: id.into(),
            name: name.into(),
            kind,
        };
        let speaker = |id: &str, name: &str, kind, enabled, delay_ms| Speaker {
            id: id.into(),
            name: name.into(),
            kind,
            enabled,
            volume: volume::DEFAULT_LOCAL_POSITION,
            delay_ms,
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
        };
        let mut speakers = vec![
            // The reference: no delay of its own.
            speaker(
                "local:speakers",
                "Speakers",
                SpeakerKind::BuiltIn,
                true,
                None,
            ),
            speaker(
                "local:headphones",
                "Headphones",
                SpeakerKind::Headphones,
                false,
                Some(0.0),
            ),
            speaker(
                "airplay:kitchen",
                "Kitchen",
                SpeakerKind::AirPlay,
                true,
                None,
            ),
            speaker(
                "airplay:living",
                "Living Room",
                SpeakerKind::AirPlay,
                false,
                None,
            ),
            speaker(
                "local:desk",
                "Desk Speaker",
                SpeakerKind::Usb,
                false,
                Some(120.0),
            ),
            speaker(
                "local:bt",
                "Headset",
                SpeakerKind::Bluetooth,
                false,
                Some(-250.0),
            ),
            speaker(
                "airplay:bedroom",
                "Bedroom",
                SpeakerKind::AirPlay,
                false,
                None,
            ),
        ];
        speakers[3].availability = Availability::NeedsCode;
        speakers[6].availability = Availability::PasswordNotYet;
        speakers[2].auto_connect = true;
        let state = AppState {
            running: false,
            sources: vec![
                source("system", "All System Audio", SourceKind::System),
                source("app:player", "Music Player", SourceKind::App),
                source("app:browser", "Web Browser", SourceKind::App),
                source("input:line", "Line In", SourceKind::Input),
            ],
            source_id: "system".into(),
            master_volume: volume::DEFAULT_MASTER_POSITION,
            speakers,
            groups: vec![Group {
                id: "group:1".into(),
                name: "Downstairs".into(),
                members: vec!["airplay:kitchen".into(), "airplay:living".into()],
            }],
            equalizer: EqSettings::default(),
            notices: Vec::new(),
            pin_prompt: None,
            reference_name: Some("Speakers".into()),
            airplay_delay_ms: Some(1750.0),
        };
        MockBackend { state, next_id: 1 }
    }

    /// Posts a notice about `subject`, replacing an older one about it.
    #[cfg(test)]
    pub fn post(&mut self, subject: &str, message: NoticeMessage, time: &str) {
        self.state.notices.retain(|n| n.subject != subject);
        self.state.notices.insert(
            0,
            Notice {
                id: self.next_id,
                subject: subject.into(),
                message,
                time: time.into(),
            },
        );
        self.next_id += 1;
        self.state.notices.truncate(NOTICE_CAPACITY);
    }

    fn speaker_mut(&mut self, id: &str) -> Option<&mut Speaker> {
        self.state.speakers.iter_mut().find(|s| s.id == id)
    }
}

#[cfg(any(not(windows), test))]
impl Backend for MockBackend {
    fn snapshot(&self) -> AppState {
        self.state.clone()
    }

    fn set_running(&mut self, on: bool) {
        self.state.running = on;
    }

    fn select_source(&mut self, id: &str) {
        if self.state.sources.iter().any(|s| s.id == id) {
            self.state.source_id = id.into();
        }
    }

    fn set_master_volume(&mut self, position: f64) {
        self.state.master_volume = volume::clamp_position(position);
    }

    fn set_speaker_enabled(&mut self, id: &str, on: bool) {
        if let Some(speaker) = self.speaker_mut(id) {
            speaker.enabled = on;
        }
        // Like the Mac: turning a speaker on starts streaming.
        if on {
            self.state.running = true;
        }
    }

    fn set_speaker_volume(&mut self, id: &str, position: f64) {
        if let Some(speaker) = self.speaker_mut(id) {
            speaker.volume = volume::clamp_position(position);
        }
    }

    fn set_speaker_delay(&mut self, id: &str, ms: f64) {
        if let Some(speaker) = self.speaker_mut(id)
            && speaker.delay_ms.is_some()
        {
            speaker.delay_ms = Some(clamp_delay_ms(ms));
        }
    }

    fn set_airplay_delay(&mut self, ms: f64) {
        if !ms.is_nan() && self.state.airplay_delay_ms.is_some() {
            self.state.airplay_delay_ms = Some(ms.clamp(0.0, MAX_DELAY_MS).round());
        }
    }

    fn set_auto_connect(&mut self, id: &str, on: bool) {
        if let Some(speaker) = self.speaker_mut(id) {
            speaker.auto_connect = on;
        }
    }

    fn toggle_group(&mut self, id: &str) {
        let Some(group) = self.state.groups.iter().find(|g| g.id == id).cloned() else {
            return;
        };
        let turn_on = self.state.group_state(&group).click_turns_on();
        for member in &group.members {
            if let Some(speaker) = self.speaker_mut(member) {
                speaker.enabled = turn_on;
            }
        }
        if turn_on {
            self.state.running = true;
        }
    }

    fn new_group(&mut self, name: &str) -> Result<(), GroupError> {
        let members: Vec<String> = self
            .state
            .speakers
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.id.clone())
            .collect();
        if members.is_empty() {
            return Err(GroupError::NoActiveSpeakers);
        }
        self.state.groups.push(Group {
            id: format!("group:{}", self.next_id),
            name: name.trim().to_string(),
            members,
        });
        self.next_id += 1;
        Ok(())
    }

    fn rename_group(&mut self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Some(group) = self.state.groups.iter_mut().find(|g| g.id == id) {
            group.name = name.into();
        }
    }

    fn delete_group(&mut self, id: &str) {
        self.state.groups.retain(|g| g.id != id);
    }

    fn set_equalizer(&mut self, settings: EqSettings) {
        self.state.equalizer = settings.quantized();
    }

    fn dismiss_notice(&mut self, id: u64) {
        self.state.notices.retain(|n| n.id != id);
    }

    fn dismiss_all_notices(&mut self) {
        self.state.notices.clear();
    }

    fn refresh_devices(&mut self) {}
}

/// The preset whose gains these settings have, if any (else the UI says "Custom").
pub fn matching_preset_index(settings: &EqSettings) -> Option<usize> {
    eq::PRESETS
        .iter()
        .position(|preset| preset.gains_db == settings.gains_db)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(x: i32, width: f64, maximized: bool) -> WindowFrame {
        WindowFrame {
            x,
            y: 40,
            width,
            height: 700.0,
            maximized,
        }
    }

    #[test]
    fn a_maximized_window_keeps_its_last_normal_frame_marked_maximized() {
        let normal = frame(100, 1000.0, false);
        let maximized = MainWindowShows::Maximized(frame(-8, 1920.0, false));
        assert_eq!(
            frame_to_keep(maximized, Some(normal)),
            Some(frame(100, 1000.0, true))
        );
        // Maximized before any normal frame was kept: its own frame.
        assert_eq!(
            frame_to_keep(maximized, None),
            Some(frame(-8, 1920.0, true))
        );
        // Back to normal, the flag goes.
        let shows = MainWindowShows::Normal(frame(120, 1100.0, false));
        assert_eq!(
            frame_to_keep(shows, Some(frame(100, 1000.0, true))),
            Some(frame(120, 1100.0, false))
        );
        // Hidden or minimized: what was kept stays.
        let kept = Some(frame(100, 1000.0, true));
        assert_eq!(frame_to_keep(MainWindowShows::Not, kept), kept);
    }

    #[test]
    fn a_speaker_plays_only_while_streaming() {
        assert_eq!(SpeakerState::new(false, true), SpeakerState::Off);
        assert_eq!(SpeakerState::new(true, false), SpeakerState::WillPlay);
        assert_eq!(SpeakerState::new(true, true), SpeakerState::Playing);
        assert!(SpeakerState::WillPlay.click_turns_on());
        assert!(!SpeakerState::Playing.click_turns_on());
    }

    #[test]
    fn this_pc_comes_first_then_the_rest_by_name() {
        let state = MockBackend::new().snapshot();
        let (this_pc, connected) = state.layout();
        let names = |list: &[&Speaker]| list.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&this_pc), ["Speakers", "Headphones"]);
        assert_eq!(
            names(&connected),
            [
                "Bedroom",
                "Desk Speaker",
                "Headset",
                "Kitchen",
                "Living Room"
            ]
        );
    }

    #[test]
    fn virtual_outputs_are_listed_only_when_shown_or_on() {
        let speaker = |kind, enabled| Speaker {
            id: "local:x".into(),
            name: "X".into(),
            kind,
            enabled,
            volume: 60.0,
            delay_ms: Some(0.0),
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
        };
        assert!(!speaker(SpeakerKind::Virtual, false).is_listed(false));
        assert!(speaker(SpeakerKind::Virtual, false).is_listed(true));
        assert!(speaker(SpeakerKind::Virtual, true).is_listed(false));
        for kind in [
            SpeakerKind::BuiltIn,
            SpeakerKind::Usb,
            SpeakerKind::Bluetooth,
        ] {
            assert!(speaker(kind, false).is_listed(false), "{kind:?}");
        }
    }

    #[test]
    fn turning_a_speaker_on_starts_streaming() {
        let mut backend = MockBackend::new();
        backend.set_speaker_enabled("local:desk", true);
        let state = backend.snapshot();
        assert!(state.running);
        assert_eq!(
            state.speaker_state(state.speaker("local:desk").unwrap()),
            SpeakerState::Playing
        );
    }

    #[test]
    fn a_group_is_on_when_all_its_present_members_are() {
        let mut backend = MockBackend::new();
        let group = backend.snapshot().groups[0].clone();
        assert_eq!(backend.snapshot().group_state(&group), SpeakerState::Off);
        backend.toggle_group(&group.id);
        let state = backend.snapshot();
        assert_eq!(state.group_state(&group), SpeakerState::Playing);
        backend.toggle_group(&group.id);
        let state = backend.snapshot();
        assert!(state.available_members(&group).all(|s| !s.enabled));
    }

    #[test]
    fn a_new_group_needs_a_speaker_that_is_on() {
        let mut backend = MockBackend::new();
        for id in ["local:speakers", "airplay:kitchen"] {
            backend.set_speaker_enabled(id, false);
        }
        assert_eq!(
            backend.new_group("Office"),
            Err(GroupError::NoActiveSpeakers)
        );
        backend.set_speaker_enabled("local:desk", true);
        assert_eq!(backend.new_group(" Office "), Ok(()));
        let group = backend.snapshot().groups.last().cloned().unwrap();
        assert_eq!(group.name, "Office");
        assert_eq!(group.members, ["local:desk"]);
    }

    #[test]
    fn delays_are_whole_milliseconds_within_the_limit() {
        assert_eq!(parse_delay_ms("120"), Some(120.0));
        assert_eq!(parse_delay_ms(" 99,6 "), Some(100.0));
        assert_eq!(parse_delay_ms("-5"), Some(-5.0));
        assert_eq!(parse_delay_ms("999999"), Some(MAX_DELAY_MS));
        assert_eq!(parse_delay_ms(""), None);
        assert_eq!(parse_delay_ms("12a"), None);
        assert_eq!(parse_delay_ms("inf"), None);
        let mut backend = MockBackend::new();
        backend.set_speaker_delay("airplay:kitchen", 50.0);
        let state = backend.snapshot();
        assert_eq!(state.speaker("airplay:kitchen").unwrap().delay_ms, None);
    }

    #[test]
    fn an_offset_takes_a_leading_plus_or_minus_and_other_text_is_ignored() {
        assert_eq!(parse_delay_ms("+150"), Some(150.0));
        assert_eq!(parse_delay_ms("−250"), Some(-250.0));
        assert_eq!(parse_delay_ms("-250"), Some(-250.0));
        assert_eq!(parse_delay_ms(" −99,6 "), Some(-100.0));
        assert_eq!(parse_delay_ms("−3000"), Some(-2000.0), "clamped");
        assert_eq!(parse_delay_ms("+"), None);
        assert_eq!(parse_delay_ms("−"), None);
        assert_eq!(parse_delay_ms("+-5"), None);
        assert_eq!(parse_delay_ms("1−2"), None);
        assert_eq!(clamp_delay_ms(-2400.0), -2000.0);
        assert_eq!(clamp_delay_ms(25_000.0), 20_000.0);
    }

    #[test]
    fn a_newer_notice_about_the_same_subject_replaces_the_older_one() {
        let quit = |source: &str| NoticeMessage::SourceQuit {
            source: source.into(),
        };
        let mut backend = MockBackend::new();
        backend.post("source", quit("first"), "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        backend.post("source", quit("second"), "09.42");
        let messages: Vec<_> = backend
            .snapshot()
            .notices
            .iter()
            .map(|n| n.message.clone())
            .collect();
        assert_eq!(messages, [quit("second"), NoticeMessage::NoSpeaker]);
        for n in 0..10 {
            backend.post(&format!("s{n}"), NoticeMessage::CaptureLost, "09.43");
        }
        assert_eq!(backend.snapshot().notices.len(), NOTICE_CAPACITY);
    }

    #[test]
    fn the_equalizer_is_quantized_and_matches_presets() {
        let mut backend = MockBackend::new();
        let mut settings = EqSettings::default();
        settings.gains_db[0] = 3.3;
        backend.set_equalizer(settings);
        assert_eq!(backend.snapshot().equalizer.gains_db[0], 3.5);
        assert_eq!(matching_preset_index(&EqSettings::default()), Some(0));
        assert_eq!(matching_preset_index(&backend.snapshot().equalizer), None);
    }
}
