//! A fake AirPlay 2 television, for dry runs of a real sender before a
//! session with the real set: the test fake's Samsung profile
//! ([`FakeAp2::samsung`]). By default it serves on 127.0.0.1 only and
//! advertises nothing, so Windows Firewall has nothing to ask; `--lan`
//! serves it on every interface and advertises it over mDNS as
//! `_airplay._tcp` only, so a sender on the network takes the AirPlay 2 path.
//! Development only; examples never enter a release build.
//!
//! ```text
//! cargo run -p audiosync-airplay --example fake_tv -- [--lan] [--name <text>]
//!     [--port <n>] [--pin every|once] [--code <digits>] [--summary]
//! ```
//!
//! Every request line and its answer status goes to stdout; with `--summary`
//! only one plain line per event a dry-run sheet judges, with its running
//! count (the `summary` module). On stdin,
//! `vol <dB>` moves the level as the television's remote does and `quit`
//! stops it.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "fake_tv/summary.rs"]
mod summary;

use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::rtsp::Request;
use common::ap2::{Ap2Server, FakeAp2, dict, int, plist, request_key};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use plist::Value;
use summary::Summary;

const SERVICE_TYPE: &str = "_airplay._tcp.local.";
/// The label of its own mDNS host name, in the `.local.` domain.
const HOST_LABEL: &str = "audiosync-test-tv";
/// A locally administered address in the documentation block of RFC 7042.
const DEVICE_ID: &str = "02:00:5E:00:53:55";
/// Bits 38 (CoreUtils pairing) and 48 (transient pairing): AirPlay 2 capable.
const FEATURES_HIGH: u64 = 0x1_0040;
const MODEL: &str = "Speaker1,1";
/// Status flags as the television sends them without a PIN (bit 2).
const FLAGS_PLAIN: u64 = 0x4;
/// Bit 3: a PIN on every connection, and every pair-verify turned down.
const FLAGS_PIN_EVERY_TIME: u64 = 0x4c;
/// Bit 9: a PIN the first time only.
const FLAGS_PIN_ONCE: u64 = 0x244;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pin {
    None,
    EveryTime,
    Once,
}

struct Options {
    name: String,
    port: u16,
    pin: Pin,
    code: String,
    on_network: bool,
    summary: bool,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        name: "AudioSync Test TV".to_owned(),
        port: 0,
        pin: Pin::None,
        code: "4821".to_owned(),
        on_network: false,
        summary: false,
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--lan" => options.on_network = true,
            "--summary" => options.summary = true,
            "--name" => options.name = value()?,
            "--port" => options.port = value()?.parse().map_err(|_| "bad --port")?,
            "--pin" => {
                options.pin = match value()?.as_str() {
                    "every" => Pin::EveryTime,
                    "once" => Pin::Once,
                    other => return Err(format!("--pin takes every or once, not {other}")),
                }
            }
            "--code" => {
                let code = value()?;
                if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("--code takes digits".to_owned());
                }
                options.code = code;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

fn status_flags(pin: Pin) -> u64 {
    match pin {
        Pin::None => FLAGS_PLAIN,
        Pin::EveryTime => FLAGS_PIN_EVERY_TIME,
        Pin::Once => FLAGS_PIN_ONCE,
    }
}

fn television(options: &Options) -> FakeAp2 {
    let mut fake = FakeAp2::samsung();
    let flags = status_flags(options.pin);
    if options.pin != Pin::None {
        fake.hap.code = options.code.clone();
        fake.hap.pairing_per_connection = options.pin == Pin::EveryTime;
    }
    fake.info_body = plist(dict(&[
        ("features", int((FEATURES_HIGH << 32) as i64)),
        ("statusFlags", int(flags as i64)),
        ("name", Value::String(options.name.clone())),
        ("model", Value::String(MODEL.to_owned())),
        ("deviceID", Value::String(DEVICE_ID.to_owned())),
    ]));
    fake
}

fn advertise(options: &Options, port: u16) -> Result<(ServiceDaemon, String), String> {
    let features = format!("0x0,{FEATURES_HIGH:#x}");
    let flags = format!("{:#x}", status_flags(options.pin));
    let txt = [
        ("features", features.as_str()),
        ("flags", flags.as_str()),
        ("model", MODEL),
        ("deviceid", DEVICE_ID),
    ];
    let failed = |error: mdns_sd::Error| format!("mDNS: {error}");
    let daemon = ServiceDaemon::new().map_err(failed)?;
    let host = format!("{HOST_LABEL}.local.");
    let info = ServiceInfo::new(SERVICE_TYPE, &options.name, &host, (), port, &txt[..])
        .map_err(failed)?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_owned();
    daemon.register(info).map_err(failed)?;
    Ok((daemon, fullname))
}

/// Browses for the record just registered and logs it once it resolves,
/// which shows the advertisement reached the network.
fn watch_advertisement(daemon: &ServiceDaemon, fullname: String, start: Instant) {
    let Ok(events) = daemon.browse(SERVICE_TYPE) else {
        return;
    };
    thread::spawn(move || {
        while let Ok(event) = events.recv() {
            if let ServiceEvent::ServiceResolved(service) = event
                && service.fullname == fullname
            {
                let addresses: Vec<String> =
                    service.addresses.iter().map(|a| a.to_string()).collect();
                println!(
                    "{} mDNS resolved {} port {} at {}",
                    stamp(start),
                    service.fullname,
                    service.port,
                    addresses.join(", ")
                );
                return;
            }
        }
    });
}

fn stamp(start: Instant) -> String {
    summary::stamp(start.elapsed().as_secs_f64())
}

/// The request line and what identifies it: the session or stream SETUP,
/// a text body's first line (`volume: -20.000000`), `RTP-Info`, or the
/// content type of a binary body.
fn describe(request: &Request) -> String {
    let mut line = format!("{} {}", request.method, request.uri);
    let key = request_key(request);
    if let Some(kind) = key.strip_prefix("SETUP ") {
        line.push_str(&format!(" ({kind})"));
    }
    if let Some(info) = request.headers.get("RTP-Info") {
        line.push_str(&format!(" RTP-Info: {info}"));
    }
    let kind = request.headers.get("Content-Type").unwrap_or_default();
    if kind.starts_with("text/") {
        let text = String::from_utf8_lossy(&request.body);
        if let Some(first) = text.lines().next() {
            line.push_str(&format!(" [{}]", first.trim()));
        }
    } else if !request.body.is_empty() && !request.uri.starts_with("/pair-") {
        line.push_str(&format!(" [{kind}, {} bytes]", request.body.len()));
    }
    line
}

/// Logs requests in order as their answers go out, and new connections:
/// in full, or as the summary's lines when there is one.
fn log_requests(
    server: &Ap2Server,
    start: Instant,
    stop: &AtomicBool,
    summary: Option<&Mutex<Summary>>,
) {
    let (mut logged, mut connections) = (0, 0);
    while !stop.load(Ordering::SeqCst) {
        {
            let fake = server.fake();
            if fake.connections != connections {
                connections = fake.connections;
                match summary {
                    Some(summary) => println!("{}", lock(summary).connection(seconds(start))),
                    None => println!("{} connection {connections} opened", stamp(start)),
                }
            }
            while let (Some((request, framed)), Some(Some(answered))) =
                (fake.requests.get(logged), fake.answered.get(logged))
            {
                if let Some(summary) = summary {
                    let line = lock(summary).request(seconds(start), request, answered.status);
                    if let Some(line) = line {
                        println!("{line}");
                    }
                    logged += 1;
                    continue;
                }
                let sealed = if *framed { " (encrypted)" } else { "" };
                let level = answered
                    .level
                    .as_ref()
                    .map(|level| format!(", reads now answer {level}"))
                    .unwrap_or_default();
                println!(
                    "{} {}{sealed} -> {}{level}",
                    stamp(start),
                    describe(request),
                    answered.status
                );
                logged += 1;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The level a volume read answers now, after the remote's grid.
fn level_read(fake: &FakeAp2) -> Option<f64> {
    let body = fake.bodies.get("GET_PARAMETER")?;
    let text = std::str::from_utf8(body).ok()?;
    text.strip_prefix("volume:")?.trim().parse().ok()
}

fn seconds(start: Instant) -> f64 {
    start.elapsed().as_secs_f64()
}

fn lock(summary: &Mutex<Summary>) -> std::sync::MutexGuard<'_, Summary> {
    summary
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("fake_tv: {error}");
            return ExitCode::from(2);
        }
    };
    let start = Instant::now();
    let ip = if options.on_network {
        Ipv4Addr::UNSPECIFIED
    } else {
        Ipv4Addr::LOCALHOST
    };
    let server = Ap2Server::start_at(
        SocketAddr::new(IpAddr::V4(ip), options.port),
        television(&options),
    );
    let port = server.address.port();
    let advertised = if options.on_network {
        match advertise(&options, port) {
            Ok(advertised) => Some(advertised),
            Err(error) => {
                eprintln!("fake_tv: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    match &advertised {
        Some((_, fullname)) => println!(
            "{} mode: LAN, serving \"{}\" on every interface, port {port}, advertised as {fullname}",
            stamp(start),
            options.name
        ),
        None => println!(
            "{} mode: loopback, serving \"{}\" on 127.0.0.1:{port}, not advertised (--lan serves the network)",
            stamp(start),
            options.name
        ),
    }
    match options.pin {
        Pin::None => println!("{} pairing: transient, no PIN", stamp(start)),
        Pin::EveryTime => println!("{} pairing: PIN {} every time", stamp(start), options.code),
        Pin::Once => println!(
            "{} pairing: PIN {} the first time",
            stamp(start),
            options.code
        ),
    }
    println!(
        "{} stdin: vol <dB> moves the level, quit stops",
        stamp(start)
    );
    if let Some((daemon, fullname)) = &advertised {
        watch_advertisement(daemon, fullname.clone(), start);
    }

    let server = Arc::new(server);
    let stop = Arc::new(AtomicBool::new(false));
    let summary = options
        .summary
        .then(|| Arc::new(Mutex::new(Summary::default())));
    let logger = {
        let (server, stop, summary) = (server.clone(), stop.clone(), summary.clone());
        thread::spawn(move || log_requests(&server, start, &stop, summary.as_deref()))
    };
    let mut quit = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("quit"), _) => {
                quit = true;
                break;
            }
            (Some("vol"), Some(db)) => match db.parse::<f64>() {
                Ok(db) => {
                    let level = {
                        let mut fake = server.fake();
                        fake.set_on_tv(db);
                        level_read(&fake).unwrap_or(db)
                    };
                    match &summary {
                        Some(summary) => {
                            println!("{}", lock(summary).pushed(seconds(start), level))
                        }
                        None => println!(
                            "{} remote: level set to {db} dB (0.3 dB grid)",
                            stamp(start)
                        ),
                    }
                }
                Err(_) => println!("{} vol takes a number in dB", stamp(start)),
            },
            (None, _) => {}
            _ => println!("{} commands: vol <dB>, quit", stamp(start)),
        }
    }
    // A run with no terminal sees stdin end at once: it serves until killed.
    if !quit {
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
    stop.store(true, Ordering::SeqCst);
    let _ = logger.join();
    if let Some((daemon, fullname)) = advertised {
        let _ = daemon.unregister(&fullname);
        let _ = daemon.shutdown();
    }
    println!("{} stopped", stamp(start));
    ExitCode::SUCCESS
}
