//! The AirPlay 2 volume against the sans-I/O fake receiver: where the
//! initial volume goes and its one resend (P183), and the volume reads
//! (P122, P184).

mod common;

use std::time::{Duration, Instant};

use audiosync_airplay::ap2::{Ap2Config, Ap2Event};
use audiosync_airplay::error::Warning;
use audiosync_airplay::metadata::{Metadata, TrackInfo};
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Rig, FakeAp2, ap2_config, dict, int, plist};

const SET: &str = "SET_PARAMETER";
const READ: &str = "GET_PARAMETER";
const FEEDBACK: &str = "POST /feedback";

fn reading_config() -> Ap2Config {
    let mut config = ap2_config();
    config.read_volume = true;
    config
}

fn answering(level: &str) -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.bodies
        .insert(READ.to_owned(), format!("volume: {level}\r\n").into_bytes());
    fake
}

fn with_text(mut fake: FakeAp2) -> FakeAp2 {
    fake.info_body = plist(dict(&[(
        "features",
        int(0x4_0000_0000 | 0x1_0000_0000_0000 | (1 << 17)),
    )]));
    fake
}

/// The bodies of the volume SET_PARAMETERs, in order.
fn volume_sets(rig: &Ap2Rig) -> Vec<String> {
    rig.fake
        .requests
        .iter()
        .map(|(request, _)| request)
        .filter(|request| {
            request.method == Method::SetParameter
                && request.body.starts_with(b"volume:")
                && request.headers.get("Content-Type") == Some("text/parameters")
        })
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect()
}

fn reported(rig: &Ap2Rig) -> Vec<f64> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            Ap2Event::VolumeReported(db) => Some(*db),
            _ => None,
        })
        .collect()
}

fn warnings(rig: &Ap2Rig) -> Vec<&Warning> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            Ap2Event::Warning(warning) => Some(warning),
            _ => None,
        })
        .collect()
}

/// Asks for a read every 100 ms for `by`.
fn ask_for(rig: &mut Ap2Rig, by: Duration) {
    let end = rig.now + by;
    while rig.now < end {
        let now = rig.now;
        rig.session.read_volume(now);
        rig.pump();
        rig.advance(Duration::from_millis(100));
    }
}

fn since(times: &[Instant], start: Instant) -> Vec<Duration> {
    times.iter().map(|at| *at - start).collect()
}

// covers: P42, P119, P183
#[test]
fn the_initial_volume_waits_for_the_flush_answer_and_goes_ahead_of_metadata() {
    let mut fake = with_text(FakeAp2::new());
    fake.hold = Some("FLUSH".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert_eq!(rig.keys().last(), Some(&"FLUSH"), "{:?}", rig.keys());
    assert!(volume_sets(&rig).is_empty(), "{:?}", rig.keys());

    let now = rig.now;
    rig.session.set_volume(Volume::Db(-30.0), now);
    rig.session.set_volume(Volume::Db(-25.0), now);
    let track = Metadata::Track {
        info: TrackInfo {
            title: "Title".to_owned(),
            artist: "Artist".to_owned(),
            album: "Album".to_owned(),
        },
        rtptime: 1000,
    };
    assert_eq!(rig.session.send_metadata(track, now), Ok(()));
    rig.pump();
    assert!(volume_sets(&rig).is_empty(), "{:?}", rig.keys());

    rig.release();
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    let keys = rig.keys();
    let flush = keys.iter().position(|key| *key == "FLUSH").expect("FLUSH");
    assert_eq!(keys[flush + 1], SET, "{keys:?}");
    assert_eq!(volume_sets(&rig), ["volume: -25.000000\r\n"]);
    // The text SET_PARAMETER follows the volume.
    assert_eq!(rig.times_of(SET).len(), 2, "{keys:?}");
    assert!(rig.failure().is_none());
}

// covers: P183
#[test]
fn a_refused_volume_warns_once_and_goes_once_more_with_the_next_feedback() {
    let mut fake = FakeAp2::new();
    fake.status.insert(SET.to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    let start = rig.now;
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    assert_eq!(volume_sets(&rig), ["volume: -15.000000\r\n"]);

    rig.advance(Duration::from_secs(12));
    assert_eq!(
        volume_sets(&rig),
        ["volume: -15.000000\r\n", "volume: -15.000000\r\n"]
    );
    let sets = rig.times_of(SET);
    let feedback = rig.times_of(FEEDBACK);
    assert_eq!(since(&sets, start)[1], Duration::from_secs(2));
    assert_eq!(feedback.first(), sets.get(1));
    assert_eq!(
        warnings(&rig),
        [&Warning::RequestFailed {
            method: Method::SetParameter,
            status: 500
        }]
    );
    assert!(rig.failure().is_none(), "{:?}", rig.events);
    assert!(rig.ready());

    // A new value from the host goes, and gets its own one resend.
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-10.0), now);
    rig.pump();
    rig.advance(Duration::from_secs(10));
    assert_eq!(
        volume_sets(&rig)[2..],
        ["volume: -10.000000\r\n", "volume: -10.000000\r\n"]
    );
    assert_eq!(warnings(&rig).len(), 1);
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P183
#[test]
fn an_accepted_resend_leaves_the_volume_alone_after_it() {
    let mut fake = FakeAp2::new();
    fake.status_once.insert(SET.to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    rig.advance(Duration::from_secs(10));
    assert_eq!(
        volume_sets(&rig),
        ["volume: -15.000000\r\n", "volume: -15.000000\r\n"]
    );
    assert!(rig.failure().is_none());
}

// covers: P183
#[test]
fn a_refused_volume_the_read_back_shows_applied_neither_warns_nor_goes_again() {
    let mut fake = FakeAp2::new();
    fake.status.insert(SET.to_owned(), 500);
    fake.applies_volume = Some(0.6);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    for level in [-28.110378, -25.748351] {
        let now = rig.now;
        rig.session.set_volume(Volume::Db(level), now);
        rig.pump();
        rig.advance(Duration::from_secs(10));
    }
    assert_eq!(
        volume_sets(&rig),
        [
            "volume: -15.000000\r\n",
            "volume: -28.110378\r\n",
            "volume: -25.748351\r\n"
        ]
    );
    assert_eq!(rig.times_of(READ).len(), 3, "{:?}", rig.keys());
    assert!(warnings(&rig).is_empty(), "{:?}", rig.events);
    // The host never asked for the level.
    assert!(reported(&rig).is_empty());
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P183
#[test]
fn a_refused_volume_the_read_back_shows_unapplied_warns_and_goes_once_more() {
    let mut fake = answering("-20.000000");
    fake.status.insert(SET.to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    rig.advance(Duration::from_secs(10));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-10.0), now);
    rig.pump();
    rig.advance(Duration::from_secs(10));
    assert_eq!(
        volume_sets(&rig),
        [
            "volume: -15.000000\r\n",
            "volume: -15.000000\r\n",
            "volume: -10.000000\r\n",
            "volume: -10.000000\r\n"
        ]
    );
    assert!(rig.keys().contains(&READ), "{:?}", rig.keys());
    assert_eq!(
        warnings(&rig),
        [&Warning::RequestFailed {
            method: Method::SetParameter,
            status: 500
        }]
    );
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P183
#[test]
fn a_level_kept_from_the_speaker_is_never_sent_and_ends_a_pending_resend() {
    let mut fake = answering("-20.000000");
    fake.status.insert(SET.to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    rig.advance(Duration::from_secs(10));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-10.0), now);
    rig.pump();
    // The user set −20 dB on the speaker before the check read came back.
    rig.session.keep_volume(Volume::Db(-20.0));
    rig.advance(Duration::from_secs(10));
    let sets = volume_sets(&rig);
    let count = |body: &str| sets.iter().filter(|set| *set == body).count();
    assert_eq!(count("volume: -10.000000\r\n"), 1, "{sets:?}");
    assert_eq!(count("volume: -20.000000\r\n"), 0, "{sets:?}");
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P183
#[test]
fn a_level_kept_from_the_speaker_drops_a_resend_already_queued() {
    let mut fake = answering("-20.000000");
    fake.status.insert(SET.to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    rig.advance(Duration::from_secs(10));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-10.0), now);
    rig.pump();
    // The read showed the set unapplied: the resend queues behind a held
    // feedback, and the user's level on the speaker arrives before it goes.
    rig.fake.hold = Some(FEEDBACK.to_owned());
    while rig.keys().last() != Some(&FEEDBACK) {
        rig.advance(Duration::from_millis(100));
    }
    rig.session.keep_volume(Volume::Db(-20.0));
    rig.release();
    rig.advance(Duration::from_secs(10));
    let sets = volume_sets(&rig);
    let count = |body: &str| sets.iter().filter(|set| *set == body).count();
    assert_eq!(count("volume: -10.000000\r\n"), 1, "{sets:?}");
    assert_eq!(count("volume: -20.000000\r\n"), 0, "{sets:?}");
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P183
#[test]
fn a_level_kept_from_the_speaker_while_the_resend_is_unanswered_is_never_sent() {
    let mut fake = answering("-20.000000");
    fake.status.insert(SET.to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    rig.advance(Duration::from_secs(10));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-10.0), now);
    rig.pump();
    // The resend goes out and its refusal is held back until the user's
    // level on the speaker has arrived.
    rig.fake.hold = Some(SET.to_owned());
    let resent = |rig: &Ap2Rig| {
        volume_sets(rig)
            .iter()
            .filter(|set| *set == "volume: -10.000000\r\n")
            .count()
    };
    let end = rig.now + Duration::from_secs(10);
    while resent(&rig) < 2 && rig.now < end {
        rig.advance(Duration::from_millis(100));
    }
    assert_eq!(resent(&rig), 2, "{:?}", rig.keys());
    rig.session.keep_volume(Volume::Db(-20.0));
    rig.release();
    rig.advance(Duration::from_secs(10));
    let sets = volume_sets(&rig);
    let count = |body: &str| sets.iter().filter(|set| *set == body).count();
    assert_eq!(count("volume: -10.000000\r\n"), 2, "{sets:?}");
    assert_eq!(count("volume: -20.000000\r\n"), 0, "{sets:?}");
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P183, P184
#[test]
fn reads_stopped_by_refused_volumes_warn_at_the_hosts_first_ask() {
    let mut fake = FakeAp2::new();
    fake.status.insert(SET.to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    rig.advance(Duration::from_secs(4));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-10.0), now);
    rig.pump();
    rig.advance(Duration::from_secs(4));
    // Three failed checks stopped the reads before the host asked for one.
    assert_eq!(rig.times_of(READ).len(), 3, "{:?}", rig.keys());
    assert_eq!(warnings(&rig).len(), 1, "{:?}", rig.events);

    ask_for(&mut rig, Duration::from_secs(4));
    assert_eq!(rig.times_of(READ).len(), 3, "{:?}", rig.keys());
    assert_eq!(
        warnings(&rig),
        [
            &Warning::RequestFailed {
                method: Method::SetParameter,
                status: 500
            },
            &Warning::VolumeReadsStopped
        ]
    );
    assert!(reported(&rig).is_empty());
}

// covers: P183, P184
#[test]
fn a_check_read_sent_just_before_a_read_tick_gets_a_full_interval() {
    let mut fake = FakeAp2::new();
    fake.status.insert(SET.to_owned(), 500);
    fake.applies_volume = Some(0.6);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    // Read ticks fall on whole seconds after the stream became ready.
    let ready = rig.times_of(SET)[0];
    rig.advance((ready + Duration::from_millis(2900)) - rig.now);

    rig.fake.hold = Some(READ.to_owned());
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-20.0), now);
    rig.pump();
    assert_eq!(rig.times_of(READ).len(), 2, "{:?}", rig.keys());
    rig.advance(Duration::from_millis(500));
    rig.release();
    rig.advance(Duration::from_secs(8));
    assert_eq!(
        volume_sets(&rig),
        ["volume: -15.000000\r\n", "volume: -20.000000\r\n"]
    );
    assert!(warnings(&rig).is_empty(), "{:?}", rig.events);
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P183, P184
#[test]
fn a_check_read_never_answered_fails_once_and_never_ends_the_session() {
    let mut fake = FakeAp2::new();
    fake.status.insert(SET.to_owned(), 500);
    fake.applies_volume = Some(0.6);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    rig.advance(Duration::from_secs(1));

    rig.fake.silent.push(READ.to_owned());
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-20.0), now);
    rig.pump();
    rig.advance(Duration::from_secs(10));
    // Each unanswered check read failed once: one resend, then nothing.
    assert_eq!(
        volume_sets(&rig),
        [
            "volume: -15.000000\r\n",
            "volume: -20.000000\r\n",
            "volume: -20.000000\r\n"
        ]
    );
    assert_eq!(rig.times_of(READ).len(), 3, "{:?}", rig.keys());
    assert_eq!(
        warnings(&rig),
        [&Warning::RequestFailed {
            method: Method::SetParameter,
            status: 500
        }]
    );
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P122, P184
#[test]
fn reads_once_after_the_initial_volume_then_only_when_asked_once_a_second() {
    let mut rig = Ap2Rig::new(reading_config(), answering("-27.000000"));
    let start = rig.now;
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    let keys = rig.keys();
    let set = keys.iter().position(|key| *key == SET).expect("volume");
    assert_eq!(keys[set + 1], READ, "{keys:?}");
    let read = rig.fake.last(READ).expect("read");
    assert_eq!(read.body, b"volume\r\n");
    assert_eq!(read.headers.get("Content-Type"), Some("text/parameters"));
    assert_eq!(reported(&rig), [-27.0]);

    // Nobody asks: no reads.
    rig.advance(Duration::from_secs(9));
    assert_eq!(rig.times_of(READ).len(), 1, "{:?}", rig.keys());

    // Asked all the time: one read on each 1 s read tick.
    rig.fake
        .bodies
        .insert(READ.to_owned(), b"volume: -26.700001\r\n".to_vec());
    ask_for(&mut rig, Duration::from_secs(9));
    let ready = rig.times_of(SET)[0];
    let reads = since(&rig.times_of(READ), ready);
    assert!(reads.len() >= 9, "{reads:?}");
    for pair in reads[1..].windows(2) {
        assert_eq!(pair[1] - pair[0], Duration::from_secs(1), "{reads:?}");
    }
    for at in &reads[1..] {
        assert_eq!(at.as_millis() % 1000, 0, "{reads:?}");
    }
    assert!(since(&rig.times_of(READ), start)[0] < Duration::from_secs(1));
    assert_eq!(reported(&rig)[1], -26.700001);
    // Host requests while reads go on warn of nothing.
    assert!(warnings(&rig).is_empty(), "{:?}", rig.events);
    assert!(rig.failure().is_none());
}

// covers: P122
#[test]
fn a_read_answer_with_an_empty_line_before_the_level_counts() {
    let mut fake = FakeAp2::new();
    fake.bodies
        .insert(READ.to_owned(), b"\r\nvolume: -26.400000\r\n".to_vec());
    let rig = Ap2Rig::new(reading_config(), fake);
    assert_eq!(reported(&rig), [-26.4]);
    assert_eq!(volume_sets(&rig), ["volume: -15.000000\r\n"]);
}

// covers: P120, P184
#[test]
fn a_read_waits_behind_a_queued_set_and_a_read_older_than_the_last_set_is_dropped() {
    let mut rig = Ap2Rig::new(reading_config(), answering("-27.000000"));
    assert_eq!(reported(&rig), [-27.0]);

    // A set queued behind the feedback goes before the read.
    rig.fake.hold = Some(FEEDBACK.to_owned());
    rig.advance(Duration::from_secs(2));
    assert_eq!(rig.keys().last(), Some(&FEEDBACK));
    let now = rig.now;
    rig.session.read_volume(now);
    rig.advance(Duration::from_millis(1100));
    assert_eq!(rig.keys().last(), Some(&FEEDBACK));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-12.0), now);
    rig.pump();
    rig.release();
    let keys = rig.keys();
    assert_eq!(keys[keys.len() - 2..], [SET, READ], "{keys:?}");
    assert_eq!(reported(&rig), [-27.0, -27.0]);

    // A set made while a read is out makes its answer stale.
    rig.fake.hold = Some(READ.to_owned());
    let reads = rig.times_of(READ).len();
    let now = rig.now;
    rig.session.read_volume(now);
    while rig.times_of(READ).len() == reads {
        rig.advance(Duration::from_millis(100));
    }
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-9.0), now);
    rig.pump();
    rig.release();
    assert_eq!(reported(&rig), [-27.0, -27.0]);
    assert_eq!(
        volume_sets(&rig).last().map(String::as_str),
        Some("volume: -9.000000\r\n")
    );
    assert!(rig.failure().is_none());
}

// covers: P184
#[test]
fn three_failed_reads_stop_reading_with_one_warning_and_the_session_plays_on() {
    for body in [None, Some(b"\r\n".to_vec())] {
        let mut fake = FakeAp2::new();
        match body {
            None => {
                fake.status.insert(READ.to_owned(), 500);
            }
            Some(body) => {
                fake.bodies.insert(READ.to_owned(), body);
            }
        }
        let mut rig = Ap2Rig::new(reading_config(), fake);
        ask_for(&mut rig, Duration::from_secs(20));
        assert_eq!(rig.times_of(READ).len(), 3, "{:?}", rig.keys());
        assert_eq!(warnings(&rig), [&Warning::VolumeReadsStopped]);
        assert!(reported(&rig).is_empty());
        assert!(rig.failure().is_none(), "{:?}", rig.events);
        assert!(rig.times_of(FEEDBACK).len() >= 9);
        assert_eq!(volume_sets(&rig), ["volume: -15.000000\r\n"]);
    }
}

// covers: P184
#[test]
fn a_read_left_unanswered_fails_at_the_next_read_tick_and_never_ends_the_session() {
    let mut fake = FakeAp2::new();
    fake.silent.push(READ.to_owned());
    let mut rig = Ap2Rig::new(reading_config(), fake);
    ask_for(&mut rig, Duration::from_secs(30));
    let reads = rig.times_of(READ);
    assert_eq!(reads.len(), 3, "{:?}", rig.keys());
    for pair in reads[1..].windows(2) {
        assert_eq!(pair[1] - pair[0], Duration::from_secs(1), "{reads:?}");
    }
    assert_eq!(warnings(&rig), [&Warning::VolumeReadsStopped]);
    assert!(rig.failure().is_none(), "{:?}", rig.events);
    let feedback = rig.times_of(FEEDBACK);
    let recent = rig.now - Duration::from_secs(3);
    assert!(
        feedback.last().is_some_and(|at| *at > recent),
        "{feedback:?}"
    );
}

// covers: P184
#[test]
fn reads_given_up_are_forgotten_once_a_later_request_is_answered() {
    let mut fake = FakeAp2::new();
    fake.silent.push(READ.to_owned());
    let mut rig = Ap2Rig::new(reading_config(), fake);
    for _ in 0..2 {
        ask_for(&mut rig, Duration::from_secs(2));
        // The read given up at the tick, then the feedback answered.
        assert_eq!(rig.session.late_answers(), 0, "{:?}", rig.keys());
    }
    assert_eq!(rig.times_of(READ).len(), 3, "{:?}", rig.keys());
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P184
#[test]
fn a_failed_read_leaves_the_feedback_loss_count_alone() {
    let mut fake = FakeAp2::new();
    fake.status.insert(READ.to_owned(), 500);
    fake.status_once.insert(FEEDBACK.to_owned(), 500);
    let mut rig = Ap2Rig::new(reading_config(), fake);
    ask_for(&mut rig, Duration::from_secs(5));
    rig.fake.status_once.insert(FEEDBACK.to_owned(), 500);
    ask_for(&mut rig, Duration::from_secs(10));
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P119, P183, P184
#[test]
fn a_receiver_that_refuses_and_applies_every_set_shows_each_level_once_a_second() {
    let mut fake = FakeAp2::new();
    fake.status.insert(SET.to_owned(), 500);
    fake.applies_volume = Some(0.6);
    fake.second_info_body = Some(plist(dict(&[(
        "initialVolume",
        plist::Value::Real(-28.200001),
    )])));
    let mut rig = Ap2Rig::new(reading_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-28.110378), now);
    rig.pump();
    ask_for(&mut rig, Duration::from_secs(5));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-25.748351), now);
    rig.pump();
    ask_for(&mut rig, Duration::from_secs(5));
    assert_eq!(
        volume_sets(&rig),
        [
            "volume: -15.000000\r\n",
            "volume: -28.110378\r\n",
            "volume: -25.748351\r\n"
        ]
    );
    let levels = reported(&rig);
    assert!(levels.len() >= 9, "{levels:?}");
    assert!(
        levels.iter().any(|db| (db + 28.2).abs() < 0.001),
        "{levels:?}"
    );
    assert!((levels.last().copied().unwrap_or_default() + 25.8).abs() < 0.001);
    assert!(warnings(&rig).is_empty(), "{:?}", rig.events);
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P184
#[test]
fn a_session_the_host_never_asked_sends_no_read() {
    let mut rig = Ap2Rig::new(ap2_config(), answering("-27.000000"));
    assert!(rig.ready());
    rig.advance(Duration::from_secs(9));
    assert!(!rig.keys().contains(&READ), "{:?}", rig.keys());
    assert!(reported(&rig).is_empty());
}
