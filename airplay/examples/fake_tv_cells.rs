//! Runs the dry-run cells the sender can prove against the fake television
//! of `fake_tv`, in process on 127.0.0.1 with nothing advertised, and
//! prints one line per cell, `<cell> PASS|FAIL|MANUAL <measured value>`;
//! a cell that needs the app, a player or the real television prints
//! MANUAL with its step. The sender plays the host's part: it asks for the
//! level once a second, pauses and resumes, sends track info, and enters
//! the PIN. Exits 1 when a cell fails. Development only.
//!
//! ```text
//! cargo run -p audiosync-airplay --example fake_tv_cells -- [--pause-minutes <n>]
//! ```
//!
//! The paused-cover cell pauses 11 minutes unless `--pause-minutes` asks
//! for less, which its line then says.

#[path = "fake_tv/cells.rs"]
mod cells;
#[path = "../tests/common/mod.rs"]
mod common;
#[path = "fake_tv/television.rs"]
mod television;

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::metadata::TrackInfo;
use audiosync_airplay::pairing::PairingEvent;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::sender::{
    NowPlaying, Progress, Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions,
    SpeakerState,
};
use audiosync_airplay::volume::{MIN_DB, Volume, parse_reported};

use cells::{Cell, Pause, Status};
use common::ap2::Ap2Server;
use common::hap::{item, untlv};
use common::net::{Events, HostAudio, free_ptp_ports, recv_queued};
use television::{Pin, television};

const NAME: &str = "AudioSync Test TV";
const CODE: &str = "4821";
const LONG: Duration = Duration::from_secs(15);
/// The host asks a playing speaker for its level once a second (S291).
const ASK_EVERY: Duration = Duration::from_secs(1);
/// The rows the app sets, the first as the speaker starts.
const SEND_ROWS: [u32; 5] = [20, 50, 80, 100, 0];
/// The rows `vol` sets on the television, each unlike the one before.
const READ_ROWS: [u32; 5] = [20, 80, 100, 0, 50];
/// Remote presses of 0.3 dB down from row 50.
const PRESSES: [f64; 3] = [-15.3, -15.6, -15.9];
/// Audio packets that show play went on after a resume.
const RESUMED_PACKETS: usize = 50;

/// The fake television with a data port that counts the audio it gets.
struct Tv {
    server: Ap2Server,
    pin: Pin,
    audio: Arc<AtomicUsize>,
}

/// What the television was sent so far, by kind.
#[derive(Clone, Copy)]
struct Sent {
    flushes: usize,
    teardowns: usize,
    setups: usize,
    records: usize,
}

impl Tv {
    fn start(pin: Pin) -> Tv {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback data port");
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("a read timeout");
        let mut fake = television(NAME, pin, CODE, television::FEATURES_METADATA);
        fake.data_port = socket.local_addr().expect("its address").port();
        let audio = Arc::new(AtomicUsize::new(0));
        let counted = audio.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                match recv_queued(&socket, &mut buf) {
                    Ok(_) => {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                    // Windows reports an earlier send to a closed port here.
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Tv {
            server: Ap2Server::start(fake),
            pin,
            audio,
        }
    }

    fn device(&self) -> Device {
        let record = ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: NAME.to_owned(),
            host: format!("{}.local", television::HOST_LABEL),
            port: self.server.address.port(),
            txt: television::txt(self.pin, television::FEATURES_METADATA)
                .into_iter()
                .map(|(key, value)| format!("{key}={value}").into_bytes())
                .collect(),
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
        };
        let key = DeviceKey::Id(DeviceId(television::DEVICE_ID));
        Device::from_records(key, None, Some(&record)).expect("the television's record")
    }

    fn count(&self, key: &str) -> usize {
        self.server
            .fake()
            .keys()
            .iter()
            .filter(|k| *k == key)
            .count()
    }

    fn sent(&self) -> Sent {
        Sent {
            flushes: self.count("FLUSH"),
            teardowns: self.count("TEARDOWN"),
            setups: self.count("SETUP session") + self.count("SETUP stream"),
            records: self.count("RECORD"),
        }
    }

    /// The level a volume read answers now.
    fn level(&self) -> Option<f64> {
        parse_reported(self.server.fake().bodies.get("GET_PARAMETER")?)
    }

    fn volume_sets(&self) -> usize {
        self.server
            .fake()
            .requests
            .iter()
            .filter(|(request, _)| {
                request.method == Method::SetParameter && request.body.starts_with(b"volume:")
            })
            .count()
    }

    /// PIN pairings started (pair-setup M1 without the transient flag) and
    /// finished (M5, sent only once the PIN proof was accepted).
    fn pin_pairings(&self) -> (usize, usize) {
        let fake = self.server.fake();
        let mut counts = (0, 0);
        for (request, _) in &fake.requests {
            if request.method != Method::Post || request.uri != "/pair-setup" {
                continue;
            }
            let items = untlv(&request.body);
            match item(&items, 6).and_then(|state| state.first().copied()) {
                Some(1) if item(&items, 19).is_none() => counts.0 += 1,
                Some(5) => counts.1 += 1,
                _ => {}
            }
        }
        counts
    }

    /// Track info, artwork and progress sets the television received.
    fn metadata_sets(&self) -> usize {
        self.sets_of(|kind, body| {
            kind == "application/x-dmap-tagged"
                || kind == "image/jpeg"
                || (kind == "text/parameters" && body.starts_with(b"progress:"))
        })
    }

    /// The DMAP track text sets alone, so progress cannot stand in for them.
    fn track_text_sets(&self) -> usize {
        self.sets_of(|kind, _| kind == "application/x-dmap-tagged")
    }

    fn sets_of(&self, wanted: impl Fn(&str, &[u8]) -> bool) -> usize {
        self.server
            .fake()
            .requests
            .iter()
            .filter(|(request, _)| {
                request.method == Method::SetParameter
                    && request
                        .headers
                        .get("Content-Type")
                        .is_some_and(|kind| wanted(kind, &request.body))
            })
            .count()
    }

    fn audio(&self) -> usize {
        self.audio.load(Ordering::SeqCst)
    }
}

/// The sender with the host's side of the cells.
struct Host {
    sender: Sender,
    _audio: HostAudio,
    events: Events,
    next_ask: Instant,
}

impl Host {
    fn start() -> Host {
        let (sender, writer) = Sender::start(SenderConfig {
            airplay2: true,
            airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
            ptp_ports: free_ptp_ports(),
            ..SenderConfig::default()
        })
        .expect("the sender starts");
        Host {
            sender,
            _audio: HostAudio::start(writer),
            events: Events::default(),
            next_ask: Instant::now(),
        }
    }

    /// Polls events until `done` holds or `by` passes, asking `asking` for
    /// its level once a second; whether `done` held.
    fn run_until(
        &mut self,
        asking: Option<&DeviceKey>,
        by: Duration,
        mut done: impl FnMut(&Host) -> bool,
    ) -> bool {
        let end = Instant::now() + by;
        loop {
            self.events.drain(&self.sender);
            if done(self) {
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            if let Some(key) = asking
                && now >= self.next_ask
            {
                self.sender.read_volume(key);
                self.next_ask = now + ASK_EVERY;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn run(&mut self, asking: Option<&DeviceKey>, by: Duration) {
        self.run_until(asking, by, |_| false);
    }

    fn playing(&self, key: &DeviceKey) -> usize {
        self.events
            .of(key)
            .iter()
            .filter(|event| matches!(event, SpeakerEvent::State(SpeakerState::Playing)))
            .count()
    }

    fn reports_since(&self, key: &DeviceKey, since: Instant) -> Vec<(Duration, f64)> {
        self.events
            .seen
            .iter()
            .filter(|(at, _)| *at >= since)
            .filter_map(|(at, event)| match event {
                SenderEvent::Speaker {
                    key: k,
                    event: SpeakerEvent::VolumeReported(db),
                } if k == key => Some((*at - since, *db)),
                _ => None,
            })
            .collect()
    }

    fn pairing(&self, key: &DeviceKey, wanted: impl Fn(&PairingEvent) -> bool) -> usize {
        self.events
            .seen
            .iter()
            .filter(|(_, event)| {
                matches!(event, SenderEvent::Pairing { key: k, event } if k == key && wanted(event))
            })
            .count()
    }

    /// Adds the speaker at `row` and waits until it plays and audio
    /// reaches the television.
    fn play(&mut self, tv: &Tv, row: u32) -> Result<DeviceKey, String> {
        let device = tv.device();
        let key = device.key.clone();
        let before = self.playing(&key);
        self.sender
            .add_speaker(device, SpeakerOptions::new(volume_for_row(row)));
        let heard = tv.audio();
        let played = self.run_until(Some(&key), LONG, |host| {
            host.playing(&key) > before && tv.audio() > heard + RESUMED_PACKETS
        });
        if played {
            Ok(key)
        } else {
            Err(format!(
                "the speaker never played: {:?}",
                self.events.of(&key).last()
            ))
        }
    }
}

/// The volume the host sends for `row`: muted at 0 (S289), which
/// [`Volume::from_db`] would clamp to −30 dB.
fn volume_for_row(row: u32) -> Volume {
    if row == 0 {
        return Volume::Muted;
    }
    Volume::from_db(cells::db_for_row(row)).unwrap_or(Volume::Muted)
}

/// The level the television's remote sets for `row`: its floor for row 0.
fn tv_db_for_row(row: u32) -> f64 {
    if row == 0 {
        MIN_DB
    } else {
        cells::db_for_row(row)
    }
}

fn k1(tv: &Tv, host: &mut Host, key: &DeviceKey) -> [Cell; 3] {
    let mut landed = Vec::new();
    for (index, row) in SEND_ROWS.into_iter().enumerate() {
        if index > 0 {
            host.sender.set_volume(key, volume_for_row(row));
        }
        let target = cells::db_for_row(row);
        host.run_until(Some(key), Duration::from_secs(3), |_| {
            tv.level().is_some_and(|db| (db - target).abs() < 0.35)
        });
        landed.push((row, tv.level()));
    }
    let send = cells::k1_send(&landed);

    let mut seen = Vec::new();
    for row in READ_ROWS {
        host.run(Some(key), Duration::from_millis(400));
        let pressed = Instant::now();
        tv.server.fake().set_on_tv(tv_db_for_row(row));
        let shown = |host: &Host| {
            host.reports_since(key, pressed)
                .into_iter()
                .find(|(_, db)| cells::row_for_db(*db) == row)
        };
        host.run_until(Some(key), Duration::from_secs(3), |host| {
            shown(host).is_some()
        });
        seen.push((
            row,
            shown(host).map(|(after, db)| (cells::row_for_db(db), after)),
        ));
    }
    let read = cells::k1_read(&seen);

    let sets_before = tv.volume_sets();
    let mut presses = Vec::new();
    for db in PRESSES {
        host.run(Some(key), Duration::from_millis(400));
        let pressed = Instant::now();
        tv.server.fake().set_on_tv(db);
        let reported = |host: &Host| {
            host.reports_since(key, pressed)
                .into_iter()
                .find(|(_, level)| (level - db).abs() <= cells::PRESS_DB)
        };
        host.run_until(Some(key), Duration::from_millis(2500), |host| {
            reported(host).is_some()
        });
        presses.push((db, reported(host).map(|(after, level)| (level, after))));
    }
    host.run(Some(key), Duration::from_secs(2));
    let press = cells::k1_press(&presses, tv.volume_sets() - sets_before);
    [send, read, press]
}

fn d1(tv: &Tv, host: &mut Host) -> Cell {
    const SWITCH_ONS: usize = 2;
    let device = tv.device();
    let key = device.key.clone();
    let prompt = |event: &PairingEvent| *event == PairingEvent::PinRequested;
    let paired = |event: &PairingEvent| {
        matches!(
            event,
            PairingEvent::PairedForSession | PairingEvent::HomeKitPaired(_)
        )
    };
    for on in 1..=SWITCH_ONS {
        // The host pairs first: with a PIN every time, nothing it stored verifies.
        host.sender.start_pairing(device.clone());
        if host.run_until(None, LONG, |host| host.pairing(&key, prompt) >= on) {
            let _ = host.sender.submit_pin(&key, CODE);
        }
        host.run_until(None, LONG, |host| host.pairing(&key, paired) >= on);
        host.sender
            .add_speaker(device.clone(), SpeakerOptions::new(volume_for_row(20)));
        host.run_until(Some(&key), LONG, |host| host.playing(&key) >= on);
        host.run(Some(&key), Duration::from_secs(1));
        host.sender.remove_speaker(&key);
        host.run_until(None, LONG, |_| tv.count("TEARDOWN") >= on);
    }
    let (started, done) = tv.pin_pairings();
    cells::d1(
        SWITCH_ONS,
        host.pairing(&key, prompt),
        started,
        done,
        host.playing(&key),
    )
}

/// Pauses for `hold`, resumes, and counts what the television was sent
/// from the pause on.
fn pause_once(tv: &Tv, host: &mut Host, key: &DeviceKey, hold: Duration) -> Pause {
    let before = tv.sent();
    host.sender.pause();
    host.run_until(Some(key), LONG, |_| tv.count("FLUSH") > before.flushes);
    host.run(Some(key), hold);
    let heard = tv.audio();
    host.sender.resume();
    let resumed = host.run_until(Some(key), LONG, |_| tv.audio() > heard + RESUMED_PACKETS);
    // A request sent late still counts.
    host.run(Some(key), Duration::from_secs(1));
    let after = tv.sent();
    Pause {
        flushes: after.flushes - before.flushes,
        teardowns: after.teardowns - before.teardowns,
        setups: after.setups - before.setups,
        records: after.records - before.records,
        resumed,
    }
}

fn now_playing(title: &str, elapsed: u64) -> NowPlaying {
    NowPlaying {
        track: Some(TrackInfo {
            title: title.to_owned(),
            artist: "Test Artist".to_owned(),
            album: "Test Album".to_owned(),
        }),
        artwork: None,
        progress: Some(Progress {
            elapsed: Duration::from_secs(elapsed),
            length: Duration::from_secs(240),
        }),
    }
}

fn d3_track(tv: &Tv, host: &mut Host, key: &DeviceKey) -> Cell {
    let before = tv.count("FLUSH");
    let sets_before = tv.metadata_sets();
    let text_before = tv.track_text_sets();
    let changes = [
        now_playing("Track One", 10),
        now_playing("Track Two", 0),
        now_playing("Track Two", 120),
    ];
    let count = changes.len();
    for change in changes {
        let _ = host.sender.set_now_playing(change);
        host.run(Some(key), Duration::from_secs(1));
    }
    host.run(Some(key), Duration::from_secs(1));
    cells::d3_track(
        count,
        tv.metadata_sets() - sets_before,
        tv.track_text_sets() - text_before,
        tv.count("FLUSH") - before,
    )
}

struct Report {
    manual: Vec<Cell>,
    failed: bool,
}

impl Report {
    fn show(&mut self, cell: &Cell) {
        println!("{}", cell.line());
        let _ = std::io::stdout().flush();
        self.failed |= cell.status == Status::Fail;
    }

    fn show_manual(&mut self, ids: &[&str]) {
        for id in ids {
            if let Some(index) = self.manual.iter().position(|cell| cell.id == *id) {
                let cell = self.manual.remove(index);
                self.show(&cell);
            }
        }
    }

    fn show_failed(&mut self, ids: &[&'static str], why: &str) {
        for id in ids {
            self.show(&Cell::new(id, Status::Fail, why.to_owned()));
        }
    }
}

fn parse_pause(mut args: impl Iterator<Item = String>) -> Result<Duration, String> {
    let mut pause = cells::COVER_PAUSE;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pause-minutes" => {
                let minutes: f64 = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .filter(|minutes: &f64| minutes.is_finite() && *minutes >= 0.0)
                    .ok_or("--pause-minutes takes a number of minutes")?;
                pause = Duration::from_secs_f64(minutes * 60.0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(pause)
}

fn main() -> ExitCode {
    let cover_pause = match parse_pause(std::env::args().skip(1)) {
        Ok(pause) => pause,
        Err(error) => {
            eprintln!("fake_tv_cells: {error}");
            return ExitCode::from(2);
        }
    };
    let mut report = Report {
        manual: cells::manual(),
        failed: false,
    };

    let tv = Tv::start(Pin::None);
    let mut host = Host::start();
    match host.play(&tv, SEND_ROWS[0]) {
        Ok(key) => {
            for cell in k1(&tv, &mut host, &key) {
                report.show(&cell);
            }
        }
        Err(why) => report.show_failed(&["K1.send", "K1.read", "K1.press"], &why),
    }
    host.sender.shutdown();
    report.show_manual(&["K1.row", "K2", "K3"]);

    let tv = Tv::start(Pin::EveryTime);
    let mut host = Host::start();
    report.show(&d1(&tv, &mut host));
    host.sender.shutdown();

    let tv = Tv::start(Pin::None);
    let mut host = Host::start();
    match host.play(&tv, SEND_ROWS[0]) {
        Ok(key) => {
            report.show(&cells::d2(pause_once(
                &tv,
                &mut host,
                &key,
                Duration::from_secs(2),
            )));
            report.show(&d3_track(&tv, &mut host, &key));
            report.show_manual(&["D3"]);
            eprintln!(
                "fake_tv_cells: D4 pauses {:.1} min",
                cover_pause.as_secs_f64() / 60.0
            );
            let pause = pause_once(&tv, &mut host, &key, cover_pause);
            report.show(&cells::d4(cover_pause, pause));
        }
        Err(why) => {
            report.show_failed(&["D2", "D3.track"], &why);
            report.show_manual(&["D3"]);
            report.show_failed(&["D4"], &why);
        }
    }
    host.sender.shutdown();
    report.show_manual(&["D4.cover", "D5", "D6", "W1", "W2", "W3", "W4", "W5", "W6"]);

    if report.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
