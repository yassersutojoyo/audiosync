//! Plays one stream to receivers given by address, with no discovery and no
//! UI, through the same sender the apps drive: every receiver is a speaker
//! of one sender on one timeline. It sends digital silence, with `--tone`
//! a 440 Hz sine at -30 dBFS, or with `--click` a short burst every 4 s from
//! 6 s on, for `--seconds` (20 by default), then prints one summary line per
//! receiver. Exits 1 when a receiver never started playing or reported an
//! error, 2 on bad arguments.
//!
//! With `--click` it sends PCM and also prints, per receiver and click, the
//! click's RTP timestamp and the play time the sender's sync packets state
//! for it, both read back from the datagrams that left, and a `SENDER` line
//! per receiver after the first: PASS when every click sent with 3 s of the
//! run left has a stated time on each receiver, and those times differ by
//! the delay difference within one frame (P102). Exits 3 when that line
//! fails and every receiver played.
//!
//! ```text
//! cargo run --release -p audiosync-airplay --example rx_pair -- \
//!     --rx <host:port>[,ap1|ap2][,delay=<ms>] [--rx ...] [--seconds <n>] [--tone|--click] [--verbose]
//! ```
//!
//! `ap2` (the default) takes the AirPlay 2 path, `ap1` the AirPlay 1 path.
//! `delay=<ms>` is the speaker's d_s (P99), set through the speaker options
//! as a host sets it, so the click shows the sender's own timing; the
//! example `rx_onset` and `scripts/two_speaker_sync_wsl.sh` measure it.
//! The check is the protocol side only: sessions set up and packets go
//! out. Whether the receivers share one anchor is read from their logs. It
//! never says the receivers sound in step. With two AirPlay 2 receivers
//! on the PTP timeline the sender names
//! both in each SETPEERS and updates the first when the second joins (P207,
//! P209); this driver can't see those requests, the receivers' logs can.
//! An AirPlay 2 receiver needs an IP address of its own (one PTP peer per
//! address), and the sender binds the PTP ports 319 and 320, which takes
//! root or a lowered `net.ipv4.ip_unprivileged_port_start` on Linux.
//! Development only.

#[path = "rx_pair/sync_check.rs"]
mod sync_check;

use std::collections::BTreeMap;
use std::f64::consts::TAU;
use std::net::{SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::audio::PACKET_FRAMES;
use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ProtocolPath, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
    random_airplay2_device_id,
};
use audiosync_airplay::stream::SpeakerCounters;
use audiosync_airplay::volume::Volume;
use sync_check::{
    CLICK_FIRST_FRAME, CLICK_PERIOD_FRAMES, RATE, RxSpec, WireLog, click_sample, parse_rx_spec,
};

const BLOCK_FRAMES: usize = 441;
const TONE_HZ: f64 = 440.0;
/// -30 dBFS.
const TONE_LEVEL: f64 = 0.031_622_776;

struct Rx {
    address: SocketAddr,
    path: ProtocolPath,
    delay: Duration,
}

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Silence,
    Tone,
    Click,
}

struct Args {
    receivers: Vec<Rx>,
    seconds: u64,
    source: Source,
    verbose: bool,
}

#[derive(Default)]
struct Outcome {
    path: Option<ProtocolPath>,
    played: bool,
    state: Option<SpeakerState>,
    first_sound: Option<f64>,
    errors: Vec<String>,
    warnings: usize,
    /// Sender clock seconds when it first reached Playing.
    joined: Option<f64>,
    wire: WireLog,
}

fn usage() -> &'static str {
    "usage: rx_pair --rx <host:port>[,ap1|ap2][,delay=<ms>] [--rx ...] [--seconds <n>] [--tone|--click] [--verbose]"
}

fn parse_rx(text: &str) -> Result<Rx, String> {
    let RxSpec {
        host_port,
        path,
        delay,
    } = parse_rx_spec(text)?;
    let address = host_port
        .to_socket_addrs()
        .map_err(|error| format!("--rx {text}: {error}"))?
        .next()
        .ok_or_else(|| format!("--rx {text}: no address"))?;
    Ok(Rx {
        address,
        path,
        delay,
    })
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        receivers: Vec::new(),
        seconds: 20,
        source: Source::Silence,
        verbose: false,
    };
    let mut items = std::env::args().skip(1);
    while let Some(item) = items.next() {
        match item.as_str() {
            "--rx" => {
                let value = items.next().ok_or("--rx needs host:port")?;
                args.receivers.push(parse_rx(&value)?);
            }
            "--seconds" => {
                let value = items.next().ok_or("--seconds needs a number")?;
                args.seconds = value
                    .parse()
                    .map_err(|_| format!("--seconds {value}: not a whole number"))?;
            }
            "--tone" | "--click" if args.source != Source::Silence => {
                return Err("--tone and --click exclude each other".to_owned());
            }
            "--tone" => args.source = Source::Tone,
            "--click" => args.source = Source::Click,
            "--verbose" => args.verbose = true,
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if args.receivers.is_empty() {
        return Err("at least one --rx is needed".to_owned());
    }
    if args.receivers.len() > 200 {
        return Err("at most 200 receivers".to_owned());
    }
    Ok(args)
}

/// The device discovery would have built for this receiver, from the one
/// service record of its path.
fn device(rx: &Rx, n: u8) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, n]);
    let (kind, instance, txt): (_, _, &[&str]) = match rx.path {
        ProtocolPath::AirPlay1 => (
            ServiceKind::Raop,
            format!("{device_id}@Receiver {n}"),
            &[
                "txtvers=1",
                "ch=2",
                "cn=0,1",
                "et=0",
                "sr=44100",
                "ss=16",
                "tp=UDP",
                "md=0,1,2",
            ],
        ),
        ProtocolPath::AirPlay2 => (
            ServiceKind::AirPlay,
            format!("Receiver {n}"),
            &["features=0x0,0x10040", "flags=0x4", "model=Receiver1,1"],
        ),
    };
    let mut txt: Vec<Vec<u8>> = txt.iter().map(|s| s.as_bytes().to_vec()).collect();
    if rx.path == ProtocolPath::AirPlay2 {
        txt.push(format!("deviceid={device_id}").into_bytes());
    }
    let record = ServiceRecord {
        kind,
        instance,
        host: format!("receiver-{n}.local"),
        port: rx.address.port(),
        txt,
        addresses: vec![ScopedAddr::new(rx.address.ip())],
    };
    let (raop, airplay) = match rx.path {
        ProtocolPath::AirPlay1 => (Some(&record), None),
        ProtocolPath::AirPlay2 => (None, Some(&record)),
    };
    Device::from_records(DeviceKey::Id(device_id), raop, airplay).expect("a device from a record")
}

/// A capture thread at real-time pace, as a host's would be.
fn capture(
    mut writer: InputWriter,
    source: Source,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let start = Instant::now();
        let mut written: u64 = 0;
        let mut block = vec![0f32; BLOCK_FRAMES * 2];
        while !stop.load(Ordering::Relaxed) {
            for (index, pair) in block.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let n = written + index as u64;
                let sample = match source {
                    Source::Silence => 0.0,
                    Source::Tone => {
                        let phase = (n as f64 * TONE_HZ / RATE).fract();
                        (TONE_LEVEL * (TAU * phase).sin()) as f32
                    }
                    Source::Click => click_sample(n),
                };
                pair[0] = sample;
                pair[1] = sample;
            }
            writer.write_f32(&block, Instant::now());
            written += BLOCK_FRAMES as u64;
            let next = start + Duration::from_secs_f64(written as f64 / RATE);
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
    })
}

fn path_name(path: Option<ProtocolPath>) -> &'static str {
    match path {
        Some(ProtocolPath::AirPlay1) => "ap1",
        Some(ProtocolPath::AirPlay2) => "ap2",
        None => "none",
    }
}

fn summary(rx: &Rx, outcome: &Outcome, counters: &SpeakerCounters, ok: bool) -> String {
    let first_sound = outcome
        .first_sound
        .map_or("none".to_owned(), |seconds| format!("{seconds:.6}"));
    let errors = if outcome.errors.is_empty() {
        "none".to_owned()
    } else {
        outcome.errors.join("; ")
    };
    format!(
        "rx {} {} path={} delay_ms={} state={:?} frames_sent={} packets_sent={} first_sound_s={} \
         resend_requests={} packets_resent={} late_packets={} warnings={} errors={}",
        rx.address,
        if ok { "OK" } else { "FAIL" },
        path_name(outcome.path),
        rx.delay.as_millis(),
        outcome.state,
        counters.packets_sent() * PACKET_FRAMES,
        counters.packets_sent(),
        first_sound,
        counters.resend_requests(),
        counters.packets_resent(),
        counters.late_packets(),
        outcome.warnings,
        errors,
    )
}

/// Prints each receiver's stated play time per click and a SENDER line per
/// receiver after the first; true when every SENDER line passes.
fn sender_check(args: &Args, order: &[DeviceKey], outcomes: &BTreeMap<DeviceKey, Outcome>) -> bool {
    // A click captured less than 3 s before the end may not have gone out.
    let expected = (0..)
        .map(|k| CLICK_FIRST_FRAME + k * CLICK_PERIOD_FRAMES)
        .take_while(|&frame| frame as f64 / RATE + 3.0 < args.seconds as f64)
        .count();
    let mut stated = Vec::new();
    for (rx, key) in args.receivers.iter().zip(order) {
        let outcome = &outcomes[key];
        let joined = outcome
            .joined
            .map_or("none".to_owned(), |s| format!("{s:.6}"));
        let first = outcome
            .wire
            .first_packet
            .map_or("none".to_owned(), |(packet, rtp)| format!("{packet}/{rtp}"));
        let clicks = outcome.wire.clicks();
        for (index, (rtp, play)) in clicks.iter().enumerate() {
            let play = play.map_or("none".to_owned(), |s| format!("{s:.9}"));
            println!(
                "stated {} delay_ms={} joined_s={joined} first_packet/rtp={first} syncs={} click={} rtp={rtp} play_s={play}",
                rx.address,
                rx.delay.as_millis(),
                outcome.wire.syncs(),
                index + 1,
            );
        }
        stated.push(clicks);
    }
    let mut pass = true;
    let reference = &args.receivers[0];
    for (rx, clicks) in args.receivers.iter().zip(&stated).skip(1) {
        let wanted = rx.delay.as_secs_f64() - reference.delay.as_secs_f64();
        let diffs: Vec<f64> = stated[0]
            .iter()
            .zip(clicks)
            .take(expected)
            .map_while(|((_, a), (_, b))| Some((*b)? - (*a)?))
            .collect();
        let worst = diffs
            .iter()
            .map(|diff| (diff - wanted) * RATE)
            .fold(0.0f64, |worst, error| {
                if error.abs() > worst.abs() {
                    error
                } else {
                    worst
                }
            });
        let ok = diffs.len() == expected && expected > 0 && worst.abs() <= 1.0;
        pass &= ok;
        let first = diffs
            .first()
            .map_or("none".to_owned(), |diff| format!("{:.3}", diff * 1000.0));
        println!(
            "SENDER {} {} vs {}: stated play times differ by {first} ms on the first click, expected {:.0} ms; worst error {worst:.2} frames over {} of {expected} clicks (limit 1 frame)",
            if ok { "PASS" } else { "FAIL" },
            reference.address,
            rx.address,
            wanted * 1000.0,
            diffs.len(),
        );
    }
    pass
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{}", usage());
            return ExitCode::from(2);
        }
    };
    let config = SenderConfig {
        airplay2: true,
        airplay2_device_id: random_airplay2_device_id(),
        ..SenderConfig::default()
    };
    let (sender, writer) = match Sender::start(config) {
        Ok(started) => started,
        Err(error) => {
            eprintln!("the sender didn't start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let clock = sender.clock();
    let stop = Arc::new(AtomicBool::new(false));
    let audio = capture(writer, args.source, stop.clone());

    let mut order = Vec::new();
    let mut keys = BTreeMap::new();
    let mut outcomes: BTreeMap<DeviceKey, Outcome> = BTreeMap::new();
    let mut counters = Vec::new();
    for (index, rx) in args.receivers.iter().enumerate() {
        let device = device(rx, index as u8 + 1);
        let key = device.key.clone();
        let mut options = SpeakerOptions::new(Volume::Db(-20.0));
        options.delay = rx.delay;
        options.trace = args.verbose;
        options.trace_timing = args.source == Source::Click;
        // The traces find a click by its first non-zero payload byte.
        options.pcm = args.source == Source::Click;
        counters.push(sender.add_speaker(device, options));
        keys.insert(key.clone(), rx.address);
        outcomes.insert(key.clone(), Outcome::default());
        order.push(key);
    }

    let end = Instant::now() + Duration::from_secs(args.seconds);
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        // A dead driver thread makes wait_event return at once, every time.
        if !sender.is_running() {
            eprintln!("the sender's driver thread stopped");
            for outcome in outcomes.values_mut() {
                outcome.errors.push("SenderStopped".to_owned());
            }
            break;
        }
        let Some(event) = sender.wait_event(left) else {
            continue;
        };
        match event {
            SenderEvent::Speaker { key, event } => {
                let address = keys.get(&key).copied();
                let Some(outcome) = outcomes.get_mut(&key) else {
                    continue;
                };
                let address = address.map_or_else(|| "?".to_owned(), |a| a.to_string());
                match event {
                    SpeakerEvent::Path(path) => outcome.path = Some(path),
                    SpeakerEvent::State(state) => {
                        if state == SpeakerState::Playing && outcome.joined.is_none() {
                            outcome.joined = Some(clock.seconds(Instant::now()));
                        }
                        outcome.played |= state == SpeakerState::Playing;
                        outcome.state = Some(state);
                        eprintln!("{address}: {state:?}");
                    }
                    SpeakerEvent::Latency(latency) => {
                        outcome.first_sound = Some(clock.seconds(latency.first_sound));
                    }
                    SpeakerEvent::Error(error) => {
                        eprintln!("{address}: error {:?}", error.error);
                        outcome.errors.push(format!("{:?}", error.error.kind));
                    }
                    SpeakerEvent::Warning(warning) => {
                        eprintln!("{address}: warning {warning:?}");
                        outcome.warnings += 1;
                    }
                    SpeakerEvent::Trace(line) => {
                        outcome.wire.trace(&line);
                        if args.verbose {
                            eprintln!("{address}: {line}");
                        }
                    }
                    _ => {}
                }
            }
            SenderEvent::Timeline {
                latency,
                origin,
                restarted,
            } => eprintln!("timeline: latency {latency:?}, origin {origin}, restarted {restarted}"),
            _ => {}
        }
    }

    let mut failed = false;
    for ((rx, key), counters) in args.receivers.iter().zip(&order).zip(&counters) {
        let outcome = &outcomes[key];
        let ok = outcome.played && outcome.errors.is_empty();
        failed |= !ok;
        println!("{}", summary(rx, outcome, counters, ok));
    }
    let sender_failed = args.source == Source::Click && !sender_check(&args, &order, &outcomes);
    stop.store(true, Ordering::Relaxed);
    let _ = audio.join();
    sender.shutdown();
    if failed {
        ExitCode::FAILURE
    } else if sender_failed {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}
