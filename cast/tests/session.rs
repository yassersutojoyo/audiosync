//! The sender against a fake device over TLS on 127.0.0.1 (sections 3, 4
//! and 8): handshake, status, launch, heartbeat, takeover and teardown.

mod common;

use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_cast::channel::Event;
use audiosync_cast::client::{
    CastClient, Fingerprint, FingerprintBook, FingerprintChange, MAX_EVENTS, QUIT_BUDGET,
};
use audiosync_cast::discovery::DeviceId;
use audiosync_cast::error::{Backoff, ConnectError, EndReason, LaunchError, ReconnectPacer};
use audiosync_cast::host::SHUTDOWN_MARGIN;
use audiosync_cast::ids::RequestIds;
use audiosync_cast::message::{
    DEFAULT_MEDIA_RECEIVER, NS_CONNECTION, NS_DEVICE_AUTH, NS_HEARTBEAT, NS_RECEIVER, PLATFORM,
    SENDER,
};
use common::{Behavior, Command, FakeDevice, OnLaunch, ServerKey, app, json_message};
use serde_json::json;

fn connect(device: &FakeDevice) -> CastClient {
    CastClient::connect(&[device.address], RequestIds::starting_at(10)).expect("connects")
}

fn drain(client: &mut CastClient) -> Vec<Event> {
    std::iter::from_fn(|| client.poll_event()).collect()
}

// covers: C19, C21
#[test]
fn a_self_signed_certificate_for_another_name_is_accepted_and_fingerprinted() {
    let device = FakeDevice::start(Behavior::default());
    let client = connect(&device);
    assert_eq!(client.address(), device.address);
    assert_eq!(
        client.fingerprint(),
        Some(Fingerprint::of(&device.certificate_der))
    );
    assert!(client.local_address().ip().is_loopback());
    assert!(client.status().is_some());
    let _ = client.stop(Duration::from_secs(1));
    device.join();

    let id = DeviceId([7; 16]);
    let mut book = FingerprintBook::new();
    let first = Fingerprint([1; 32]);
    let second = Fingerprint([2; 32]);
    assert_eq!(book.record(id, first), FingerprintChange::First);
    assert_eq!(book.record(id, first), FingerprintChange::Same);
    assert_eq!(
        book.record(id, second),
        FingerprintChange::Changed { before: first },
        "a changed fingerprint is reported, and the connection above was never refused for it"
    );
}

#[test]
fn a_tls_1_2_only_device_connects() {
    let device = FakeDevice::start(Behavior {
        tls12_only: true,
        ..Behavior::default()
    });
    let client = connect(&device);
    assert!(client.status().is_some());
    let _ = client.stop(Duration::from_secs(1));
    device.join();
}

// C20's ECDHE-RSA AES-GCM half; the fake device's rustls has no CBC suites to
// serve, so the ClientHello test below shows the sender offers them.
#[test]
fn a_tls_1_2_device_with_an_rsa_key_connects_on_each_ecdhe_rsa_aes_gcm_suite() {
    use rustls::crypto::ring::cipher_suite::{
        TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    };
    for suite in [
        TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    ] {
        let device = FakeDevice::start(Behavior {
            tls12_only: true,
            key: ServerKey::Rsa,
            suite: Some(suite),
            ..Behavior::default()
        });
        let client = connect(&device);
        assert!(client.status().is_some(), "{:?}", suite.suite());
        assert_eq!(
            client.fingerprint(),
            Some(Fingerprint::of(&device.certificate_der))
        );
        assert_eq!(device.negotiated_suites(), vec![suite.suite()]);
        let _ = client.stop(Duration::from_secs(1));
        device.join();
    }
}

/// The legacy version and the cipher suites of the first ClientHello sent to
/// `listener`, read from its TLS record (RFC 5246, sections 6.2.1 and
/// 7.4.1.2).
fn client_hello(listener: TcpListener) -> ([u8; 2], Vec<u16>) {
    use std::io::Read;
    let (mut stream, _) = listener.accept().expect("accept");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).expect("record header");
    assert_eq!(header[0], 22, "a handshake record");
    let mut body = vec![0u8; usize::from(u16::from_be_bytes([header[3], header[4]]))];
    stream.read_exact(&mut body).expect("record body");
    assert_eq!(body[0], 1, "a ClientHello");
    // The handshake header (4 bytes), the version (2) and the random (32)
    // come before the session ID's length byte.
    let version = [body[4], body[5]];
    let session_id = 4 + 2 + 32;
    let suites = session_id + 1 + usize::from(body[session_id]);
    let length = usize::from(u16::from_be_bytes([body[suites], body[suites + 1]]));
    let offered = body[suites + 2..suites + 2 + length]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    (version, offered)
}

// covers: C20
// The test above connects on each ECDHE-RSA AES-GCM suite; this one shows the
// platform TLS stack also offers the AES-CBC-SHA suite older devices pick.
#[test]
fn the_client_hello_offers_tls_1_2_with_ecdhe_rsa_aes_gcm_and_aes_cbc_sha() {
    const ECDHE_RSA_AES_128_GCM_SHA256: u16 = 0xC02F;
    const ECDHE_RSA_AES_256_GCM_SHA384: u16 = 0xC030;
    const ECDHE_RSA_AES_256_CBC_SHA: u16 = 0xC014;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let device = thread::spawn(move || client_hello(listener));
    let error = CastClient::connect(&[address], RequestIds::starting_at(1)).expect_err("no answer");
    assert!(matches!(error, ConnectError::Tls { .. }), "{error}");
    let (version, suites) = device.join().expect("a ClientHello");
    assert_eq!(version, [3, 3], "TLS 1.2, or 1.3 behind it");
    let mut wanted = vec![ECDHE_RSA_AES_128_GCM_SHA256, ECDHE_RSA_AES_256_GCM_SHA384];
    // Schannel's suite list is Windows policy, and Windows Server 2025 leaves the CBC suites out;
    // a client SKU still has to offer it, so a policy change there fails here.
    if !cfg!(windows) || windows_installation_type() == "Client" {
        wanted.push(ECDHE_RSA_AES_256_CBC_SHA);
    } else {
        eprintln!(
            "warning: a Windows server SKU, the CBC suite is not required; offered {suites:04x?}"
        );
    }
    for suite in wanted {
        assert!(suites.contains(&suite), "{suite:#06x} in {suites:04x?}");
    }
}

/// "Client", "Server" or "Server Core", from the registry through reg.exe.
fn windows_installation_type() -> String {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "/v",
            "InstallationType",
        ])
        .output()
        .expect("reg.exe runs");
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("InstallationType") && words.next() == Some("REG_SZ"))
                .then(|| words.collect::<Vec<_>>().join(" "))
        })
        .unwrap_or_else(|| panic!("no InstallationType in {text}"))
}

#[test]
fn the_windows_installation_type_is_read() {
    if cfg!(windows) {
        let kind = windows_installation_type();
        assert!(
            ["Client", "Server", "Server Core"].contains(&kind.as_str()),
            "{kind}"
        );
    }
}

#[test]
fn every_message_on_the_wire_is_one_length_prefixed_cast_message() {
    let device = FakeDevice::start(Behavior::default());
    let client = connect(&device);
    let received = device.received();
    assert_eq!(received[0].0, PLATFORM);
    assert_eq!(received[0].1, NS_CONNECTION);
    assert_eq!(received[0].2, json!({"type": "CONNECT"}));
    assert_eq!(received[1].1, NS_RECEIVER);
    assert_eq!(
        received[1].2,
        json!({"type": "GET_STATUS", "requestId": 10})
    );
    let _ = client.stop(Duration::from_secs(1));
    let log = device.log.lock().expect("log").clone();
    assert!(log.iter().all(|message| message.source_id == SENDER));
    device.join();
}

#[test]
fn a_device_that_never_reports_its_status_is_a_connect_failure() {
    let device = FakeDevice::start(Behavior {
        answer_status: false,
        ..Behavior::default()
    });
    let started = Instant::now();
    let error =
        CastClient::connect(&[device.address], RequestIds::starting_at(1)).expect_err("no status");
    assert!(matches!(error, ConnectError::NoStatus), "{error}");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(5) && waited < Duration::from_secs(8),
        "{waited:?}"
    );
    device.join();
}

#[test]
fn connect_failures_say_their_cause_and_retries_back_off() {
    let address = common::refused_address();
    let error = CastClient::connect(&[address], RequestIds::starting_at(1)).expect_err("refused");
    assert!(matches!(error, ConnectError::Tcp { .. }), "{error}");
    assert!(error.to_string().contains(&address.to_string()));
    assert!(matches!(
        CastClient::connect(&[], RequestIds::starting_at(1)),
        Err(ConnectError::NoAddress)
    ));

    // A plain TCP server that isn't TLS fails the handshake with a reason.
    let plain = TcpListener::bind("127.0.0.1:0").expect("bind");
    let plain_address = plain.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        use std::io::Write;
        if let Ok((mut stream, _)) = plain.accept() {
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        }
    });
    let error =
        CastClient::connect(&[plain_address], RequestIds::starting_at(1)).expect_err("no TLS");
    assert!(matches!(error, ConnectError::Tls { .. }), "{error}");
    let _ = server.join();

    let mut backoff = Backoff::new();
    let waits: Vec<u64> = (0..8).map(|_| backoff.next_wait().as_secs()).collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
    backoff.reset();
    assert_eq!(backoff.next_wait(), Duration::from_secs(1));
}

/// Runs [`CastClient::connect_while`] on `addresses` and clears its
/// `selected` flag after `delay`. The end is reported by the connect itself;
/// [`HANG_GUARD`] only catches a connect that never ends, so a busy machine
/// can't fail the test.
fn connect_deselected_after(addresses: &[SocketAddr], delay: Duration) -> ConnectError {
    let selected = Arc::new(AtomicBool::new(true));
    let (sender, ended) = mpsc::channel();
    let flag = Arc::clone(&selected);
    let addresses = addresses.to_vec();
    let connecting = thread::spawn(move || {
        let result = CastClient::connect_while(&addresses, RequestIds::starting_at(1), &flag);
        let _ = sender.send(result.err());
    });
    thread::sleep(delay);
    selected.store(false, Ordering::SeqCst);
    let error = ended
        .recv_timeout(HANG_GUARD)
        .expect("the connect ends after the deselect")
        .expect("deselected, not connected");
    connecting.join().expect("connect thread");
    error
}

/// Far beyond every connect timeout added up (TCP and TLS 5 s each, plus
/// the status wait), so only a connect that ignores the deselect hits it.
const HANG_GUARD: Duration = Duration::from_secs(20);

// Partly C80 and C88: a deselect ends a connect at once, whatever step it
// waits on, so a quit never waits out the timeouts of every address. Each
// step that times out reports its own error, so Stopped shows the deselect
// ended it.
#[test]
fn a_deselect_ends_a_connect_while_it_waits() {
    let silent = FakeDevice::start(Behavior {
        answer_status: false,
        ..Behavior::default()
    });
    let error = connect_deselected_after(&[silent.address], Duration::from_millis(1_000));
    assert!(
        matches!(error, ConnectError::Ended(EndReason::Stopped)),
        "{error}"
    );

    // A documentation address (RFC 5737) that no device answers: the TCP
    // connect waits up to its timeout, or fails at once without a route.
    let nowhere = SocketAddr::new(IpAddr::from([192, 0, 2, 1]), 8009);
    let error = connect_deselected_after(&[nowhere, silent.address], Duration::from_millis(300));
    assert!(
        matches!(
            &error,
            ConnectError::Ended(EndReason::Stopped) | ConnectError::Tcp { .. }
        ),
        "{error}"
    );
    silent.join();

    // TCP is accepted (the listen backlog does it) but the TLS handshake
    // never gets an answer.
    let stalled = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = stalled.local_addr().expect("address");
    let error = connect_deselected_after(&[address], Duration::from_millis(300));
    assert!(
        matches!(error, ConnectError::Ended(EndReason::Stopped)),
        "{error}"
    );
}

#[test]
fn ipv4_is_tried_before_ipv6() {
    let v4 = common::refused_address();
    let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), v4.port());
    let error = CastClient::connect(&[v6, v4], RequestIds::starting_at(1)).expect_err("closed");
    let ConnectError::Tcp { address, .. } = error else {
        panic!("{error}");
    };
    assert_eq!(
        address, v4,
        "IPv4 was tried first, and its error is the one reported"
    );
}

#[test]
fn a_host_that_never_polls_keeps_a_bounded_event_queue() {
    let device = FakeDevice::start(Behavior::default());
    let mut client = connect(&device);
    for _ in 0..(MAX_EVENTS * 2) {
        device.send(Command::Send(json_message(
            PLATFORM,
            "*",
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "requestId": 0, "status": {}}),
        )));
    }
    client.run_for(Duration::from_millis(500));
    let events = drain(&mut client);
    assert!(events.len() <= MAX_EVENTS, "{} events", events.len());
    assert!(
        events.iter().any(|event| matches!(event, Event::Ready(_))),
        "events other than status reports are kept"
    );
    let _ = client.stop(Duration::from_secs(1));
    device.join();
}

// covers: C4
#[test]
fn launch_only_ever_starts_the_default_media_receiver() {
    let device = FakeDevice::start(Behavior::default());
    let mut client = connect(&device);
    let app = client.launch().expect("launched");
    assert_eq!(app.session_id, "session-1");
    assert_eq!(app.transport_id, "transport-1");
    let received = device.received();
    let launches: Vec<_> = received
        .iter()
        .filter(|(_, _, payload)| payload["type"] == "LAUNCH")
        .collect();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].0, PLATFORM);
    assert_eq!(launches[0].2["appId"], DEFAULT_MEDIA_RECEIVER);
    // The sender has written the app's CONNECT; the fake device may not
    // have read it yet.
    let until = Instant::now() + Duration::from_secs(2);
    let to_app = loop {
        let found = device
            .received()
            .into_iter()
            .find(|(destination, _, _)| destination == "transport-1");
        match found {
            Some(message) => break message,
            None if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            None => panic!("no message to the app"),
        }
    };
    assert_eq!(
        to_app.2,
        json!({"type": "CONNECT"}),
        "CONNECT before anything else"
    );
    let _ = client.stop(Duration::from_secs(2));
    device.join();
}

#[test]
fn a_running_default_media_receiver_is_reused_over_the_wire() {
    let device = FakeDevice::start(Behavior {
        apps: vec![app(DEFAULT_MEDIA_RECEIVER, "kept", "kept-transport")],
        ..Behavior::default()
    });
    let mut client = connect(&device);
    let app = client.launch().expect("reused");
    assert_eq!(app.session_id, "kept");
    assert!(!device.types().iter().any(|t| t == "LAUNCH"));
    let _ = client.stop(Duration::from_secs(2));
    device.join();
}

#[test]
fn a_launch_after_a_refused_one_waits_for_its_own_answer() {
    let device = FakeDevice::start(Behavior {
        on_launch: OnLaunch::RefuseOnce("NOT_ALLOWED"),
        ..Behavior::default()
    });
    let mut client = connect(&device);
    assert!(matches!(client.launch(), Err(LaunchError::Refused { .. })));
    // The host retries without draining the events first.
    let app = client.launch().expect("the second launch starts the app");
    assert_eq!(app.session_id, "session-2");
    assert_eq!(device.types().iter().filter(|t| *t == "LAUNCH").count(), 2);
    let _ = client.stop(Duration::from_secs(2));
    device.join();
}

#[test]
fn a_launch_after_a_timed_out_one_succeeds() {
    let device = FakeDevice::start(Behavior {
        on_launch: OnLaunch::IgnoreOnce,
        ..Behavior::default()
    });
    let mut client = connect(&device);
    // Device pings keep the channel alive through the 15 s launch timeout.
    let pinger = {
        let commands = device.commands();
        std::thread::spawn(move || {
            for _ in 0..40 {
                let ping = json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"type": "PING"}));
                if commands.send(Command::Send(ping)).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })
    };
    assert_eq!(client.launch(), Err(LaunchError::Timeout));
    let app = client.launch().expect("the retry starts the app");
    assert_eq!(app.session_id, "session-2");
    let _ = client.stop(Duration::from_secs(2));
    device.join();
    let _ = pinger.join();
}

#[test]
fn launch_errors_over_the_wire() {
    let device = FakeDevice::start(Behavior {
        on_launch: OnLaunch::CancelOnce,
        ..Behavior::default()
    });
    let mut client = connect(&device);
    assert!(client.launch().is_ok(), "CANCELLED is retried once");
    assert_eq!(device.types().iter().filter(|t| *t == "LAUNCH").count(), 2);
    let _ = client.stop(Duration::from_secs(2));
    device.join();

    let device = FakeDevice::start(Behavior {
        on_launch: OnLaunch::Refuse("NOT_ALLOWED"),
        ..Behavior::default()
    });
    let mut client = connect(&device);
    assert_eq!(
        client.launch(),
        Err(LaunchError::Refused {
            reason: "NOT_ALLOWED".to_owned()
        })
    );
    let _ = client.stop(Duration::from_secs(1));
    device.join();
}

#[test]
fn the_sender_answers_device_pings_over_the_wire() {
    let device = FakeDevice::start(Behavior::default());
    let mut client = connect(&device);
    device.send(Command::Send(json_message(
        PLATFORM,
        SENDER,
        NS_HEARTBEAT,
        json!({"type": "PING"}),
    )));
    client.run_for(Duration::from_millis(300));
    assert!(device.wait_for("PONG", Duration::from_secs(1)));
    let _ = client.stop(Duration::from_secs(1));
    device.join();
}

#[test]
fn a_device_auth_message_from_the_device_is_ignored_over_the_wire() {
    let device = FakeDevice::start(Behavior::default());
    let mut client = connect(&device);
    device.send(Command::Send(json_message(
        PLATFORM,
        SENDER,
        NS_DEVICE_AUTH,
        json!({}),
    )));
    client.run_for(Duration::from_millis(300));
    assert!(!client.is_ended());
    assert!(
        device
            .received()
            .iter()
            .all(|(_, namespace, _)| namespace != NS_DEVICE_AUTH)
    );
    let _ = client.stop(Duration::from_secs(1));
    device.join();
}

#[test]
fn another_sender_taking_over_ends_the_session() {
    let device = FakeDevice::start(Behavior::default());
    let mut client = connect(&device);
    client.launch().expect("launched");
    device.send(Command::TakeOver);
    client.run_for(Duration::from_secs(1));
    assert!(client.is_ended());
    assert!(drain(&mut client).contains(&Event::Ended(EndReason::TakenOver)));
    assert_eq!(
        device.types().iter().filter(|t| *t == "LAUNCH").count(),
        1,
        "no relaunch"
    );
    device.join();
}

#[test]
fn a_broken_frame_from_the_device_closes_the_channel() {
    let device = FakeDevice::start(Behavior::default());
    let mut client = connect(&device);
    let mut bytes = (70_000u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(&[0; 16]);
    device.send(Command::SendRaw(bytes));
    client.run_for(Duration::from_secs(1));
    assert!(client.is_ended());
    assert!(
        drain(&mut client)
            .iter()
            .any(|event| matches!(event, Event::Ended(EndReason::Fault(_))))
    );
    device.join();
}

#[test]
fn stop_tears_down_in_order_over_the_wire() {
    let device = FakeDevice::start(Behavior::default());
    let mut client = connect(&device);
    client.launch().expect("launched");
    let started = Instant::now();
    assert_eq!(client.stop(Duration::from_secs(2)), EndReason::Stopped);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(device.wait_for_count("CLOSE", 2, Duration::from_secs(2)));
    let tail: Vec<(String, String)> = device
        .received()
        .into_iter()
        .filter(|(_, _, payload)| payload["type"] == "STOP" || payload["type"] == "CLOSE")
        .map(|(destination, _, payload)| {
            (
                destination,
                payload["type"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        tail,
        [
            (PLATFORM.to_owned(), "STOP".to_owned()),
            ("transport-1".to_owned(), "CLOSE".to_owned()),
            (PLATFORM.to_owned(), "CLOSE".to_owned()),
        ]
    );
    device.join();
}

// covers: C88
// A device that stops reading fills the socket buffers until a write
// blocks: that write gives up well inside the margin a quit has, and the
// teardown after it keeps to the quit budget.
#[test]
fn a_device_that_stops_reading_holds_no_write_or_teardown_past_the_quit_budget() {
    let device = FakeDevice::start(Behavior::default());
    let mut client = connect(&device);
    device.send(Command::Stall);
    thread::sleep(Duration::from_millis(100));
    let until = Instant::now() + Duration::from_secs(30);
    let mut longest = Duration::ZERO;
    while !client.is_ended() && Instant::now() < until {
        let started = Instant::now();
        client.request_members();
        longest = longest.max(started.elapsed());
    }
    assert!(client.is_ended(), "no write ever blocked");
    assert!(
        longest < SHUTDOWN_MARGIN,
        "one write blocked for {longest:?}"
    );
    let started = Instant::now();
    let _ = client.stop(QUIT_BUDGET);
    let took = started.elapsed();
    assert!(
        took < QUIT_BUDGET + SHUTDOWN_MARGIN,
        "the teardown took {took:?}"
    );
    device.join();
}

/// Reconnects to a receiver that closes each channel right after its first
/// status, on a test clock from `start`: 50 ms per connect, 10 ms of life
/// after it. The times of the attempts before `start + span`.
fn reconnects_within(pacer: &mut ReconnectPacer, start: Instant, span: Duration) -> Vec<Duration> {
    let mut now = start;
    let mut attempts = Vec::new();
    loop {
        now += pacer.next_wait(now);
        if now >= start + span {
            return attempts;
        }
        attempts.push(now - start);
        now += Duration::from_millis(50);
        pacer.connected(now);
        now += Duration::from_millis(10);
    }
}

fn gaps_in_seconds(attempts: &[Duration]) -> Vec<u64> {
    attempts
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).as_secs())
        .collect()
}

// covers: C81
#[test]
fn reconnects_to_a_receiver_that_keeps_closing_are_spaced_out() {
    let start = Instant::now();
    let mut pacer = ReconnectPacer::default();
    let attempts = reconnects_within(&mut pacer, start, Duration::from_secs(10));
    assert!(attempts.len() <= 4, "{} reconnects in 10 s", attempts.len());
    assert_eq!(
        gaps_in_seconds(&attempts),
        [1, 2, 4],
        "at once, then 1, 2, 4 s"
    );
    let later = gaps_in_seconds(&reconnects_within(
        &mut pacer,
        start,
        Duration::from_secs(40),
    ));
    assert!(later.iter().all(|gap| *gap <= 10), "{later:?}");
    assert!(later.ends_with(&[10, 10]), "{later:?}");

    // 60 s on a working channel start the spacing over.
    let mut pacer = ReconnectPacer::default();
    reconnects_within(&mut pacer, start, Duration::from_secs(10));
    let healthy = start + Duration::from_secs(10);
    pacer.connected(healthy);
    let after = healthy + Duration::from_secs(60);
    assert_eq!(pacer.next_wait(after), Duration::ZERO);
    assert_eq!(pacer.next_wait(after), Duration::from_secs(1));
}

// covers: C81
#[test]
fn an_announcement_of_the_receiver_retries_at_once_and_starts_the_spacing_over() {
    let start = Instant::now();
    let mut pacer = ReconnectPacer::default();
    let attempts = reconnects_within(&mut pacer, start, Duration::from_secs(30));
    assert!(attempts.len() >= 5, "{attempts:?}");
    let now = start + Duration::from_secs(30);
    assert!(pacer.next_wait(now) > Duration::ZERO, "spaced before");
    pacer.announced();
    assert_eq!(pacer.next_wait(now), Duration::ZERO, "at once");
    assert_eq!(
        pacer.next_wait(now),
        Duration::from_secs(1),
        "from the start"
    );
}
