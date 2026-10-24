//! The MPRIS reader against fake players on a real session bus. Run them
//! under `dbus-run-session -- cargo test`, which gives them a private bus.
//! Each test lists only the players under its own name prefix, so tests
//! run side by side, and players already on a desktop's bus don't count.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

use super::bus::{Bus, OBJECT_PATH, Players, field};
use super::fields::Field;
use super::read_media_sessions;
use crate::audio::media_sessions::{SessionList, spawn};
use crate::audio::now_playing::{Follow, TrackReader};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{CardSource, MediaControls, Transport};

struct FakePlayer {
    status: &'static str,
    title: &'static str,
    art_url: Option<String>,
    can_go_next: bool,
    nexts: Arc<AtomicUsize>,
}

fn owned(value: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(value.into()).expect("no file descriptor in it")
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.status.to_owned()
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let mut metadata = HashMap::new();
        metadata.insert("xesam:title".to_owned(), owned(self.title));
        metadata.insert(
            "xesam:artist".to_owned(),
            owned(vec!["Artist One".to_owned(), "Artist Two".to_owned()]),
        );
        metadata.insert("mpris:length".to_owned(), owned(200_000_000_i64));
        if let Some(url) = &self.art_url {
            metadata.insert("mpris:artUrl".to_owned(), owned(url.clone()));
        }
        metadata
    }

    #[zbus(property)]
    fn position(&self) -> i64 {
        50_000_000
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.can_go_next
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    fn next(&self) {
        self.nexts.fetch_add(1, Ordering::SeqCst);
    }
}

struct FakeRoot;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl FakeRoot {
    #[zbus(property)]
    fn identity(&self) -> String {
        "AudioSync test player".to_owned()
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        "audiosynctestapp".to_owned()
    }
}

fn player(status: &'static str, title: &'static str) -> FakePlayer {
    FakePlayer {
        status,
        title,
        art_url: None,
        can_go_next: true,
        nexts: Arc::default(),
    }
}

/// The name prefix of one test's players.
fn prefix(test: &str) -> String {
    format!("org.mpris.MediaPlayer2.audiosynctest_{test}.")
}

/// Serves `player` on the session bus as `name` until dropped.
fn serve(name: &str, player: FakePlayer) -> Connection {
    zbus::blocking::connection::Builder::session()
        .expect("a session bus: run the tests under dbus-run-session")
        .name(name)
        .unwrap()
        .serve_at(OBJECT_PATH, player)
        .unwrap()
        .serve_at(OBJECT_PATH, FakeRoot)
        .unwrap()
        .build()
        .expect("the fake player is on the bus")
}

fn players(test: &str) -> Players {
    Players::new(&prefix(test), Bus::Session)
}

fn titles(players: &mut Players) -> Vec<(String, String)> {
    players
        .list()
        .unwrap()
        .into_iter()
        .map(|(session, _)| (session.app_id, session.title))
        .collect()
}

fn picked(players: &mut Players, follow: &Follow) -> Option<String> {
    let listed = players.list().unwrap();
    players
        .pick(&listed, follow)
        .map(|index| listed[index].0.app_id.clone())
}

fn icon_url() -> String {
    format!(
        "file://{}/res/audiosync-256.png",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn a_playing_fake_shows_its_track_and_artwork_in_both_readers() {
    let name = format!("{}a", prefix("playing"));
    let _fake = serve(
        &name,
        FakePlayer {
            art_url: Some(icon_url()),
            ..player("Playing", "Song")
        },
    );
    let poll = Duration::from_millis(50);
    let (mut track, mut card) = spawn(|| Ok(players("playing")), poll, DebugLog::default());
    track.follow(Some(Follow::AnyApp));
    card.follow(Some(CardSource::AnyApp));
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut read, mut shown) = (None, None);
    while read.is_none() || shown.is_none() {
        assert!(Instant::now() < deadline, "no reading");
        read = read.or_else(|| track.take_reading().and_then(|reading| reading.track));
        shown = shown.or_else(|| card.take_reading().flatten());
        thread::sleep(Duration::from_millis(10));
    }
    let (read, shown) = (read.unwrap(), shown.unwrap());
    assert_eq!(read.session.app_id, name);
    assert_eq!(read.session.title, "Song");
    assert_eq!(read.session.artist, "Artist One, Artist Two");
    assert!(read.session.playing);
    let jpeg = read
        .artwork
        .expect("the file:// artwork reaches the receivers");
    assert!(jpeg.starts_with(&[0xFF, 0xD8]));
    let timeline = read.timeline.expect("a track with a length has a timeline");
    assert_eq!(timeline.length, Duration::from_secs(200));
    assert_eq!(shown.title, "Song");
    assert!(
        shown.artwork.is_some(),
        "the file:// artwork reaches the card"
    );
}

#[test]
fn of_two_paused_fakes_the_first_by_name_counts() {
    let base = prefix("paused");
    let _b = serve(&format!("{base}b"), player("Paused", "Second"));
    let _a = serve(&format!("{base}a"), player("Paused", "First"));
    let mut players = players("paused");
    assert_eq!(
        titles(&mut players),
        vec![
            (format!("{base}a"), "First".to_owned()),
            (format!("{base}b"), "Second".to_owned()),
        ]
    );
    assert_eq!(
        picked(&mut players, &Follow::AnyApp),
        Some(format!("{base}a"))
    );
}

#[test]
fn a_stopped_fake_has_no_track() {
    let _fake = serve(
        &format!("{}a", prefix("stopped")),
        player("Stopped", "Song"),
    );
    let mut players = players("stopped");
    assert_eq!(titles(&mut players), vec![]);
    assert_eq!(picked(&mut players, &Follow::AnyApp), None);
}

#[test]
fn a_fake_that_exits_drops_out() {
    let fake = serve(&format!("{}a", prefix("exits")), player("Playing", "Song"));
    let mut players = players("exits");
    assert_eq!(titles(&mut players).len(), 1);
    fake.close().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !titles(&mut players).is_empty() {
        assert!(Instant::now() < deadline, "the player is still listed");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn next_reaches_the_fake_only_when_it_can_go_next() {
    let base = prefix("next");
    let able = player("Playing", "Song");
    let able_nexts = able.nexts.clone();
    let _able = serve(&format!("{base}a"), able);
    let unable = FakePlayer {
        can_go_next: false,
        ..player("Paused", "Other")
    };
    let unable_nexts = unable.nexts.clone();
    let _unable = serve(&format!("{base}b"), unable);
    let mut players = players("next");
    let listed = players.list().unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    assert_eq!(
        players.send(&listed[0].1, Transport::Next, deadline),
        Ok(true)
    );
    assert_eq!(able_nexts.load(Ordering::SeqCst), 1);
    assert_eq!(
        players.send(&listed[1].1, Transport::Next, deadline),
        Ok(false)
    );
    assert_eq!(unable_nexts.load(Ordering::SeqCst), 0);
}

#[test]
fn one_app_counts_by_its_process_and_its_parents_then_by_name() {
    let base = prefix("owner");
    let _fake = serve(&format!("{base}a"), player("Paused", "Song"));
    let mut players = players("owner");
    let app = |executable: &str, pid: Option<u32>| Follow::App {
        executable: executable.to_owned(),
        pid,
    };
    let ours = Some(format!("{base}a"));
    assert_eq!(
        picked(&mut players, &app("x", Some(std::process::id()))),
        ours
    );
    // A child of the player's process plays: the player is its parent's.
    let mut child = Command::new("sleep").arg("10").spawn().unwrap();
    let from_child = picked(&mut players, &app("x", Some(child.id())));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(from_child, ours);
    // Another process tree: only the desktop entry can match.
    assert_eq!(
        picked(&mut players, &app("audiosynctestapp", Some(1))),
        ours
    );
    assert_eq!(picked(&mut players, &app("other", Some(1))), None);
    assert_eq!(picked(&mut players, &app("", None)), None);
}

#[test]
fn without_a_bus_every_reading_is_no_track() {
    let missing = Bus::At("unix:path=/nonexistent/audiosync-test-bus".to_owned());
    let mut players = Players::new(&prefix("nobus"), missing);
    assert!(players.list().is_err());
    let (mut track, _card) = spawn(
        move || Ok(players),
        Duration::from_millis(20),
        DebugLog::default(),
    );
    track.follow(Some(Follow::AnyApp));
    let deadline = Instant::now() + Duration::from_secs(10);
    let reading = loop {
        assert!(Instant::now() < deadline, "no reading");
        if let Some(reading) = track.take_reading() {
            break reading;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(reading.track, None);
}

#[test]
fn any_integer_type_reads_as_an_integer() {
    let integers = [
        Value::U8(7),
        Value::I16(7),
        Value::U16(7),
        Value::I32(7),
        Value::U32(7),
        Value::I64(7),
        Value::U64(7),
        Value::Value(Box::new(Value::U32(7))),
    ];
    for value in &integers {
        assert_eq!(field(value), Field::Integer(7), "{value:?}");
    }
    assert_eq!(field(&Value::U64(u64::MAX)), Field::Other, "too large");
    assert_eq!(field(&Value::F64(7.0)), Field::Other);
    assert_eq!(
        field(&Value::from(vec!["A".to_owned(), "B".to_owned()])),
        Field::Texts(vec!["A".to_owned(), "B".to_owned()])
    );
    assert_eq!(field(&Value::from(vec![1_u32, 2])), Field::Other);
}

/// Reads this desktop's players; play something first. Run with
/// `cargo test -- --ignored --nocapture reads_this_desktops`.
#[test]
#[ignore = "reads this desktop's media players"]
fn reads_this_desktops_players() {
    let (mut track, mut card) = read_media_sessions(DebugLog::default());
    track.follow(Some(Follow::AnyApp));
    card.follow(Some(CardSource::AnyApp));
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut reading, mut shown) = (None, None);
    while reading.is_none() || shown.is_none() {
        assert!(Instant::now() < deadline, "no reading");
        reading = reading.or_else(|| track.take_reading());
        shown = shown.or_else(|| card.take_reading());
        thread::sleep(Duration::from_millis(50));
    }
    match reading.unwrap().track {
        Some(track) => println!(
            "{:?}, artwork: {:?} bytes, timeline: {:?}",
            track.session,
            track.artwork.map(|jpeg| jpeg.len()),
            track.timeline
        ),
        None => println!("no player counts"),
    }
    if let Some(shown) = shown.unwrap() {
        println!("card: {} / {}", shown.title, shown.subtitle());
    }
}

#[test]
fn a_fifo_as_artwork_shows_none_and_holds_nothing_up() {
    let fifo = std::env::temp_dir().join(format!("audiosync-test-art-{}", std::process::id()));
    let _ = std::fs::remove_file(&fifo);
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    let name = format!("{}a", prefix("fifo"));
    let _fake = serve(
        &name,
        FakePlayer {
            art_url: Some(format!("file://{}", fifo.display())),
            ..player("Playing", "Song")
        },
    );
    let (sent, read) = mpsc::channel();
    thread::spawn(move || {
        let mut players = players("fifo");
        let listed = players.list().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let _ = sent.send(players.thumbnail(&listed[0].1, deadline));
    });
    let thumbnail = read.recv_timeout(Duration::from_secs(5));
    let _ = std::fs::remove_file(&fifo);
    let thumbnail = thumbnail.expect("reading the artwork doesn't wait on the FIFO");
    assert!(thumbnail.is_err(), "no artwork: {thumbnail:?}");
}
