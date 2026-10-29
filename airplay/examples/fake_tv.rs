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
//!     [--deviceid <XX:XX:XX:XX:XX:XX>] [--port <n>] [--pin every|once]
//!     [--code <digits>] [--password <text>] [--ptp-only] [--summary]
//! ```
//!
//! `FAKE_TV_LATENCY_US` set to whole microseconds adds that output latency
//! to its `/info` answer; unset or empty, it reports none.
//!
//! Two copies run side by side when each has its own `--deviceid`, since a
//! sender keys a speaker by it; a copy with another id also gets its own
//! default name and mDNS host, and the port is free (0, printed at start)
//! unless `--port` names one.
//!
//! `--password` sets status flags bit 7 and pairs with the password in
//! place of a PIN, a pairing kept across connections; it excludes `--pin`
//! and `--code`.
//!
//! `--ptp-only` sets features bit 41 and answers a session SETUP that asks
//! for NTP with 501, as a receiver that times with PTP only. It binds
//! nothing on the PTP ports and sends no PTP message.
//!
//! Every request line and its answer status goes to stdout; with `--summary`
//! only one plain line per event a dry-run sheet judges, with its running
//! count (the `summary` module). On stdin,
//! `vol <dB>` moves the level as the television's remote does and `quit`
//! stops it.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "fake_tv/options.rs"]
mod options;
#[path = "fake_tv/summary.rs"]
mod summary;
#[path = "fake_tv/television.rs"]
mod television;

use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::rtsp::Request;
use common::ap2::{Ap2Server, FakeAp2, request_key};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use options::{Options, parse};
use summary::Summary;
use television::{Pin, television};

const SERVICE_TYPE: &str = "_airplay._tcp.local.";

fn advertise(options: &Options, port: u16) -> Result<(ServiceDaemon, String), String> {
    let mut entries = television::txt(options.pin, 0, options.device_id);
    if options.ptp_only {
        entries = television::ptp_only_txt(entries);
    }
    let txt: Vec<(&str, &str)> = entries
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let failed = |error: mdns_sd::Error| format!("mDNS: {error}");
    let daemon = ServiceDaemon::new().map_err(failed)?;
    let host = format!("{}.local.", television::host_label(options.device_id));
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
            // Two connections can open within one poll.
            while connections < fake.connections {
                connections += 1;
                match summary {
                    Some(summary) => println!("{}", lock(summary).connection(seconds(start))),
                    None => println!("{} connection {connections} opened", stamp(start)),
                }
            }
            while let (Some((request, framed)), Some(Some(answered))) =
                (fake.requests.get(logged), fake.answered.get(logged))
            {
                if let Some(summary) = summary {
                    let level = answered.level.as_deref();
                    let line =
                        lock(summary).request(seconds(start), request, answered.status, level);
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
    summary::parse_level(std::str::from_utf8(body).ok()?)
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
    let latency_var = std::env::var_os(television::LATENCY_VAR);
    let latency_us = match television::latency_from(
        latency_var
            .as_deref()
            .map(|v| v.to_string_lossy())
            .as_deref(),
    ) {
        Ok(latency_us) => latency_us,
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
    let mut fake = television(
        &options.name,
        options.pin,
        &options.code,
        0,
        options.device_id,
    );
    if let Some(micros) = latency_us {
        television::report_latency(&mut fake, micros);
    }
    if options.ptp_only {
        television::ptp_only(&mut fake);
    }
    let server = Ap2Server::start_at(SocketAddr::new(IpAddr::V4(ip), options.port), fake);
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
    println!(
        "{} identity: deviceid {}, mDNS host {}.local",
        stamp(start),
        television::device_id_text(options.device_id),
        television::host_label(options.device_id)
    );
    match options.pin {
        Pin::None => println!("{} pairing: transient, no PIN", stamp(start)),
        Pin::EveryTime => println!("{} pairing: PIN {} every time", stamp(start), options.code),
        Pin::Once => println!(
            "{} pairing: PIN {} the first time",
            stamp(start),
            options.code
        ),
        Pin::Password => println!(
            "{} pairing: password {}, kept after pairing",
            stamp(start),
            options.code
        ),
    }
    if options.ptp_only {
        println!(
            "{} timing: PTP only (features bit 41; NTP session SETUP answered 501)",
            stamp(start)
        );
    }
    if let Some(micros) = latency_us {
        println!(
            "{} latency: /info reports outputLatencyMicros {micros} ({})",
            stamp(start),
            television::LATENCY_VAR
        );
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
        .then(|| Arc::new(Mutex::new(Summary::new(options.pin == Pin::Password))));
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
