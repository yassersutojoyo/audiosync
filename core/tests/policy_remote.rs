//! Remote commands from AirPlay speakers: the core's decision (S273 to
//! S279). The volume checks that need the sender's levels and the 3 s
//! window run on the core's AirPlay handle, in `core/src/ffi/airplay.rs`.

use audiosync_core::policy::remote::{
    Action, App, Command, Control, Decision, Event, OwnVolume, Playback, Remote,
};

const KITCHEN: &str = "02005E005301";
const PAIR: &str = "pair:6A2C";

fn own(level: f64) -> OwnVolume {
    OwnVolume {
        level,
        dragging: false,
        echo: false,
        reported: None,
    }
}

fn music(playback: Playback) -> Option<App<'static>> {
    Some(App {
        name: "Music",
        playback,
    })
}

/// A command from the Kitchen speaker, played as its own row, at `at` s.
fn kitchen(command: Command, at: f64) -> Event<'static> {
    Event {
        speaker: "Kitchen",
        row: Some(KITCHEN),
        member: false,
        command,
        app: music(Playback::Playing),
        volume: own(40.0),
        at,
    }
}

/// A command from a member of the stereo pair, which acts on its row.
fn member(speaker: &'static str, command: Command, at: f64) -> Event<'static> {
    Event {
        speaker,
        row: Some(PAIR),
        member: true,
        ..kitchen(command, at)
    }
}

fn moved(decision: &Decision) -> Option<(f64, f64)> {
    match decision.action {
        Action::Move { from, to } => Some((from, to)),
        _ => None,
    }
}

// covers: S274
#[test]
fn a_remote_play_pause_goes_to_the_now_playing_control() {
    let mut remote = Remote::default();
    for (command, control) in [
        (Command::PlayPause, Control::PlayPause),
        (Command::Next, Control::Next),
        (Command::Previous, Control::Previous),
    ] {
        let decision = remote.decide(&kitchen(command, 0.0));
        assert_eq!(decision.action, Action::Send(control));
        assert!(decision.line.contains("sent to Music"), "{}", decision.line);
    }
    // Pause while the app plays, Play while it is paused: play-pause.
    let pause = remote.decide(&kitchen(Command::Pause, 1.0));
    assert_eq!(pause.action, Action::Send(Control::PlayPause));
    let play = Event {
        app: music(Playback::Paused),
        ..kitchen(Command::Play, 2.0)
    };
    assert_eq!(
        remote.decide(&play).action,
        Action::Send(Control::PlayPause)
    );
}

// covers: S274
#[test]
fn a_remote_play_while_the_app_plays_sends_nothing() {
    let mut remote = Remote::default();
    let decision = remote.decide(&kitchen(Command::Play, 0.0));
    assert_eq!(decision.action, Action::Nothing);
    assert!(
        decision.line.contains("Music already plays"),
        "{}",
        decision.line
    );
    let paused = Event {
        app: music(Playback::Paused),
        ..kitchen(Command::Pause, 1.0)
    };
    assert_eq!(remote.decide(&paused).action, Action::Nothing);
}

// covers: S274
#[test]
fn a_remote_pause_without_a_reading_goes_as_play_pause() {
    let mut remote = Remote::default();
    for (command, at) in [(Command::Pause, 0.0), (Command::Play, 1.0)] {
        let unknown = Event {
            app: music(Playback::Unknown),
            ..kitchen(command, at)
        };
        assert_eq!(
            remote.decide(&unknown).action,
            Action::Send(Control::PlayPause)
        );
    }
}

// covers: S274, S279
#[test]
fn a_remote_command_with_no_app_to_control_does_nothing() {
    let mut remote = Remote::default();
    for (index, command) in [
        Command::PlayPause,
        Command::Play,
        Command::Pause,
        Command::Next,
        Command::Previous,
    ]
    .into_iter()
    .enumerate()
    {
        let alone = Event {
            app: None,
            ..kitchen(command, index as f64)
        };
        let decision = remote.decide(&alone);
        assert_eq!(decision.action, Action::Nothing);
        assert!(
            decision.line.ends_with("ignored, no app to control"),
            "{}",
            decision.line
        );
    }
}

// covers: S275
#[test]
fn remote_volume_steps_clamp_at_0_and_100() {
    let mut remote = Remote::default();
    let up = remote.decide(&kitchen(Command::VolumeUp, 0.0));
    assert_eq!(moved(&up), Some((40.0, 45.0)));
    let down = remote.decide(&kitchen(Command::VolumeDown, 1.0));
    assert_eq!(moved(&down), Some((40.0, 35.0)));
    let near_top = Event {
        volume: own(98.0),
        ..kitchen(Command::VolumeUp, 2.0)
    };
    assert_eq!(moved(&remote.decide(&near_top)), Some((98.0, 100.0)));
    let top = Event {
        volume: own(100.0),
        ..kitchen(Command::VolumeUp, 3.0)
    };
    let decision = remote.decide(&top);
    assert_eq!(decision.action, Action::Nothing);
    assert!(decision.line.ends_with("level stays at 100"));
    let near_bottom = Event {
        volume: own(3.0),
        ..kitchen(Command::VolumeDown, 4.0)
    };
    assert_eq!(moved(&remote.decide(&near_bottom)), Some((3.0, 0.0)));
    let bottom = Event {
        volume: own(0.0),
        ..kitchen(Command::VolumeDown, 5.0)
    };
    assert_eq!(remote.decide(&bottom).action, Action::Nothing);
    // Counted from the row the latest level shows, one not sent yet included.
    let fraction = Event {
        volume: own(42.5),
        ..kitchen(Command::VolumeUp, 6.0)
    };
    assert_eq!(moved(&remote.decide(&fraction)), Some((42.5, 47.0)));
}

// covers: S277
#[test]
fn mute_toggle_twice_restores_the_position_before() {
    let mut remote = Remote::default();
    let mute = remote.decide(&kitchen(Command::MuteToggle, 0.0));
    assert_eq!(moved(&mute), Some((40.0, 0.0)));
    let muted = Event {
        volume: own(0.0),
        ..kitchen(Command::MuteToggle, 1.0)
    };
    let restore = remote.decide(&muted);
    assert_eq!(moved(&restore), Some((0.0, 40.0)));
    assert!(restore.line.ends_with("level 0 -> 40"), "{}", restore.line);
    // Restored once: at 0 again with nothing remembered, nothing happens.
    let again = Event {
        volume: own(0.0),
        ..kitchen(Command::MuteToggle, 2.0)
    };
    assert_eq!(remote.decide(&again).action, Action::Nothing);
}

// covers: S277
#[test]
fn mute_toggle_after_a_volume_change_does_not_restore() {
    // The user moved the volume, then back to 0.
    let mut remote = Remote::default();
    remote.decide(&kitchen(Command::MuteToggle, 0.0));
    remote.forget(KITCHEN);
    let at_zero = Event {
        volume: own(0.0),
        ..kitchen(Command::MuteToggle, 1.0)
    };
    let decision = remote.decide(&at_zero);
    assert_eq!(decision.action, Action::Nothing);
    assert!(
        decision.line.contains("nothing to restore"),
        "{}",
        decision.line
    );

    // A remote step moved it, then down to 0 again.
    let mut remote = Remote::default();
    remote.decide(&kitchen(Command::MuteToggle, 0.0));
    let up = Event {
        volume: own(0.0),
        ..kitchen(Command::VolumeUp, 1.0)
    };
    assert_eq!(moved(&remote.decide(&up)), Some((0.0, 5.0)));
    let down = Event {
        volume: own(5.0),
        ..kitchen(Command::VolumeDown, 2.0)
    };
    assert_eq!(moved(&remote.decide(&down)), Some((5.0, 0.0)));
    let at_zero = Event {
        volume: own(0.0),
        ..kitchen(Command::MuteToggle, 3.0)
    };
    assert_eq!(remote.decide(&at_zero).action, Action::Nothing);

    // A new session of the speaker remembers nothing.
    let mut remote = Remote::default();
    remote.decide(&kitchen(Command::MuteToggle, 0.0));
    remote.forget(KITCHEN);
    assert_eq!(remote.decide(&at_zero).action, Action::Nothing);
}

// covers: S278
#[test]
fn one_press_passed_on_by_two_members_counts_once() {
    let mut remote = Remote::default();
    let left = remote.decide(&member("Left", Command::VolumeUp, 10.0));
    assert_eq!(moved(&left), Some((40.0, 45.0)));
    let right = remote.decide(&member("Right", Command::VolumeUp, 10.12));
    assert_eq!(right.action, Action::Nothing);
    assert!(
        right
            .line
            .ends_with("ignored, counted once with an earlier press"),
        "{}",
        right.line
    );
    // Another kind in the window counts.
    let mute = remote.decide(&member("Right", Command::MuteToggle, 10.2));
    assert_eq!(moved(&mute), Some((40.0, 0.0)));

    // A transport press counts once whichever speakers pass it on.
    let first = remote.decide(&kitchen(Command::Next, 20.0));
    assert_eq!(first.action, Action::Send(Control::Next));
    let other = Event {
        speaker: "Bedroom",
        row: Some("02005E005302"),
        ..kitchen(Command::Next, 20.25)
    };
    assert_eq!(remote.decide(&other).action, Action::Nothing);
    // Volume presses of speakers outside a target each count.
    let a = remote.decide(&kitchen(Command::VolumeUp, 30.0));
    let b = remote.decide(&kitchen(Command::VolumeUp, 30.1));
    assert!(moved(&a).is_some() && moved(&b).is_some());
}

// covers: S278
#[test]
fn two_presses_300_ms_apart_both_count() {
    let mut remote = Remote::default();
    let first = remote.decide(&member("Left", Command::VolumeDown, 5.0));
    let second = remote.decide(&member("Left", Command::VolumeDown, 5.3));
    assert!(moved(&first).is_some() && moved(&second).is_some());
    let next = remote.decide(&kitchen(Command::PlayPause, 6.0));
    let again = remote.decide(&kitchen(Command::PlayPause, 6.3));
    assert_eq!(next.action, Action::Send(Control::PlayPause));
    assert_eq!(again.action, Action::Send(Control::PlayPause));
}

// covers: S273
#[test]
fn a_command_for_a_speaker_that_stopped_does_nothing() {
    let mut remote = Remote::default();
    for (index, command) in [
        Command::PlayPause,
        Command::Next,
        Command::VolumeUp,
        Command::MuteToggle,
        Command::DeviceVolume(-10.0),
    ]
    .into_iter()
    .enumerate()
    {
        let stopped = Event {
            row: None,
            ..kitchen(command, index as f64)
        };
        let decision = remote.decide(&stopped);
        assert_eq!(decision.action, Action::Nothing);
        assert!(decision.line.ends_with("ignored, speaker not playing"));
    }
    // A stopped speaker's press doesn't hold back the next one.
    assert_eq!(
        remote.decide(&kitchen(Command::PlayPause, 0.1)).action,
        Action::Send(Control::PlayPause)
    );
}

// covers: S279
#[test]
fn every_remote_command_gives_one_log_line() {
    let mut remote = Remote::default();
    let cases = [
        (
            kitchen(Command::PlayPause, 0.0),
            "AirPlay remote Kitchen: PlayPause -> sent to Music as play-pause",
        ),
        (
            kitchen(Command::Play, 1.0),
            "AirPlay remote Kitchen: Play -> nothing sent, Music already plays",
        ),
        (
            kitchen(Command::VolumeUp, 2.0),
            "AirPlay remote Kitchen: VolumeUp -> level 40 -> 45",
        ),
        (
            Event {
                volume: OwnVolume {
                    echo: true,
                    ..own(40.0)
                },
                ..kitchen(Command::DeviceVolume(-18.0), 3.0)
            },
            "AirPlay remote Kitchen: DeviceVolume -18.00 dB -> ignored, echo of the level sent",
        ),
        (
            Event {
                volume: OwnVolume {
                    dragging: true,
                    ..own(40.0)
                },
                ..kitchen(Command::DeviceVolume(-12.0), 4.0)
            },
            "AirPlay remote Kitchen: DeviceVolume -12.00 dB -> ignored, slider dragged",
        ),
        (
            Event {
                volume: OwnVolume {
                    reported: Some(60.0),
                    ..own(40.0)
                },
                ..kitchen(Command::DeviceVolume(-12.0), 5.0)
            },
            "AirPlay remote Kitchen: DeviceVolume -12.00 dB -> level 40 -> 60",
        ),
        (
            Event {
                volume: OwnVolume {
                    reported: Some(0.0),
                    ..own(40.0)
                },
                ..kitchen(Command::DeviceVolume(-144.0), 6.0)
            },
            "AirPlay remote Kitchen: DeviceVolume muted -> level 40 -> 0",
        ),
        (
            Event {
                volume: OwnVolume {
                    reported: Some(40.2),
                    ..own(40.2)
                },
                ..kitchen(Command::DeviceVolume(-17.94), 7.0)
            },
            "AirPlay remote Kitchen: DeviceVolume -17.94 dB -> level stays at 40.2",
        ),
        (
            Event {
                row: None,
                ..kitchen(Command::Next, 8.0)
            },
            "AirPlay remote Kitchen: Next -> ignored, speaker not playing",
        ),
        (
            Event {
                app: None,
                ..kitchen(Command::Previous, 9.0)
            },
            "AirPlay remote Kitchen: Previous -> ignored, no app to control",
        ),
        (
            kitchen(Command::Previous, 9.1),
            "AirPlay remote Kitchen: Previous -> ignored, counted once with an earlier press",
        ),
    ];
    for (event, line) in cases {
        let decision = remote.decide(&event);
        assert_eq!(decision.line, line);
        assert!(!decision.line.contains('\n'));
    }
}
