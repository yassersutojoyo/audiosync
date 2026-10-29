//! The web remote, as the app sees it: the setting it keeps, the snapshot
//! the page shows, and what a command from the page does.
//!
//! The server (`audiosync_remote`) never reads the app's state. The windows
//! publish [`snapshot`] after every change, and each [`Command`] the page
//! sends becomes the [`Action`]s the window's own controls would take,
//! checked against the state the app holds when it applies them
//! ([`actions`]).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use audiosync_remote::{
    Command, DEFAULT_PORT, NowPlaying, Preset as RemotePreset, Source as RemoteSource,
    SourceKind as RemoteSourceKind, Speaker as RemoteSpeaker, State, Zone as RemoteZone,
    ZoneCounts,
};
use serde::{Deserialize, Serialize};

use crate::now_playing_card::{CardTrack, app_name};
use crate::ui_model::{
    AppState, Availability, MAX_DELAY_MS, MIN_DELAY_MS, SourceKind, Speaker, SpeakerKind, Waiting,
};

/// The lowest port the setting takes: below it are the well-known services.
pub const MIN_PORT: u16 = 1024;

/// File > Web Remote, as the settings file keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    #[serde(deserialize_with = "crate::settings::bool_or_false")]
    pub on: bool,
    #[serde(deserialize_with = "port_or_default")]
    pub port: u16,
}

/// A stored port that isn't a whole number from 0 to 65535 is the default,
/// so it costs only itself (S212).
fn port_or_default<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u16, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or(DEFAULT_PORT))
}

impl Default for RemoteSettings {
    fn default() -> RemoteSettings {
        RemoteSettings {
            on: false,
            port: DEFAULT_PORT,
        }
    }
}

impl RemoteSettings {
    /// The settings with a port the setting could have chosen.
    pub fn clamped(self) -> RemoteSettings {
        RemoteSettings {
            port: if self.port >= MIN_PORT {
                self.port
            } else {
                DEFAULT_PORT
            },
            ..self
        }
    }
}

/// A port typed in the window, when the setting takes it.
pub fn parse_port(text: &str) -> Option<u16> {
    text.trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port >= MIN_PORT)
}

/// Set to `1` with the debug log on, starts the web remote for this run
/// without touching its setting, so a browser check needs no click in the window.
pub const DEBUG_SWITCH_VAR: &str = "AUDIOSYNC_WEB_REMOTE";
/// The port for [`DEBUG_SWITCH_VAR`]'s run.
pub const DEBUG_PORT_VAR: &str = "AUDIOSYNC_WEB_REMOTE_PORT";

/// The port the debug switch starts the remote on: [`DEBUG_PORT_VAR`] when
/// the setting would take it, the saved port otherwise. None unless the
/// debug log is on and the switch is exactly `1`.
pub fn debug_switch_port(
    debug_log: bool,
    switch: Option<&std::ffi::OsStr>,
    port: Option<&std::ffi::OsStr>,
    saved: RemoteSettings,
) -> Option<u16> {
    if !debug_log || switch.and_then(std::ffi::OsStr::to_str) != Some("1") {
        return None;
    }
    Some(
        port.and_then(std::ffi::OsStr::to_str)
            .and_then(parse_port)
            .unwrap_or(saved.clamped().port),
    )
}

/// Whether Use Port takes `text`: a valid port other than `current`, or
/// `current` again after listening on it failed.
pub fn use_port_enabled(text: &str, current: u16, failed_on_port: bool) -> bool {
    parse_port(text).is_some_and(|port| port != current || failed_on_port)
}

/// The words a speaker row shows, from the interface's strings.
pub trait RowText {
    /// The speaker's kind ("AirPlay 2", "Bluetooth").
    fn kind(&self, speaker: &Speaker) -> String;
    /// The note under the row's name, as the window shows it; empty for none.
    fn note(&self, state: &AppState, speaker: &Speaker) -> String;
    /// The note of a speaker that asks for a code, which the page can't turn on.
    fn turn_on_at_pc(&self) -> String;
    /// The whole system's name in the source picker; the backend's is English.
    fn source_system(&self) -> String;
    /// The name of a source that takes every app except `names`.
    fn source_all_except(&self, names: &str) -> String;
}

/// A speaker that would ask for a PIN or password when turned on: the prompt
/// would open on the PC with nobody there, so the page can't turn it on.
fn asks_for_code(state: &AppState, speaker: &Speaker) -> bool {
    speaker.availability == Availability::NeedsCode
        && !speaker.paired
        && !speaker.holds_password
        && state.speaker_state(speaker).click_turns_on()
}

/// Whether the page may drive `speaker`: one that needs a kind of pairing
/// or a protocol AudioSync lacks, is pairing now, or would ask for a code
/// is shown without controls.
pub fn can_drive(state: &AppState, speaker: &Speaker) -> bool {
    match speaker.availability {
        Availability::Open => true,
        Availability::NeedsCode => !asks_for_code(state, speaker),
        _ => false,
    }
}

fn is_airplay(speaker: &Speaker) -> bool {
    matches!(speaker.kind, SpeakerKind::AirPlay | SpeakerKind::AirPlay2)
}

/// The delay `speaker`'s field shows and the range it takes, in whole
/// milliseconds; `None` without a field (the reference output, an AirPlay
/// speaker while no AirPlay sender runs).
pub fn delay_field(state: &AppState, speaker: &Speaker) -> Option<(f64, i64, i64)> {
    if is_airplay(speaker) {
        // Every AirPlay speaker shares the window's AirPlay delay (spec S178).
        return state
            .airplay_delay_ms
            .map(|ms| (ms, 0, MAX_DELAY_MS as i64));
    }
    let ms = speaker.delay_ms?;
    // A Cast speaker's offset only holds the others back (spec C77).
    let min = if speaker.cast.is_some() {
        0
    } else {
        MIN_DELAY_MS as i64
    };
    Some((ms, min, MAX_DELAY_MS as i64))
}

fn volume(position: f64) -> f64 {
    if position.is_finite() {
        position.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// The speakers the page lists, in the main window's order: the PC's own
/// outputs, then the others by name, each one the window lists (`listed`).
pub fn listed(state: &AppState, listed: impl Fn(&Speaker) -> bool) -> Vec<&Speaker> {
    let (this_pc, connected) = state.layout();
    this_pc
        .into_iter()
        .chain(connected)
        .filter(|speaker| listed(speaker))
        .collect()
}

/// What the page shows for `state`: the speakers the window lists, the
/// master volume, the track on the Now Playing card, the source picker's
/// entries in its order, and the computer's name.
pub fn snapshot(
    state: &AppState,
    shown: impl Fn(&Speaker) -> bool,
    track: Option<&CardTrack>,
    computer_name: &str,
    text: &dyn RowText,
) -> State {
    let listed_now = listed(state, shown);
    let speakers = listed_now
        .iter()
        .map(|speaker| {
            let (ms, min, max) = delay_field(state, speaker).unwrap_or((0.0, 0, 0));
            let note = if asks_for_code(state, speaker) {
                text.turn_on_at_pc()
            } else {
                text.note(state, speaker)
            };
            RemoteSpeaker {
                id: speaker.id.clone(),
                name: speaker.name.clone(),
                kind: text.kind(speaker),
                // Like the row's button: "Will play" still turns on.
                on: !state.speaker_state(speaker).click_turns_on(),
                volume: volume(speaker.volume),
                volume_step: speaker.cast.as_ref().map_or(1.0, |cast| cast.volume_step),
                delay_ms: if ms.is_finite() { ms.round() as i64 } else { 0 }.clamp(min, max),
                delay_min_ms: min,
                delay_max_ms: max,
                available: can_drive(state, speaker),
                notice: (!note.is_empty()).then_some(note),
            }
        })
        .collect();
    let sources = state
        .sources
        .iter()
        .map(|source| RemoteSource {
            id: source.id.clone(),
            name: match source.kind {
                SourceKind::System => text.source_system(),
                SourceKind::AppsExcept => text.source_all_except(&source.name),
                SourceKind::App | SourceKind::Input => source.name.clone(),
            },
            kind: match source.kind {
                SourceKind::System => RemoteSourceKind::System,
                SourceKind::App | SourceKind::AppsExcept => RemoteSourceKind::App,
                SourceKind::Input => RemoteSourceKind::Input,
            },
            // The window's picker refuses none of its entries.
            available: true,
            notice: None,
        })
        .collect();
    State {
        computer_name: computer_name.to_owned(),
        master_volume: volume(state.master_volume),
        now_playing: track.map(|track| NowPlaying {
            title: track.title.clone(),
            artist: track.artist.clone(),
            source: app_name(&track.app_id),
            playing: track.playing,
        }),
        speakers,
        zones: zones(state, &listed_now),
        presets: state
            .presets
            .iter()
            .map(|preset| RemotePreset {
                id: preset.id.clone(),
                name: preset.name.clone(),
                current: preset.current,
            })
            .collect(),
        calibrating: calibrating(state),
        sources,
        source_id: state
            .sources
            .iter()
            .any(|source| source.id == state.source_id)
            .then(|| state.source_id.clone()),
    }
}

/// A microphone run holds the outputs: a one-speaker calibration, Calibrate
/// All or Measure Again, or the cut-off sweep of one playing out.
pub fn calibrating(state: &AppState) -> bool {
    state.calibration.busy
}

/// The zones as the page shows them: each one's members that the page lists
/// now, in zone order, and how many of them are on (a waiting one included).
fn zones(state: &AppState, listed_now: &[&Speaker]) -> Vec<RemoteZone> {
    state
        .zones
        .iter()
        .map(|zone| {
            let members: Vec<&Speaker> = zone
                .members
                .iter()
                .filter_map(|key| listed_now.iter().find(|speaker| &speaker.id == key))
                .copied()
                .collect();
            let count = |test: &dyn Fn(&Speaker) -> bool| {
                u32::try_from(members.iter().filter(|speaker| test(speaker)).count())
                    .unwrap_or(u32::MAX)
            };
            // As the speaker's own switch shows it.
            let on = |speaker: &Speaker| !state.speaker_state(speaker).click_turns_on();
            RemoteZone {
                id: zone.id.clone(),
                name: zone.name.clone(),
                volume: volume(zone.volume),
                members: members.iter().map(|speaker| speaker.id.clone()).collect(),
                counts: ZoneCounts {
                    on: count(&on),
                    total: count(&|_| true),
                    all: u32::try_from(state.zone_counts(zone).total).unwrap_or(u32::MAX),
                    waiting: count(&|speaker| {
                        on(speaker)
                            && matches!(
                                speaker.waiting,
                                Some(Waiting::ForAirPlay2Slot | Waiting::ForPtp)
                            )
                    }),
                },
            }
        })
        .collect()
}

/// One change a command makes, as a control in the window would.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Enable {
        id: String,
        on: bool,
    },
    Volume {
        id: String,
        volume: f64,
    },
    Delay {
        id: String,
        ms: f64,
    },
    /// The AirPlay delay every AirPlay speaker shares.
    AirPlayDelay {
        ms: f64,
    },
    Master {
        volume: f64,
    },
    /// A click on the zone, as the core's click plan carries it out;
    /// `keep_off` are the members that would ask for a code.
    ToggleZone {
        id: String,
        keep_off: Vec<String>,
    },
    ZoneVolume {
        id: String,
        volume: f64,
    },
    RecallPreset {
        id: String,
        keep_off: Vec<String>,
    },
    /// A pick in the source picker.
    Source {
        id: String,
    },
}

/// Why a command changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No listed speaker has that ID (any more).
    UnknownSpeaker,
    /// The source picker doesn't list that ID (any more).
    UnknownSource,
    /// The speaker can't be driven now, or not that way.
    Unavailable,
    /// A volume or delay outside what the control takes.
    OutOfRange,
    /// No zone or preset has that ID (any more).
    Unknown,
    /// A microphone run holds the outputs; a command can arrive before the
    /// page's snapshot says so.
    Calibrating,
}

/// The actions `command` takes on `state`, among the speakers the window
/// lists (`shown`); none when a part of it can't be applied. A switch already
/// in the asked state takes none, so a repeat doesn't restart a stream; a
/// volume or delay acts even when unchanged.
pub fn actions(
    command: &Command,
    state: &AppState,
    shown: impl Fn(&Speaker) -> bool,
) -> Result<Vec<Action>, Refusal> {
    let in_range = |value: f64| value.is_finite() && (0.0..=100.0).contains(&value);
    if calibrating(state) {
        return Err(Refusal::Calibrating);
    }
    match command {
        Command::Zone { id, change } => {
            let zone = state
                .zones
                .iter()
                .find(|zone| &zone.id == id)
                .ok_or(Refusal::Unknown)?;
            let mut actions = Vec::new();
            if let Some(volume) = change.volume {
                if !in_range(volume) {
                    return Err(Refusal::OutOfRange);
                }
                actions.push(Action::ZoneVolume {
                    id: zone.id.clone(),
                    volume,
                });
            }
            if change.toggle {
                let can_play = zone.members.iter().any(|key| {
                    state
                        .speaker(key)
                        .is_some_and(|speaker| shown(speaker) && can_drive(state, speaker))
                });
                if !can_play {
                    return Err(Refusal::Unavailable);
                }
                actions.push(Action::ToggleZone {
                    id: zone.id.clone(),
                    keep_off: keep_off(state, &zone.members),
                });
            }
            Ok(actions)
        }
        Command::Preset { id } => {
            let preset = state
                .presets
                .iter()
                .find(|preset| &preset.id == id)
                .ok_or(Refusal::Unknown)?;
            Ok(vec![Action::RecallPreset {
                id: id.clone(),
                keep_off: keep_off(state, &preset.members),
            }])
        }
        Command::Master { volume } => {
            if !in_range(*volume) {
                return Err(Refusal::OutOfRange);
            }
            Ok(vec![Action::Master { volume: *volume }])
        }
        Command::Source { id } => {
            if !state.sources.iter().any(|source| &source.id == id) {
                return Err(Refusal::UnknownSource);
            }
            Ok(if *id == state.source_id {
                Vec::new()
            } else {
                vec![Action::Source { id: id.clone() }]
            })
        }
        Command::Speaker { id, change } => {
            let speaker = state
                .speaker(id)
                .filter(|speaker| shown(speaker))
                .ok_or(Refusal::UnknownSpeaker)?;
            if !can_drive(state, speaker) {
                return Err(Refusal::Unavailable);
            }
            let mut actions = Vec::new();
            if let Some(volume) = change.volume {
                if !in_range(volume) {
                    return Err(Refusal::OutOfRange);
                }
                if speaker.cast.as_ref().is_some_and(|cast| cast.volume_fixed) {
                    return Err(Refusal::Unavailable);
                }
                actions.push(Action::Volume {
                    id: id.clone(),
                    volume,
                });
            }
            if let Some(ms) = change.delay_ms {
                match delay_field(state, speaker) {
                    Some((_, min, max)) if (min..=max).contains(&ms) => {
                        actions.push(if is_airplay(speaker) {
                            Action::AirPlayDelay { ms: ms as f64 }
                        } else {
                            Action::Delay {
                                id: id.clone(),
                                ms: ms as f64,
                            }
                        });
                    }
                    None if ms == 0 => {}
                    _ => return Err(Refusal::OutOfRange),
                }
            }
            if let Some(on) = change.on {
                let is_on = !state.speaker_state(speaker).click_turns_on();
                if on != is_on {
                    actions.push(Action::Enable { id: id.clone(), on });
                }
            }
            Ok(actions)
        }
    }
}

/// The members a zone click or recall from the page leaves off: switched on,
/// they would ask for a code on the PC with nobody there.
fn keep_off(state: &AppState, members: &[String]) -> Vec<String> {
    members
        .iter()
        .filter(|key| {
            state
                .speaker(key)
                .is_some_and(|speaker| asks_for_code(state, speaker))
        })
        .cloned()
        .collect()
}

/// The computer's name as a `.local` host: Windows' DNS host name, in lower
/// case; `None` when it isn't a single DNS label.
pub fn local_host(name: &str) -> Option<String> {
    let name = name.trim();
    let usable = !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    usable.then(|| name.to_ascii_lowercase())
}

/// The page's address by name.
pub fn local_url(host: &str, port: u16) -> String {
    format!("http://{host}.local:{port}")
}

/// The page's address by IP literal.
pub fn ip_url(address: Ipv4Addr, port: u16) -> String {
    format!("http://{address}:{port}")
}

/// The code with its digits apart, so Narrator reads them one by one.
pub fn spaced_digits(code: &str) -> String {
    code.chars().map(String::from).collect::<Vec<_>>().join(" ")
}

/// This PC's IPv4 address on the home network: the one its default route
/// leaves from, when it is private. Nothing is sent.
pub fn lan_address() -> Option<Ipv4Addr> {
    // The wildcard bind could make Windows Firewall ask about the test program.
    #[cfg(test)]
    if audiosync_airplay::ptp::loopback_only() {
        return None;
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // A documentation address (RFC 5737): connecting only picks the route.
    socket
        .connect(SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), 9)))
        .ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(address)
            if !address.is_loopback()
                && audiosync_remote::net::is_private_peer(IpAddr::V4(address)) =>
        {
            Some(address)
        }
        _ => None,
    }
}

/// The QR code of `text`: its side in modules and each module, row by row,
/// `true` for dark; `None` when the text is too long for a QR code.
pub fn qr_modules(text: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M).ok()?;
    let modules = code
        .to_colors()
        .into_iter()
        .map(|color| color == qrcode::Color::Dark)
        .collect();
    Some((code.width(), modules))
}

/// The QR code of `text` as RGB pixels, `scale` pixels a module, with the
/// four-module quiet zone scanners need: its side in pixels and the pixels.
pub fn qr_pixels(text: &str, scale: usize) -> Option<(usize, Vec<[u8; 3]>)> {
    const QUIET: usize = 4;
    let (width, modules) = qr_modules(text)?;
    let side = (width + 2 * QUIET) * scale;
    let mut pixels = vec![[255u8; 3]; side * side];
    for (index, _) in modules.iter().enumerate().filter(|(_, dark)| **dark) {
        let (row, column) = (index / width + QUIET, index % width + QUIET);
        for y in row * scale..(row + 1) * scale {
            pixels[y * side + column * scale..y * side + (column + 1) * scale].fill([0; 3]);
        }
    }
    Some((side, pixels))
}

pub use crate::os::computer_name;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::PresetItem;
    use crate::ui_model::{CastRow, CastState};
    use audiosync_core::policy::zones::Zone;
    use audiosync_remote::{SpeakerChange, ZoneChange};

    // covers: S288
    #[test]
    fn the_debug_switch_starts_the_remote_only_with_the_debug_log() {
        use std::ffi::OsStr;

        let saved = RemoteSettings {
            on: false,
            port: 8080,
        };
        let one = Some(OsStr::new("1"));
        let port = |text: &'static str| Some(OsStr::new(text));
        assert_eq!(debug_switch_port(true, one, None, saved), Some(8080));
        assert_eq!(
            debug_switch_port(true, one, port("9000"), saved),
            Some(9000)
        );
        assert_eq!(
            debug_switch_port(true, one, port("80"), saved),
            Some(8080),
            "a port the setting wouldn't take falls back to the saved one"
        );
        assert_eq!(
            debug_switch_port(true, one, None, RemoteSettings { on: false, port: 0 }),
            Some(DEFAULT_PORT)
        );
        assert_eq!(debug_switch_port(false, one, port("9000"), saved), None);
        assert_eq!(debug_switch_port(true, None, port("9000"), saved), None);
        assert_eq!(debug_switch_port(true, port("true"), None, saved), None);
    }

    struct Text;

    impl RowText for Text {
        fn kind(&self, speaker: &Speaker) -> String {
            format!("{:?}", speaker.kind)
        }
        fn note(&self, _state: &AppState, speaker: &Speaker) -> String {
            match speaker.availability {
                Availability::AirPlay2NotYet => "AirPlay 2 not supported yet".to_owned(),
                Availability::NeedsCode => "Asks for a code".to_owned(),
                _ => String::new(),
            }
        }
        fn turn_on_at_pc(&self) -> String {
            "Turn it on at the PC".to_owned()
        }
        fn source_system(&self) -> String {
            "Alle Systemaudio".to_owned()
        }
        fn source_all_except(&self, names: &str) -> String {
            format!("All apps except {names}")
        }
    }

    fn with_sources() -> AppState {
        let source = |id: &str, name: &str, kind| crate::ui_model::Source {
            id: id.to_owned(),
            name: name.to_owned(),
            kind,
            channels: 0,
        };
        AppState {
            sources: vec![
                source("system", "All System Audio", SourceKind::System),
                source("app:42:Spotify.exe", "Spotify", SourceKind::App),
                source("input:{mic}", "USB Mic", SourceKind::Input),
            ],
            source_id: "app:42:Spotify.exe".to_owned(),
            ..state()
        }
    }

    // covers: S344
    #[test]
    fn the_snapshot_lists_the_pickers_sources_in_its_order_with_the_current_one() {
        let snapshot = snapshot(&with_sources(), not_virtual, None, "Studio PC", &Text);
        let listed: Vec<(&str, &str, RemoteSourceKind)> = snapshot
            .sources
            .iter()
            .map(|source| (source.id.as_str(), source.name.as_str(), source.kind))
            .collect();
        assert_eq!(
            listed,
            [
                ("system", "Alle Systemaudio", RemoteSourceKind::System),
                ("app:42:Spotify.exe", "Spotify", RemoteSourceKind::App),
                ("input:{mic}", "USB Mic", RemoteSourceKind::Input),
            ]
        );
        assert!(snapshot.sources.iter().all(|source| source.available));
        assert_eq!(snapshot.source_id.as_deref(), Some("app:42:Spotify.exe"));
        // What the server serves: the JSON round trip keeps the current source.
        let served = State::from_json(&snapshot.to_json(1)).unwrap();
        assert_eq!(served.source_id.as_deref(), Some("app:42:Spotify.exe"));
    }

    // covers: S344
    #[test]
    fn an_all_apps_except_source_is_an_app_named_around_its_list() {
        // The id the adapter gives a chosen mix (Chosen::key).
        const MIX: &str = "mix:except:spotify.exe,chrome.exe";
        let mut state = with_sources();
        state.sources.push(crate::ui_model::Source {
            id: MIX.to_owned(),
            name: "Spotify, Chrome".to_owned(),
            kind: SourceKind::AppsExcept,
            channels: 0,
        });
        state.source_id = MIX.to_owned();
        let snapshot = snapshot(&state, not_virtual, None, "Studio PC", &Text);
        let mix = snapshot.sources.last().unwrap();
        assert_eq!(mix.kind, RemoteSourceKind::App);
        assert_eq!(mix.name, "All apps except Spotify, Chrome");
        assert_eq!(snapshot.source_id.as_deref(), Some(MIX));
        let pick = Command::Source { id: MIX.to_owned() };
        assert_eq!(actions(&pick, &state, not_virtual), Ok(Vec::new()));
    }

    // covers: S344
    #[test]
    fn a_current_source_the_picker_doesnt_list_goes_out_as_none() {
        let unlisted = AppState {
            source_id: "input:{unplugged}".to_owned(),
            ..with_sources()
        };
        let snapshot = snapshot(&unlisted, not_virtual, None, "Studio PC", &Text);
        assert_eq!(snapshot.source_id, None);
        assert_eq!(snapshot.sources.len(), 3);
    }

    // covers: S344
    #[test]
    fn a_source_pick_is_the_pickers_and_a_repeat_or_an_unknown_one_does_nothing() {
        let state = with_sources();
        let pick = |id: &str| Command::Source { id: id.to_owned() };
        assert_eq!(
            actions(&pick("input:{mic}"), &state, not_virtual),
            Ok(vec![Action::Source {
                id: "input:{mic}".to_owned()
            }])
        );
        assert_eq!(
            actions(&pick("app:42:Spotify.exe"), &state, not_virtual),
            Ok(Vec::new())
        );
        assert_eq!(
            actions(&pick("app:7:Gone.exe"), &state, not_virtual),
            Err(Refusal::UnknownSource)
        );
    }

    fn speaker(id: &str, kind: SpeakerKind, delay: Option<f64>) -> Speaker {
        Speaker {
            id: id.to_owned(),
            name: id.to_owned(),
            system_name: id.to_owned(),
            kind,
            enabled: false,
            volume: 50.0,
            delay_ms: delay,
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
            holds_password: false,
            holds_credentials: false,
            code_every_time: false,
            own_volume: None,
            device_volume: None,
            waiting: None,
            cast: None,
            group: None,
            mic_left_out: None,
            in_target: None,
        }
    }

    fn state() -> AppState {
        let mut cast = speaker("kitchen", SpeakerKind::Cast, Some(300.0));
        cast.cast = Some(CastRow {
            group: None,
            state: CastState::Idle,
            muted: false,
            volume_fixed: false,
            added_by_address: false,
            offline: false,
            volume_step: 1.0,
        });
        let mut den = speaker("den", SpeakerKind::AirPlay2, None);
        den.availability = Availability::AirPlay2NotYet;
        let mut desk = speaker("desk", SpeakerKind::Usb, Some(-120.0));
        desk.enabled = true;
        AppState {
            running: true,
            no_speaker_on: false,
            sources: Vec::new(),
            source_id: String::new(),
            input_pair: 0,
            mix_apps: Vec::new(),
            mix_all_except: false,
            master_volume: 70.0,
            speakers: vec![
                desk,
                speaker("pc", SpeakerKind::BuiltIn, None),
                cast,
                den,
                speaker("loop", SpeakerKind::Virtual, Some(0.0)),
            ],
            hidden: Vec::new(),
            zones: Vec::new(),
            absent: Vec::new(),
            equalizer: Default::default(),
            equalizer_curves: Vec::new(),
            presets: Vec::new(),
            current_preset: None,
            notices: Vec::new(),
            pin_prompt: None,
            password_prompt: None,
            reference_name: Some("pc".to_owned()),
            airplay_delay_ms: Some(0.0),
            airplay_none_found: false,
            airplay_searching: false,
            when_source_quits: Default::default(),
            cast_list: crate::ui_model::CastList::Quiet,
            cast_by_address: false,
            cast_on: false,
            calibration: Default::default(),
            calibrate_all: Default::default(),
        }
    }

    fn not_virtual(speaker: &Speaker) -> bool {
        speaker.kind != SpeakerKind::Virtual
    }

    fn speaker_command(id: &str, change: SpeakerChange) -> Command {
        Command::Speaker {
            id: id.to_owned(),
            change,
        }
    }

    #[test]
    fn use_port_takes_only_a_valid_port_that_changes_or_retries() {
        assert!(!use_port_enabled("", 8080, true));
        assert!(!use_port_enabled("80", 8080, true));
        assert!(!use_port_enabled("70000", 8080, true));
        assert!(!use_port_enabled("8080", 8080, false));
        assert!(use_port_enabled("8080", 8080, true));
        assert!(use_port_enabled(" 8081 ", 8080, false));
    }

    // covers: S343
    #[test]
    fn the_snapshot_lists_what_the_window_lists_in_its_order() {
        let track = crate::now_playing_card::fake::track("Spotify.exe", "Song", "Band", true);
        let snapshot = snapshot(&state(), not_virtual, Some(&track), "Desk PC", &Text);
        let ids: Vec<&str> = snapshot.speakers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["pc", "den", "desk", "kitchen"],
            "this PC first, then by name"
        );
        assert_eq!(snapshot.computer_name, "Desk PC");
        assert_eq!(snapshot.master_volume, 70.0);
        State::from_json(&snapshot.to_json(1)).expect("the server takes it");
        let playing = snapshot.now_playing.expect("the card's track");
        assert_eq!(
            (
                playing.title.as_str(),
                playing.artist.as_str(),
                playing.source.as_str()
            ),
            ("Song", "Band", "Spotify")
        );
    }

    // covers: S343
    #[test]
    fn each_speaker_carries_its_switch_kind_delay_range_and_note() {
        let snapshot = snapshot(&state(), not_virtual, None, "pc", &Text);
        let get = |id: &str| snapshot.speakers.iter().find(|s| s.id == id).unwrap();
        let desk = get("desk");
        assert!(desk.on);
        assert_eq!(desk.kind, "Usb");
        assert_eq!(
            (desk.delay_ms, desk.delay_min_ms, desk.delay_max_ms),
            (-120, -2000, 20_000)
        );
        let kitchen = get("kitchen");
        assert!(!kitchen.on);
        assert_eq!((kitchen.delay_min_ms, kitchen.delay_max_ms), (0, 20_000));
        let pc = get("pc");
        assert_eq!((pc.delay_ms, pc.delay_min_ms, pc.delay_max_ms), (0, 0, 0));
        let den = get("den");
        assert!(!den.available);
        assert_eq!(den.notice.as_deref(), Some("AirPlay 2 not supported yet"));
        assert_eq!(desk.notice, None);
        assert!(snapshot.now_playing.is_none());
    }

    #[test]
    fn a_speaker_that_will_play_shows_off_and_turning_it_on_is_an_action() {
        let mut state = state();
        state.running = false;
        let snapshot = snapshot(&state, not_virtual, None, "pc", &Text);
        assert!(
            !snapshot
                .speakers
                .iter()
                .find(|s| s.id == "desk")
                .unwrap()
                .on
        );
        let on = SpeakerChange {
            on: Some(true),
            ..Default::default()
        };
        assert_eq!(
            actions(&speaker_command("desk", on), &state, not_virtual),
            Ok(vec![Action::Enable {
                id: "desk".to_owned(),
                on: true
            }])
        );
    }

    #[test]
    fn switching_maps_to_the_rows_switch_and_a_repeat_does_nothing() {
        let state = state();
        let off = SpeakerChange {
            on: Some(false),
            ..Default::default()
        };
        assert_eq!(
            actions(&speaker_command("desk", off.clone()), &state, not_virtual),
            Ok(vec![Action::Enable {
                id: "desk".to_owned(),
                on: false
            }])
        );
        assert_eq!(
            actions(&speaker_command("kitchen", off), &state, not_virtual),
            Ok(Vec::new())
        );
    }

    #[test]
    fn volume_and_master_volume_take_0_to_100() {
        let state = state();
        let change = |volume| SpeakerChange {
            volume: Some(volume),
            ..Default::default()
        };
        assert_eq!(
            actions(&speaker_command("desk", change(35.0)), &state, not_virtual),
            Ok(vec![Action::Volume {
                id: "desk".to_owned(),
                volume: 35.0
            }])
        );
        assert_eq!(
            actions(&speaker_command("desk", change(101.0)), &state, not_virtual),
            Err(Refusal::OutOfRange)
        );
        assert_eq!(
            actions(&Command::Master { volume: 0.0 }, &state, not_virtual),
            Ok(vec![Action::Master { volume: 0.0 }])
        );
        assert_eq!(
            actions(&Command::Master { volume: -1.0 }, &state, not_virtual),
            Err(Refusal::OutOfRange)
        );
        assert_eq!(
            actions(&Command::Master { volume: f64::NAN }, &state, not_virtual),
            Err(Refusal::OutOfRange)
        );
    }

    #[test]
    fn a_delay_must_stay_in_the_speakers_range() {
        let state = state();
        let change = |ms| SpeakerChange {
            delay_ms: Some(ms),
            ..Default::default()
        };
        let delay = |id: &str, ms| actions(&speaker_command(id, change(ms)), &state, not_virtual);
        assert_eq!(
            delay("desk", -2000),
            Ok(vec![Action::Delay {
                id: "desk".to_owned(),
                ms: -2000.0
            }])
        );
        assert_eq!(delay("desk", -2001), Err(Refusal::OutOfRange));
        assert_eq!(delay("desk", 20_001), Err(Refusal::OutOfRange));
        assert_eq!(
            delay("kitchen", -10),
            Err(Refusal::OutOfRange),
            "Cast starts at 0"
        );
        assert_eq!(
            delay("pc", 0),
            Ok(Vec::new()),
            "the reference has no offset"
        );
        assert_eq!(delay("pc", 10), Err(Refusal::OutOfRange));
    }

    fn with_apple_tv(availability: Availability) -> AppState {
        let mut state = state();
        let mut tv = speaker("tv", SpeakerKind::AirPlay, None);
        tv.availability = availability;
        state.speakers.push(tv);
        state.airplay_delay_ms = Some(1750.0);
        state
    }

    #[test]
    fn a_speaker_that_asks_for_a_code_is_turned_on_at_the_pc_not_from_the_page() {
        let state = with_apple_tv(Availability::NeedsCode);
        let on = SpeakerChange {
            on: Some(true),
            ..Default::default()
        };
        let tv = |state: &AppState| {
            snapshot(state, not_virtual, None, "pc", &Text)
                .speakers
                .into_iter()
                .find(|s| s.id == "tv")
                .unwrap()
        };
        assert!(!tv(&state).available, "the prompt would open at the PC");
        assert_eq!(tv(&state).notice.as_deref(), Some("Turn it on at the PC"));
        assert_eq!(
            actions(&speaker_command("tv", on.clone()), &state, not_virtual),
            Err(Refusal::Unavailable)
        );
        let mut paired = state.clone();
        paired.speakers.last_mut().unwrap().paired = true;
        assert!(
            tv(&paired).available,
            "paired, it turns on without a prompt"
        );
        assert_eq!(tv(&paired).notice.as_deref(), Some("Asks for a code"));
        let mut held = state.clone();
        held.speakers.last_mut().unwrap().holds_password = true;
        assert!(
            tv(&held).available,
            "its password held this run, it turns on without a prompt"
        );
        let mut playing = state.clone();
        playing.speakers.last_mut().unwrap().enabled = true;
        assert!(tv(&playing).available, "on, it can be turned off");
    }

    #[test]
    fn an_airplay_speaker_carries_the_shared_airplay_delay_and_sets_it() {
        let state = with_apple_tv(Availability::Open);
        let snapshot = snapshot(&state, not_virtual, None, "pc", &Text);
        let tv = snapshot.speakers.iter().find(|s| s.id == "tv").unwrap();
        assert_eq!(
            (tv.delay_ms, tv.delay_min_ms, tv.delay_max_ms),
            (1750, 0, 20_000)
        );
        let change = |ms| SpeakerChange {
            delay_ms: Some(ms),
            ..Default::default()
        };
        let delay = |ms| actions(&speaker_command("tv", change(ms)), &state, not_virtual);
        assert_eq!(delay(2100), Ok(vec![Action::AirPlayDelay { ms: 2100.0 }]));
        assert_eq!(delay(-1), Err(Refusal::OutOfRange));
        assert_eq!(delay(20_001), Err(Refusal::OutOfRange));
        let mut no_sender = state.clone();
        no_sender.airplay_delay_ms = None;
        assert_eq!(
            actions(&speaker_command("tv", change(10)), &no_sender, not_virtual),
            Err(Refusal::OutOfRange),
            "no AirPlay sender, no delay field"
        );
    }

    fn zoned() -> AppState {
        let mut state = state();
        state.speakers[2].enabled = true;
        state.speakers[2].waiting = Some(Waiting::ForAirPlay2Slot);
        let mut tv = speaker("tv", SpeakerKind::AirPlay, None);
        tv.availability = Availability::NeedsCode;
        state.speakers.push(tv);
        state.zones = vec![
            Zone {
                id: "zone:a".into(),
                name: "Downstairs".into(),
                members: vec!["kitchen".into(), "loop".into(), "desk".into(), "tv".into()],
                volume: 80.0,
                member_names: Default::default(),
            },
            Zone {
                id: "zone:b".into(),
                name: "Studio".into(),
                members: vec!["loop".into()],
                volume: 100.0,
                member_names: Default::default(),
            },
        ];
        state.presets = vec![
            PresetItem {
                id: "preset:a".into(),
                name: "Evening".into(),
                current: true,
                members: vec!["desk".into()],
            },
            PresetItem {
                id: "preset:b".into(),
                name: "Party".into(),
                current: false,
                members: vec!["tv".into(), "kitchen".into()],
            },
        ];
        state
    }

    fn zone_command(id: &str, toggle: bool, volume: Option<f64>) -> Command {
        Command::Zone {
            id: id.to_owned(),
            change: ZoneChange { toggle, volume },
        }
    }

    // covers: S256, S253
    #[test]
    fn the_snapshot_lists_zones_with_the_listed_members_and_the_presets() {
        let snapshot = snapshot(&zoned(), not_virtual, None, "Desk PC", &Text);
        let downstairs = &snapshot.zones[0];
        assert_eq!(
            downstairs.members,
            ["kitchen", "desk", "tv"],
            "the hidden output stays out"
        );
        assert_eq!(downstairs.volume, 80.0);
        assert_eq!(
            downstairs.counts,
            ZoneCounts {
                on: 2,
                total: 3,
                waiting: 1,
                all: 4
            }
        );
        assert_eq!(snapshot.zones[1].counts.total, 0);
        let presets: Vec<(&str, bool)> = snapshot
            .presets
            .iter()
            .map(|preset| (preset.id.as_str(), preset.current))
            .collect();
        assert_eq!(presets, [("preset:a", true), ("preset:b", false)]);
        assert!(!snapshot.calibrating);
        assert!(
            State::from_json(&snapshot.to_json(0)).is_ok(),
            "the snapshot passes the crate's checks"
        );
    }

    // covers: S256
    #[test]
    fn a_zone_counts_on_as_its_switches_show_and_its_absent_members_in_all() {
        let mut state = zoned();
        state.running = false;
        let snapshot = snapshot(&state, not_virtual, None, "Desk PC", &Text);
        let downstairs = &snapshot.zones[0];
        assert!(
            downstairs
                .members
                .iter()
                .all(|id| !snapshot.speaker(id).unwrap().on),
            "with streaming off every switch shows off"
        );
        assert_eq!((downstairs.counts.on, downstairs.counts.waiting), (0, 0));
        let written: serde_json::Value = serde_json::from_str(&snapshot.to_json(0)).unwrap();
        assert_eq!(written["zones"][0]["counts"]["all"], 4);
        assert_eq!(
            written["zones"][1]["counts"]["all"], 1,
            "the page lists none of Studio's members"
        );
        assert!(State::from_json(&snapshot.to_json(0)).is_ok());
    }

    // covers: S340
    #[test]
    fn a_paused_track_goes_out_as_paused() {
        let track = crate::now_playing_card::fake::track("Spotify.exe", "Song", "Band", false);
        let snapshot = snapshot(&state(), not_virtual, Some(&track), "Desk PC", &Text);
        let written: serde_json::Value = serde_json::from_str(&snapshot.to_json(0)).unwrap();
        assert_eq!(written["now_playing"]["playing"], false);
    }

    // covers: S257
    #[test]
    fn zone_and_preset_commands_map_to_the_windows_actions() {
        let state = zoned();
        assert_eq!(
            actions(
                &zone_command("zone:a", true, Some(40.0)),
                &state,
                not_virtual
            ),
            Ok(vec![
                Action::ZoneVolume {
                    id: "zone:a".into(),
                    volume: 40.0
                },
                Action::ToggleZone {
                    id: "zone:a".into(),
                    keep_off: vec!["tv".into()],
                },
            ])
        );
        assert_eq!(
            actions(
                &Command::Preset {
                    id: "preset:b".into()
                },
                &state,
                not_virtual
            ),
            Ok(vec![Action::RecallPreset {
                id: "preset:b".into(),
                keep_off: vec!["tv".into()],
            }])
        );
        assert_eq!(
            actions(&zone_command("zone:gone", true, None), &state, not_virtual),
            Err(Refusal::Unknown)
        );
        assert_eq!(
            actions(
                &Command::Preset {
                    id: "preset:gone".into()
                },
                &state,
                not_virtual
            ),
            Err(Refusal::Unknown)
        );
        assert_eq!(
            actions(&zone_command("zone:b", true, None), &state, not_virtual),
            Err(Refusal::Unavailable),
            "a zone whose only member the window hides"
        );
        let mut only_tv = state.clone();
        only_tv.zones[1].members = vec!["tv".into()];
        assert_eq!(
            actions(&zone_command("zone:b", true, None), &only_tv, not_virtual),
            Err(Refusal::Unavailable),
            "a zone whose only member asks for a code"
        );
        assert_eq!(
            actions(
                &zone_command("zone:a", false, Some(101.0)),
                &state,
                not_virtual
            ),
            Err(Refusal::OutOfRange)
        );
    }

    // covers: S258
    #[test]
    fn the_host_refuses_a_command_that_arrives_during_a_run() {
        let mut state = zoned();
        state.calibration.busy = true;
        assert!(snapshot(&state, not_virtual, None, "Desk PC", &Text).calibrating);
        let commands = [
            Command::Master { volume: 50.0 },
            speaker_command(
                "desk",
                SpeakerChange {
                    on: Some(false),
                    ..Default::default()
                },
            ),
            zone_command("zone:a", true, None),
            Command::Preset {
                id: "preset:a".into(),
            },
        ];
        for command in &commands {
            assert_eq!(
                actions(command, &state, not_virtual),
                Err(Refusal::Calibrating),
                "{command:?}"
            );
        }
    }

    #[test]
    fn an_unknown_hidden_or_unavailable_speaker_is_refused() {
        let state = state();
        let on = SpeakerChange {
            on: Some(true),
            ..Default::default()
        };
        assert_eq!(
            actions(&speaker_command("gone", on.clone()), &state, not_virtual),
            Err(Refusal::UnknownSpeaker)
        );
        assert_eq!(
            actions(&speaker_command("loop", on.clone()), &state, not_virtual),
            Err(Refusal::UnknownSpeaker),
            "a virtual output the window hides"
        );
        assert_eq!(
            actions(&speaker_command("den", on.clone()), &state, not_virtual),
            Err(Refusal::Unavailable)
        );
        let mut fixed = state.clone();
        fixed.speakers[2].cast.as_mut().unwrap().volume_fixed = true;
        let louder = SpeakerChange {
            volume: Some(80.0),
            on: Some(true),
            ..Default::default()
        };
        assert_eq!(
            actions(&speaker_command("kitchen", louder), &fixed, not_virtual),
            Err(Refusal::Unavailable),
            "a Cast speaker whose volume can't be set"
        );
    }

    #[test]
    fn the_setting_keeps_a_usable_port() {
        assert_eq!(
            RemoteSettings::default(),
            RemoteSettings {
                on: false,
                port: DEFAULT_PORT
            }
        );
        let low = RemoteSettings { on: true, port: 80 }.clamped();
        assert_eq!(
            low,
            RemoteSettings {
                on: true,
                port: DEFAULT_PORT
            }
        );
        assert_eq!(parse_port(" 8080 "), Some(8080));
        assert_eq!(parse_port("1023"), None);
        assert_eq!(parse_port("65536"), None);
        assert_eq!(parse_port("port"), None);
    }

    #[test]
    fn addresses_codes_and_the_qr_code() {
        assert_eq!(local_host("Kitchen").as_deref(), Some("kitchen"));
        assert_eq!(local_host("my pc"), None);
        assert_eq!(local_host(""), None);
        assert_eq!(local_url("kitchen", 7655), "http://kitchen.local:7655");
        assert_eq!(
            ip_url(Ipv4Addr::new(198, 51, 100, 20), 7655),
            "http://198.51.100.20:7655"
        );
        assert_eq!(spaced_digits("042917"), "0 4 2 9 1 7");
        let (width, modules) = qr_modules("http://kitchen.local:7655").unwrap();
        assert!(width >= 21 && width % 2 == 1);
        assert_eq!(modules.len(), width * width);
        let (side, pixels) = qr_pixels("http://kitchen.local:7655", 3).unwrap();
        assert_eq!(side, (width + 8) * 3);
        assert_eq!(pixels.len(), side * side);
        assert_eq!(pixels[0], [255; 3], "the quiet zone is light");
        // The top-left finder pattern starts dark right after the quiet zone.
        assert_eq!(pixels[(4 * 3) * side + 4 * 3], [0; 3]);
    }
}
