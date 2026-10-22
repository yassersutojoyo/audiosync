//! The web remote, as the app sees it: the setting it keeps, the snapshot
//! the page shows, and what a command from the page does.
//!
//! The server (`audiosync_remote`) never reads the app's state. The windows
//! publish [`snapshot`] after every change, and each [`Command`] the page
//! sends becomes the [`Action`]s the window's own controls would take,
//! checked against the state the app holds when it applies them
//! ([`actions`]).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use audiosync_remote::{Command, DEFAULT_PORT, NowPlaying, Speaker as RemoteSpeaker, State};
use serde::{Deserialize, Serialize};

use crate::now_playing_card::{CardTrack, app_name};
use crate::ui_model::{AppState, Availability, MAX_DELAY_MS, MIN_DELAY_MS, Speaker};

/// The lowest port the setting takes: below it are the well-known services.
pub const MIN_PORT: u16 = 1024;

/// File > Web Remote, as the settings file keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    pub on: bool,
    pub port: u16,
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

/// The words a speaker row shows, from the interface's strings.
pub trait RowText {
    /// The speaker's kind ("AirPlay 2", "Bluetooth").
    fn kind(&self, speaker: &Speaker) -> String;
    /// The note under the row's name, as the window shows it; empty for none.
    fn note(&self, state: &AppState, speaker: &Speaker) -> String;
}

/// Whether the page may drive `speaker`: one that needs a kind of pairing
/// or a protocol AudioSync lacks, or is pairing now, is shown without controls.
pub fn can_drive(speaker: &Speaker) -> bool {
    matches!(
        speaker.availability,
        Availability::Open | Availability::NeedsCode
    )
}

/// The delay range of `speaker`'s offset field, in whole milliseconds; `None`
/// for one without an offset (the reference output, AirPlay speakers).
pub fn delay_range(speaker: &Speaker) -> Option<(i64, i64)> {
    speaker.delay_ms?;
    // A Cast speaker's offset only holds the others back (spec C77).
    let min = if speaker.cast.is_some() {
        0
    } else {
        MIN_DELAY_MS as i64
    };
    Some((min, MAX_DELAY_MS as i64))
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
/// master volume, the track on the Now Playing card, and the computer's name.
pub fn snapshot(
    state: &AppState,
    shown: impl Fn(&Speaker) -> bool,
    track: Option<&CardTrack>,
    computer_name: &str,
    text: &dyn RowText,
) -> State {
    let speakers = listed(state, shown)
        .into_iter()
        .map(|speaker| {
            let (min, max) = delay_range(speaker).unwrap_or((0, 0));
            let note = text.note(state, speaker);
            RemoteSpeaker {
                id: speaker.id.clone(),
                name: speaker.name.clone(),
                kind: text.kind(speaker),
                // Like the row's button: "Will play" still turns on.
                on: !state.speaker_state(speaker).click_turns_on(),
                volume: volume(speaker.volume),
                volume_step: speaker.cast.as_ref().map_or(1.0, |cast| cast.volume_step),
                delay_ms: speaker
                    .delay_ms
                    .filter(|ms| ms.is_finite())
                    .map_or(0, |ms| ms.round() as i64)
                    .clamp(min, max),
                delay_min_ms: min,
                delay_max_ms: max,
                available: can_drive(speaker),
                notice: (!note.is_empty()).then_some(note),
            }
        })
        .collect();
    State {
        computer_name: computer_name.to_owned(),
        master_volume: volume(state.master_volume),
        now_playing: track.map(|track| NowPlaying {
            title: track.title.clone(),
            artist: track.artist.clone(),
            source: app_name(&track.app_id),
        }),
        speakers,
    }
}

/// One change a command makes, as a control in the window would.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Enable { id: String, on: bool },
    Volume { id: String, volume: f64 },
    Delay { id: String, ms: f64 },
    Master { volume: f64 },
}

/// Why a command changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No listed speaker has that ID (any more).
    UnknownSpeaker,
    /// The speaker can't be driven now, or not that way.
    Unavailable,
    /// A volume or delay outside what the control takes.
    OutOfRange,
}

/// The actions `command` takes on `state`, among the speakers the window
/// lists (`shown`); none when a part of it can't be applied. Unchanged values
/// take none, so a repeated switch doesn't restart a stream.
pub fn actions(
    command: &Command,
    state: &AppState,
    shown: impl Fn(&Speaker) -> bool,
) -> Result<Vec<Action>, Refusal> {
    let in_range = |value: f64| value.is_finite() && (0.0..=100.0).contains(&value);
    match command {
        Command::Master { volume } => {
            if !in_range(*volume) {
                return Err(Refusal::OutOfRange);
            }
            Ok(vec![Action::Master { volume: *volume }])
        }
        Command::Speaker { id, change } => {
            let speaker = state
                .speaker(id)
                .filter(|speaker| shown(speaker))
                .ok_or(Refusal::UnknownSpeaker)?;
            if !can_drive(speaker) {
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
                match delay_range(speaker) {
                    Some((min, max)) if (min..=max).contains(&ms) => {
                        actions.push(Action::Delay {
                            id: id.clone(),
                            ms: ms as f64,
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

/// Windows' DNS host name for this PC.
#[cfg(windows)]
pub fn computer_name() -> Option<String> {
    use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
    use windows::core::PWSTR;

    let mut size = 0u32;
    // The first call fails and reports the size the name needs.
    // SAFETY: no buffer is passed; `size` is a valid out parameter.
    let _ = unsafe { GetComputerNameExW(ComputerNameDnsHostname, None, &mut size) };
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: `buffer` holds `size` UTF-16 units, as `size` says.
    unsafe {
        GetComputerNameExW(
            ComputerNameDnsHostname,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut size,
        )
    }
    .ok()?;
    buffer.truncate(size as usize);
    String::from_utf16(&buffer).ok()
}

#[cfg(not(windows))]
pub fn computer_name() -> Option<String> {
    std::env::var("HOSTNAME").ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::{CastRow, CastState, SpeakerKind};
    use audiosync_remote::SpeakerChange;

    struct Text;

    impl RowText for Text {
        fn kind(&self, speaker: &Speaker) -> String {
            format!("{:?}", speaker.kind)
        }
        fn note(&self, _state: &AppState, speaker: &Speaker) -> String {
            match speaker.availability {
                Availability::AirPlay2NotYet => "AirPlay 2 not supported yet".to_owned(),
                _ => String::new(),
            }
        }
    }

    fn speaker(id: &str, kind: SpeakerKind, delay: Option<f64>) -> Speaker {
        Speaker {
            id: id.to_owned(),
            name: id.to_owned(),
            kind,
            enabled: false,
            volume: 50.0,
            delay_ms: delay,
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
            device_volume: None,
            waiting: None,
            cast: None,
            group: None,
        }
    }

    fn state() -> AppState {
        let mut cast = speaker("kitchen", SpeakerKind::Cast, Some(300.0));
        cast.cast = Some(CastRow {
            group: None,
            state: CastState::Idle,
            muted: false,
            volume_fixed: false,
            volume_step: 1.0,
        });
        let mut den = speaker("den", SpeakerKind::AirPlay2, None);
        den.availability = Availability::AirPlay2NotYet;
        let mut desk = speaker("desk", SpeakerKind::Usb, Some(-120.0));
        desk.enabled = true;
        AppState {
            running: true,
            sources: Vec::new(),
            source_id: String::new(),
            master_volume: 70.0,
            speakers: vec![
                desk,
                speaker("pc", SpeakerKind::BuiltIn, None),
                cast,
                den,
                speaker("loop", SpeakerKind::Virtual, Some(0.0)),
            ],
            groups: Vec::new(),
            equalizer: Default::default(),
            notices: Vec::new(),
            pin_prompt: None,
            password_prompt: None,
            reference_name: Some("pc".to_owned()),
            airplay_delay_ms: Some(0.0),
            airplay_none_found: false,
            when_source_quits: Default::default(),
            cast_list: crate::ui_model::CastList::Quiet,
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
