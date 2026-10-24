//! Remote commands from AirPlay speakers (S273 to S279): what a speaker's
//! remote asks for through DACP (`docs/AIRPLAY_SPEC.md` P192) becomes a
//! now-playing control to send, a move of the row's own volume, or nothing,
//! and one debug log line. Both hosts call it, so the Mac and Windows agree.

use std::collections::HashMap;

use super::Seconds;

/// Commands of one kind this close together count once (S278).
pub const REPEAT_WINDOW: Seconds = 0.3;

/// How far VolumeUp and VolumeDown move the own volume (S275).
pub const VOLUME_STEP: f64 = 5.0;

/// Presses exactly [`REPEAT_WINDOW`] apart both count, whatever the clock's
/// rounding.
const CLOCK_SLACK: Seconds = 1e-6;

/// A level in dB at or below which a DeviceVolume is the receiver muting.
pub const MUTED_DB: f64 = -144.0;

/// What a speaker's remote asked for (P192, S273).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    MuteToggle,
    /// The level the receiver asks for, in dB; [`MUTED_DB`] for muted.
    DeviceVolume(f64),
}

impl Command {
    /// One number per kind of command, a DeviceVolume's level left out.
    fn kind(self) -> u8 {
        match self {
            Command::PlayPause => 0,
            Command::Play => 1,
            Command::Pause => 2,
            Command::Next => 3,
            Command::Previous => 4,
            Command::VolumeUp => 5,
            Command::VolumeDown => 6,
            Command::MuteToggle => 7,
            Command::DeviceVolume(_) => 8,
        }
    }

    fn is_transport(self) -> bool {
        self.kind() <= 4
    }

    fn describe(self) -> String {
        match self {
            Command::PlayPause => "PlayPause".to_owned(),
            Command::Play => "Play".to_owned(),
            Command::Pause => "Pause".to_owned(),
            Command::Next => "Next".to_owned(),
            Command::Previous => "Previous".to_owned(),
            Command::VolumeUp => "VolumeUp".to_owned(),
            Command::VolumeDown => "VolumeDown".to_owned(),
            Command::MuteToggle => "MuteToggle".to_owned(),
            Command::DeviceVolume(db) if db <= MUTED_DB => "DeviceVolume muted".to_owned(),
            Command::DeviceVolume(db) => format!("DeviceVolume {db:.2} dB"),
        }
    }
}

/// A control of the host's now-playing control (S274).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    PlayPause,
    Next,
    Previous,
}

impl Control {
    fn describe(self) -> &'static str {
        match self {
            Control::PlayPause => "play-pause",
            Control::Next => "next track",
            Control::Previous => "previous track",
        }
    }
}

/// What the latest now-playing reading says of the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Playback {
    Playing,
    Paused,
    /// No reading yet, or one that doesn't say.
    Unknown,
}

/// The app the host's now-playing control follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct App<'a> {
    /// Its name for the log.
    pub name: &'a str,
    pub playback: Playback,
}

/// The acting row's own volume as the host has it now (S249, S271).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OwnVolume {
    /// Its latest position, 0 to 100, one not sent yet included.
    pub position: f64,
    /// The user drags its slider.
    pub dragging: bool,
    /// For a DeviceVolume: its level lies within 0.5 dB of the one the host
    /// sent the speaker in the last 3 s (S271).
    pub echo: bool,
    /// For a DeviceVolume: the position, rounded to a whole step, that
    /// plays at its level through the zone and the master; `None` when
    /// every position sounds the same there.
    pub reported: Option<f64>,
}

/// A command the sender handed over (P193).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event<'a> {
    /// The speaker that sent it, by name.
    pub speaker: &'a str,
    /// The row it acts on, the target's for a member (S278); `None` when
    /// the host no longer plays the speaker.
    pub row: Option<&'a str>,
    /// It came from a member of a stereo pair or home theater group.
    pub member: bool,
    pub command: Command,
    /// What the now-playing control follows; `None` with no app to control.
    pub app: Option<App<'a>>,
    pub volume: OwnVolume,
    pub at: Seconds,
}

/// What the host does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Nothing,
    /// Send this control to the app the now-playing control follows.
    Send(Control),
    /// Set the row's own volume to `to` as at the end of a drag: send it
    /// to the speaker, which starts the 3 s window (S271).
    Move {
        from: f64,
        to: f64,
    },
}

/// An event's action, with its one debug log line (S279).
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub action: Action,
    pub line: String,
}

/// The decisions' memory: the latest counted presses and the positions
/// MuteToggle muted.
#[derive(Debug, Default)]
pub struct Remote {
    /// The latest counted transport command: its kind and when.
    transport: Option<(u8, Seconds)>,
    /// Per target row, the latest counted VolumeUp, VolumeDown or
    /// MuteToggle from one of its members.
    member_presses: HashMap<String, (u8, Seconds)>,
    /// Per row, the position MuteToggle muted from.
    muted_from: HashMap<String, f64>,
}

impl Remote {
    /// Decides what `event` does (S273 to S279).
    pub fn decide(&mut self, event: &Event<'_>) -> Decision {
        let (action, outcome) = self.act(event);
        let line = format!(
            "AirPlay remote {}: {} -> {outcome}",
            event.speaker,
            event.command.describe()
        );
        Decision { action, line }
    }

    /// The row's own volume was moved by the user, or its session is new:
    /// a MuteToggle has nothing to restore there (S277).
    pub fn forget(&mut self, row: &str) {
        self.muted_from.remove(row);
    }

    fn act(&mut self, event: &Event<'_>) -> (Action, String) {
        let Some(row) = event.row else {
            return (Action::Nothing, "ignored, speaker not playing".to_owned());
        };
        if self.repeated(event, row) {
            return (
                Action::Nothing,
                "ignored, counted once with an earlier press".to_owned(),
            );
        }
        let own = event.volume;
        match event.command {
            Command::PlayPause => send(event.app, Control::PlayPause),
            Command::Next => send(event.app, Control::Next),
            Command::Previous => send(event.app, Control::Previous),
            Command::Play | Command::Pause => {
                let wanted = if event.command == Command::Play {
                    Playback::Playing
                } else {
                    Playback::Paused
                };
                match event.app {
                    Some(app) if app.playback == wanted => {
                        let state = if wanted == Playback::Playing {
                            "plays"
                        } else {
                            "is paused"
                        };
                        (
                            Action::Nothing,
                            format!("nothing sent, {} already {state}", app.name),
                        )
                    }
                    app => send(app, Control::PlayPause),
                }
            }
            Command::VolumeUp => self.step(row, own.position, VOLUME_STEP),
            Command::VolumeDown => self.step(row, own.position, -VOLUME_STEP),
            Command::MuteToggle => self.mute_toggle(row, own.position),
            Command::DeviceVolume(_) => {
                if own.dragging {
                    return (Action::Nothing, "ignored, slider dragged".to_owned());
                }
                if own.echo {
                    return (
                        Action::Nothing,
                        "ignored, echo of the level sent".to_owned(),
                    );
                }
                let Some(to) = own.reported else {
                    return (
                        Action::Nothing,
                        "ignored, every position sounds the same at this zone and master volume"
                            .to_owned(),
                    );
                };
                if to == own.position.round() {
                    return (
                        Action::Nothing,
                        format!("own volume stays at {}", position_text(own.position)),
                    );
                }
                self.move_to(row, own.position, to)
            }
        }
    }

    /// Whether `event` repeats a counted press within [`REPEAT_WINDOW`]:
    /// any transport command of its kind, or a volume press of its kind
    /// from a member of the same target (S278). Counted presses are kept.
    fn repeated(&mut self, event: &Event<'_>, row: &str) -> bool {
        let kind = event.command.kind();
        let within = |latest: Option<&(u8, Seconds)>| {
            latest.is_some_and(|&(latest, at)| {
                latest == kind && event.at - at < REPEAT_WINDOW - CLOCK_SLACK && event.at >= at
            })
        };
        if event.command.is_transport() {
            if within(self.transport.as_ref()) {
                return true;
            }
            self.transport = Some((kind, event.at));
            return false;
        }
        let pressed = matches!(
            event.command,
            Command::VolumeUp | Command::VolumeDown | Command::MuteToggle
        );
        if !pressed || !event.member {
            return false;
        }
        if within(self.member_presses.get(row)) {
            return true;
        }
        self.member_presses.insert(row.to_owned(), (kind, event.at));
        false
    }

    fn step(&mut self, row: &str, position: f64, by: f64) -> (Action, String) {
        let to = (position + by).clamp(0.0, 100.0);
        self.move_to(row, position, to)
    }

    fn move_to(&mut self, row: &str, from: f64, to: f64) -> (Action, String) {
        if to == from {
            return (
                Action::Nothing,
                format!("own volume stays at {}", position_text(from)),
            );
        }
        self.muted_from.remove(row);
        moved(from, to)
    }

    fn mute_toggle(&mut self, row: &str, position: f64) -> (Action, String) {
        if position > 0.0 {
            self.muted_from.insert(row.to_owned(), position);
            return moved(position, 0.0);
        }
        match self.muted_from.remove(row) {
            Some(before) => moved(0.0, before),
            None => (
                Action::Nothing,
                "nothing to restore, own volume stays at 0".to_owned(),
            ),
        }
    }
}

fn send(app: Option<App<'_>>, control: Control) -> (Action, String) {
    match app {
        Some(app) => (
            Action::Send(control),
            format!("sent to {} as {}", app.name, control.describe()),
        ),
        None => (Action::Nothing, "ignored, no app to control".to_owned()),
    }
}

fn moved(from: f64, to: f64) -> (Action, String) {
    (
        Action::Move { from, to },
        format!(
            "own volume {} -> {}",
            position_text(from),
            position_text(to)
        ),
    )
}

/// A position as a whole number when it is one, else to one decimal.
fn position_text(position: f64) -> String {
    if position.fract() == 0.0 {
        format!("{position:.0}")
    } else {
        format!("{position:.1}")
    }
}
