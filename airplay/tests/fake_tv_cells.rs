//! The verdicts `fake_tv_cells` prints, one line per dry-run cell.

#[path = "../examples/fake_tv/cells.rs"]
mod cells;
mod common;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::pairing::PairingEvent;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;
use cells::{Cell, PasswordStep, Pause, Status, db_for_row, row_for_db};
use common::ap2::Ap2Server;
use common::hap::{item, untlv};
use common::net::{Events, HostAudio, free_ptp_ports, serial};
use television::{Pin, television};

const LONG: Duration = Duration::from_secs(15);

#[test]
fn rows_map_to_the_default_line_and_back() {
    assert_eq!(db_for_row(0), -144.0);
    assert_eq!(db_for_row(20), -24.0);
    assert!((db_for_row(80) + 6.0).abs() < 1e-9);
    assert_eq!(db_for_row(100), 0.0);
    // A single-precision read just under the level still shows its row.
    assert_eq!(row_for_db(-24.000_001), 20);
    assert_eq!(row_for_db(-6.000_001), 80);
    assert_eq!(row_for_db(-26.7), 11);
    assert_eq!(row_for_db(-30.0), 0);
    assert_eq!(row_for_db(-144.0), 0);
    assert_eq!(row_for_db(f64::NAN), 0);
    assert_eq!(row_for_db(0.0), 100);
}

#[test]
fn a_line_is_the_cell_its_status_and_the_value() {
    let cell = Cell::new("D2", Status::Pass, "1 FLUSH".to_owned());
    assert_eq!(cell.line(), "D2        PASS    1 FLUSH");
    let manual = Cell::new("W1", Status::Manual, "on the TV".to_owned());
    assert_eq!(manual.line(), "W1        MANUAL  on the TV");
}

#[test]
fn k1_send_passes_when_every_row_lands_on_its_level() {
    let landed = [(20, Some(-24.0)), (50, Some(-15.0)), (0, Some(-144.0))];
    let cell = cells::k1_send(&landed);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(cell.value, "rows 20/50/0 -> fake at -24.0/-15.0/-144.0 dB");

    let off = cells::k1_send(&[(20, Some(-24.0)), (50, Some(-16.2))]);
    assert_eq!(off.status, Status::Fail);
    assert_eq!(
        off.value,
        "rows 20/50 -> fake at -24.0/-16.2 dB; row 50 shows 46"
    );
    let lost = cells::k1_send(&[(20, None)]);
    assert_eq!(lost.status, Status::Fail);
    assert_eq!(
        lost.value,
        "rows 20 -> fake at none dB; row 20 never reached the fake"
    );
}

#[test]
fn k1_read_passes_when_each_vol_shows_its_row_in_time() {
    let ms = Duration::from_millis;
    let seen = [(20, Some((20, ms(700)))), (100, Some((100, ms(1_100))))];
    let cell = cells::k1_read(&seen);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(
        cell.value,
        "vol rows 20/100 -> reported 20/100, slowest 1.1 s"
    );

    let slow = cells::k1_read(&[(20, Some((20, ms(2_400))))]);
    assert_eq!(slow.status, Status::Fail);
    assert_eq!(
        slow.value,
        "vol rows 20 -> reported 20, slowest 2.4 s; over 1.6 s"
    );
    let missing = cells::k1_read(&[(20, Some((20, ms(500)))), (50, None)]);
    assert_eq!(missing.status, Status::Fail);
    assert_eq!(
        missing.value,
        "vol rows 20/50 -> reported 20/none, slowest 0.5 s; row 50 never reported"
    );
}

#[test]
fn k1_press_passes_when_presses_are_reported_and_nothing_goes_back() {
    let ms = Duration::from_millis;
    let presses = [
        (-15.3, Some((-15.300_001, ms(800)))),
        (-15.6, Some((-15.6, ms(900)))),
    ];
    let cell = cells::k1_press(&presses, 0);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(
        cell.value,
        "2 remote presses -15.3/-15.6 dB reported, slowest 0.9 s; 0 volume sets back"
    );

    let echoed = cells::k1_press(&presses, 1);
    assert_eq!(echoed.status, Status::Fail);
    assert!(
        echoed.value.ends_with("1 volume sets back"),
        "{}",
        echoed.value
    );
    let unseen = cells::k1_press(&[(-15.3, None)], 0);
    assert_eq!(unseen.status, Status::Fail);
    assert!(
        unseen.value.contains("-15.3 dB never reported"),
        "{}",
        unseen.value
    );
}

#[test]
fn d1_passes_only_with_a_pin_at_every_switch_on() {
    let pass = cells::d1(2, 2, 2, 2, 2);
    assert_eq!(pass.status, Status::Pass, "{}", pass.line());
    assert_eq!(
        pass.value,
        "2 switch-ons: 2 PIN prompts, 2 PIN pairings started, 2 done, 2 playing"
    );
    assert_eq!(cells::d1(2, 1, 1, 1, 2).status, Status::Fail);
    assert_eq!(cells::d1(2, 2, 2, 2, 1).status, Status::Fail);
    assert_eq!(
        cells::d1(1, 1, 1, 1, 1).status,
        Status::Fail,
        "one switch-on proves nothing"
    );
}

#[test]
fn d1_once_passes_only_with_one_pin_for_every_switch_on() {
    let pass = cells::d1_once(2, 1, 1, 1, 2);
    assert_eq!(pass.status, Status::Pass, "{}", pass.line());
    assert_eq!(
        pass.value,
        "2 switch-ons: 1 PIN prompts, 1 PIN pairings started, 1 done, 2 playing"
    );
    assert_eq!(cells::d1_once(2, 2, 2, 2, 2).status, Status::Fail);
    assert_eq!(cells::d1_once(2, 1, 1, 1, 1).status, Status::Fail);
    assert_eq!(cells::d1_once(1, 1, 1, 1, 1).status, Status::Fail);
}

/// D1.once run end to end: the sender pairs with the PIN once, then plays
/// twice on the stored credentials.
#[test]
fn d1_once_passes_against_a_television_that_asks_for_a_pin_once() {
    const CODE: &str = "4821";
    const SWITCH_ONS: usize = 2;
    let _serial = serial();
    let server = Ap2Server::start(television(
        "AudioSync Test TV",
        Pin::Once,
        CODE,
        television::FEATURES_METADATA,
        television::DEVICE_ID,
    ));
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: "AudioSync Test TV".to_owned(),
        host: format!("{}.local", television::HOST_LABEL),
        port: server.address.port(),
        txt: television::txt(
            Pin::Once,
            television::FEATURES_METADATA,
            television::DEVICE_ID,
        )
        .into_iter()
        .map(|(key, value)| format!("{key}={value}").into_bytes())
        .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    let key = DeviceKey::Id(DeviceId(television::DEVICE_ID));
    let device = Device::from_records(key.clone(), None, Some(&record)).expect("its record");
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ptp_ports: free_ptp_ports(),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let _audio = HostAudio::start(writer);
    let mut events = Events::default();
    let prompt = |event: &SenderEvent| {
        matches!(
            event,
            SenderEvent::Pairing {
                event: PairingEvent::PinRequested,
                ..
            }
        )
    };

    sender.start_pairing(device.clone());
    assert!(
        events.wait_for(&sender, LONG, prompt).is_some(),
        "no PIN prompt"
    );
    sender.submit_pin(&key, CODE).expect("digits");
    let paired = events.wait_for(&sender, LONG, |event| {
        matches!(
            event,
            SenderEvent::Pairing {
                event: PairingEvent::HomeKitPaired(_),
                ..
            }
        )
    });
    let Some(SenderEvent::Pairing {
        event: PairingEvent::HomeKitPaired(credentials),
        ..
    }) = paired
    else {
        panic!("no credentials: {:?}", server.fake().keys());
    };
    for on in 1..=SWITCH_ONS {
        let mut options = SpeakerOptions::new(Volume::Db(-20.0));
        options.homekit_credentials = Some(credentials.clone());
        sender.add_speaker(device.clone(), options);
        let playing = |state: &SpeakerState| *state == SpeakerState::Playing;
        assert!(
            events
                .wait_state(&sender, &key, on, LONG, playing)
                .is_some(),
            "{:?} {:?}",
            events.of(&key),
            server.fake().keys()
        );
        sender.remove_speaker(&key);
        let stopped = |state: &SpeakerState| *state == SpeakerState::Stopped;
        assert!(
            events
                .wait_state(&sender, &key, on, LONG, stopped)
                .is_some()
        );
    }

    let mut pairings = (0, 0);
    for (request, _) in &server.fake().requests {
        if request.method != Method::Post || request.uri != "/pair-setup" {
            continue;
        }
        let items = untlv(&request.body);
        match item(&items, 6).and_then(|state| state.first().copied()) {
            Some(1) if item(&items, 19).is_none() => pairings.0 += 1,
            Some(5) => pairings.1 += 1,
            _ => {}
        }
    }
    let prompts = events
        .seen
        .iter()
        .filter(|(_, event)| prompt(event))
        .count();
    let playing = events
        .of(&key)
        .iter()
        .filter(|event| matches!(event, SpeakerEvent::State(SpeakerState::Playing)))
        .count();
    let cell = cells::d1_once(SWITCH_ONS, prompts, pairings.0, pairings.1, playing);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    sender.shutdown();
}

#[test]
fn pw_cells_pass_on_the_password_path_and_fail_off_it() {
    let none = PasswordStep {
        error: Some(ErrorKind::PasswordRequired),
        ..PasswordStep::default()
    };
    let wrong = PasswordStep {
        error: Some(ErrorKind::WrongPassword),
        pair_setups: 2,
        started: 1,
        ..PasswordStep::default()
    };
    let right = PasswordStep {
        playing: true,
        paired: true,
        pair_setups: 3,
        started: 1,
        done: 1,
        verifies: 2,
        ..PasswordStep::default()
    };
    let again = PasswordStep {
        playing: true,
        verifies: 2,
        ..PasswordStep::default()
    };
    let cells = cells::pw(none, wrong, right, again);
    for cell in &cells {
        assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    }
    assert_eq!(
        cells[0].line(),
        "PW.none   PASS    error PasswordRequired, 0 pair-setup (0 M1, 0 M5), 0 pin-start, 0 pair-verify"
    );

    let pin = PasswordStep {
        pin_starts: 1,
        ..right
    };
    let paired_again = PasswordStep {
        pair_setups: 3,
        ..again
    };
    let no_prompt = PasswordStep {
        error: None,
        playing: true,
        ..none
    };
    let cells = cells::pw(no_prompt, right, pin, paired_again);
    for cell in &cells {
        assert_eq!(cell.status, Status::Fail, "{}", cell.line());
    }
    assert!(cells[2].value.ends_with("a PIN pairing was started"));
    assert!(cells[3].value.ends_with("paired again"));
}

#[test]
fn d2_passes_with_one_flush_and_nothing_else() {
    let clean = Pause {
        flushes: 1,
        teardowns: 0,
        setups: 0,
        records: 0,
        resumed: true,
    };
    let cell = cells::d2(clean);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(
        cell.value,
        "pause: 1 FLUSH, 0 TEARDOWN; resume: 0 SETUP, 0 RECORD, audio again"
    );
    for broken in [
        Pause {
            flushes: 2,
            ..clean
        },
        Pause {
            flushes: 0,
            ..clean
        },
        Pause {
            teardowns: 1,
            ..clean
        },
        Pause { setups: 1, ..clean },
        Pause {
            records: 1,
            ..clean
        },
        Pause {
            resumed: false,
            ..clean
        },
    ] {
        assert_eq!(
            cells::d2(broken).status,
            Status::Fail,
            "{}",
            cells::d2(broken).value
        );
    }
    assert!(
        cells::d2(Pause {
            resumed: false,
            ..clean
        })
        .value
        .ends_with("no audio")
    );
}

#[test]
fn d3_passes_when_track_changes_and_a_seek_reach_the_tv_without_a_flush() {
    let cell = cells::d3_track(3, 6, 3, 0);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(
        cell.value,
        "3 track info changes (2 tracks, 1 seek): 6 metadata sets, 3 track text, 0 FLUSH"
    );
    assert_eq!(cells::d3_track(3, 6, 3, 1).status, Status::Fail);
}

#[test]
fn d3_fails_when_the_track_info_never_reaches_the_tv() {
    let none = cells::d3_track(3, 0, 0, 0);
    assert_eq!(none.status, Status::Fail, "{}", none.line());
    assert_eq!(
        none.value,
        "3 track info changes (2 tracks, 1 seek): 0 metadata sets, 0 track text, 0 FLUSH; fewer sets than changes"
    );
    assert_eq!(cells::d3_track(3, 2, 2, 0).status, Status::Fail);
}

#[test]
fn d3_fails_when_only_progress_reaches_the_tv_without_the_track_text() {
    let progress_only = cells::d3_track(3, 3, 0, 0);
    assert_eq!(
        progress_only.status,
        Status::Fail,
        "{}",
        progress_only.line()
    );
    assert_eq!(
        progress_only.value,
        "3 track info changes (2 tracks, 1 seek): 3 metadata sets, 0 track text, 0 FLUSH; fewer track text sets than changes"
    );
    assert_eq!(cells::d3_track(3, 5, 2, 0).status, Status::Fail);
}

#[test]
fn d4_says_how_long_it_paused_against_the_eleven_minutes() {
    let clean = Pause {
        flushes: 1,
        teardowns: 0,
        setups: 0,
        records: 0,
        resumed: true,
    };
    let full = cells::d4(Duration::from_secs(660), clean);
    assert_eq!(full.status, Status::Pass, "{}", full.line());
    assert_eq!(
        full.value,
        "paused 11.0 min: 1 FLUSH, 0 TEARDOWN, 0 SETUP, 0 RECORD, audio again"
    );
    let short = cells::d4(Duration::from_secs(60), clean);
    assert_eq!(short.status, Status::Pass);
    assert_eq!(
        short.value,
        "paused 1.0 min (short run, the cell asks 11): 1 FLUSH, 0 TEARDOWN, 0 SETUP, 0 RECORD, audio again"
    );
    let dropped = cells::d4(
        Duration::from_secs(660),
        Pause {
            teardowns: 1,
            ..clean
        },
    );
    assert_eq!(dropped.status, Status::Fail);
}

#[test]
fn manual_cells_name_their_step() {
    let manual = cells::manual();
    let ids: Vec<&str> = manual.iter().map(|cell| cell.id).collect();
    assert_eq!(
        ids,
        [
            "K1.row", "K2", "K3", "D3", "D4.cover", "D5", "D6", "W1", "W2", "W3", "W4", "W5", "W6"
        ]
    );
    assert!(manual.iter().all(|cell| cell.status == Status::Manual));
    assert!(manual.iter().all(|cell| !cell.value.is_empty()));
}
