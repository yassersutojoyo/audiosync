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
//!     [--homepod] [--dump <dir>] [--loss <percent>]
//! ```
//!
//! `--homepod` advertises and answers `/info` as a HomePod mini (model
//! `AudioAccessory5,1`, AirPlay 2 with HomeKit and transient pairing,
//! buffered audio and PTP in its features) and takes a buffered stream
//! (type 103) on a TCP port of its own. `--dump <dir>` writes every request
//! it answers, decrypted, to a numbered file in `<dir>` with the answer, and
//! logs each stream SETUP's latency keys, the timing peers, any
//! SETRATEANCHORTIME anchor and the first packet on each audio port with the
//! system clock at arrival. To measure a sender on a Mac, on that Mac:
//! `cargo run -p audiosync-airplay --example fake_tv -- --lan --homepod --dump <dir>`.
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
//! count (the `summary` module).
//!
//! On the realtime stream (type 96, UDP), which both the television and
//! `--homepod` answer when a sender asks for it, every sync packet on the
//! control port shows the latency the sender declares, its next-packet RTP
//! time minus the RTP time it says is heard now: one `latency:` line at
//! the stream start, one on each change (old -> new, in samples and ms at
//! 44.1 kHz) and one a minute, each with the unix time. `--loss <percent>`
//! drops that share of the audio packets, spread evenly, and asks the sender
//! for each one again on the control port as a receiver does; `loss:` lines
//! give each switch and, every 5 s while they move, the dropped packets,
//! resend requests and resends received. A buffered stream (type 103, TCP)
//! is reliable and has no resend requests, so loss leaves it alone.
//!
//! On stdin, `vol <dB>` moves the level as the television's remote does,
//! `loss <percent>` and `loss off` switch the packet loss, `hide` sends
//! the mDNS goodbye while the session already playing goes on, `show`
//! advertises it again, and `quit` stops it, goodbye included, as Ctrl+C does.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "fake_tv/dump.rs"]
mod dump;
#[path = "fake_tv/media.rs"]
mod media;
#[path = "fake_tv/options.rs"]
mod options;
#[path = "fake_tv/realtime.rs"]
mod realtime;
#[path = "fake_tv/stdin.rs"]
mod stdin;
#[path = "fake_tv/summary.rs"]
mod summary;
#[path = "fake_tv/television.rs"]
mod television;

use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use audiosync_airplay::rtsp::Request;
use common::ap2::{Ap2Server, FakeAp2, OTHER_METHOD, request_key};
use dump::Dump;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo, UnregisterStatus};
use options::{Options, parse};
use realtime::{Counts, Loss};
use stdin::Line;
use summary::Summary;
use television::{Pin, television};

const SERVICE_TYPE: &str = "_airplay._tcp.local.";

fn advertise(options: &Options, port: u16) -> Result<(ServiceDaemon, ServiceInfo), String> {
    let mut entries = if options.homepod {
        television::homepod_txt(options.pin, options.device_id)
    } else {
        television::txt(options.pin, options.device_id)
    };
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
    daemon.register(info.clone()).map_err(failed)?;
    Ok((daemon, info))
}

/// Unregisters and waits until the goodbye (TTL 0) went out, so a quit right
/// after does not end the process first; mdns-sd repeats it 120 ms later.
fn goodbye(daemon: &ServiceDaemon, fullname: &str) -> &'static str {
    let status = daemon
        .unregister(fullname)
        .ok()
        .and_then(|status| status.recv_timeout(Duration::from_secs(1)).ok());
    thread::sleep(Duration::from_millis(250));
    match status {
        Some(UnregisterStatus::OK) => "goodbye sent",
        _ => "goodbye not confirmed",
    }
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

/// A loss switch line, with the wall clock to set against a sender's own
/// latency readout.
fn loss_line(start: Instant, percent: f64, counts: Counts) -> String {
    let state = if percent > 0.0 {
        format!("on, {percent}% of realtime audio packets dropped and asked for again")
    } else {
        "off".to_owned()
    };
    format!(
        "{} loss: {state}, unix {:.6}; so far {} dropped, {} resend requests, {} resends received",
        stamp(start),
        dump::unix(SystemTime::now()),
        counts.dropped,
        counts.requested,
        counts.resent
    )
}

/// Every 5 s in which a loss count moved, one line with the counts.
fn report_loss(loss: Arc<Loss>, start: Instant) {
    thread::spawn(move || {
        let mut last = Counts::default();
        loop {
            thread::sleep(Duration::from_secs(5));
            let counts = loss.counts();
            if counts != last {
                last = counts;
                println!(
                    "{} loss: {} dropped, {} resend requests, {} resends received",
                    stamp(start),
                    counts.dropped,
                    counts.requested,
                    counts.resent
                );
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
    let method = request.headers.get(OTHER_METHOD);
    let mut line = format!(
        "{} {}",
        method.unwrap_or(request.method.as_str()),
        request.uri
    );
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
    mut dump: Option<Dump>,
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
                if let Some(dump) = dump.as_mut() {
                    match dump.record(request, answered) {
                        Ok(lines) => lines
                            .iter()
                            .for_each(|line| println!("{} {line}", stamp(start))),
                        Err(error) => eprintln!("fake_tv: --dump: {error}"),
                    }
                }
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
    let buffered = if options.homepod {
        match TcpListener::bind((ip, 0)) {
            Ok(listener) => Some(listener),
            Err(error) => {
                eprintln!("fake_tv: buffered audio port: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let mut fake = match &buffered {
        Some(listener) => television::homepod(
            &options.name,
            options.pin,
            &options.code,
            options.device_id,
            listener.local_addr().map_or(0, |address| address.port()),
        ),
        None => television(
            &options.name,
            options.pin,
            &options.code,
            0,
            options.device_id,
        ),
    };
    if let Some(micros) = latency_us {
        television::report_latency(&mut fake, micros);
    }
    if options.ptp_only {
        television::ptp_only(&mut fake);
    }
    if options.homepod {
        fake.second_info_body = Some(fake.info_body.clone());
    }
    let dump = match &options.dump {
        Some(dir) => match Dump::create(dir) {
            Ok(dump) => Some(dump),
            Err(error) => {
                eprintln!("fake_tv: --dump {}: {error}", dir.display());
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let sockets = (UdpSocket::bind((ip, 0)), UdpSocket::bind((ip, 0)));
    let (data, control, requests) = match sockets {
        (Ok(data), Ok(control)) => match control.try_clone() {
            Ok(requests) => (data, control, requests),
            Err(error) => {
                eprintln!("fake_tv: audio port: {error}");
                return ExitCode::FAILURE;
            }
        },
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("fake_tv: audio port: {error}");
            return ExitCode::FAILURE;
        }
    };
    fake.data_port = data.local_addr().map(|address| address.port()).ok();
    fake.control_port = control.local_addr().map(|address| address.port()).ok();
    let loss = Arc::new(Loss::new(requests));
    loss.set(options.loss);
    media::watch_udp(data, false, start, dump.is_some(), loss.clone());
    media::watch_udp(control, true, start, dump.is_some(), loss.clone());
    report_loss(loss.clone(), start);
    if let Some(listener) = buffered {
        media::watch_tcp(listener, start);
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
        Some((_, info)) => println!(
            "{} mode: LAN, serving \"{}\" on every interface, port {port}, advertised as {}",
            stamp(start),
            options.name,
            info.get_fullname()
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
    if options.homepod {
        println!(
            "{} profile: HomePod, model {}, features {:#x}, buffered audio on a TCP port",
            stamp(start),
            television::HOMEPOD_MODEL,
            television::HOMEPOD_FEATURES
        );
    }
    if let Some(dir) = &options.dump {
        println!(
            "{} dump: every request to {}, one numbered file each",
            stamp(start),
            dir.display()
        );
    }
    if let Some(micros) = latency_us {
        println!(
            "{} latency: /info reports outputLatencyMicros {micros} ({})",
            stamp(start),
            television::LATENCY_VAR
        );
    }
    if options.loss > 0.0 {
        println!("{}", loss_line(start, options.loss, loss.counts()));
    }
    println!(
        "{} stdin: vol <dB> moves the level, loss <percent> and loss off switch packet loss, hide and show the mDNS record, quit or Ctrl+C stops",
        stamp(start)
    );
    if let Some((daemon, info)) = &advertised {
        watch_advertisement(daemon, info.get_fullname().to_owned(), start);
    }

    let server = Arc::new(server);
    let stop = Arc::new(AtomicBool::new(false));
    let summary = options
        .summary
        .then(|| Arc::new(Mutex::new(Summary::new(options.pin == Pin::Password))));
    let logger = {
        let (server, stop, summary) = (server.clone(), stop.clone(), summary.clone());
        thread::spawn(move || log_requests(&server, start, &stop, summary.as_deref(), dump))
    };
    let (lines, input) = mpsc::channel();
    let interrupt = lines.clone();
    if let Err(error) = ctrlc::set_handler(move || {
        let _ = interrupt.send("quit".to_string());
    }) {
        eprintln!("fake_tv: Ctrl+C stops it without the mDNS goodbye: {error}");
    }
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let mut quit = false;
    let mut shown = true;
    for line in input {
        match stdin::read(&line) {
            Line::Quit => {
                quit = true;
                break;
            }
            Line::Vol(db) => {
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
            Line::BadVol => println!("{} vol takes a number in dB", stamp(start)),
            Line::Loss(percent) => {
                loss.set(percent);
                println!("{}", loss_line(start, percent, loss.counts()));
            }
            Line::BadLoss => println!(
                "{} loss takes a percent from 0 to 100, or off",
                stamp(start)
            ),
            command @ (Line::Hide | Line::Show) => match &advertised {
                None => println!(
                    "{} hide and show need --lan: loopback advertises nothing",
                    stamp(start)
                ),
                Some(_) if (command == Line::Hide) != shown => println!(
                    "{} mDNS: already {}",
                    stamp(start),
                    if shown { "advertised" } else { "hidden" }
                ),
                Some((daemon, info)) if shown => {
                    // An unconfirmed unregister may still have removed the record.
                    shown = false;
                    println!(
                        "{} mDNS: {}, the session keeps playing",
                        stamp(start),
                        goodbye(daemon, info.get_fullname())
                    );
                }
                Some((daemon, info)) => {
                    shown = daemon.register(info.clone()).is_ok();
                    println!(
                        "{} mDNS: {} {}",
                        stamp(start),
                        if shown {
                            "advertised again as"
                        } else {
                            "advertising failed for"
                        },
                        info.get_fullname()
                    );
                }
            },
            Line::Empty => {}
            Line::Unknown => println!("{} commands: {}", stamp(start), stdin::COMMANDS),
        }
    }
    // Only reached when the Ctrl+C handler failed to install: it serves until killed.
    if !quit {
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
    stop.store(true, Ordering::SeqCst);
    let _ = logger.join();
    if let Some((daemon, info)) = advertised {
        if shown {
            println!(
                "{} mDNS: {}",
                stamp(start),
                goodbye(&daemon, info.get_fullname())
            );
        }
        let _ = daemon.shutdown();
    }
    println!("{} stopped", stamp(start));
    ExitCode::SUCCESS
}
