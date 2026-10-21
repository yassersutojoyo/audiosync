//! The sender against a fake device over TLS on 127.0.0.1 (sections 3, 4
//! and 8): handshake, status, launch, heartbeat, takeover and teardown.

mod common;

use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use audiosync_cast::channel::Event;
use audiosync_cast::client::{CastClient, Fingerprint, FingerprintBook, FingerprintChange};
use audiosync_cast::discovery::{CastDevice, DeviceId, DeviceKind};
use audiosync_cast::error::{Backoff, ConnectError, EndReason, LaunchError};
use audiosync_cast::ids::RequestIds;
use audiosync_cast::message::{
    DEFAULT_MEDIA_RECEIVER, NS_CONNECTION, NS_DEVICE_AUTH, NS_HEARTBEAT, NS_RECEIVER, PLATFORM,
    SENDER,
};
use common::{Behavior, Command, FakeDevice, OnLaunch, app, json_message};
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
    // A port nobody listens on.
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = closed.local_addr().expect("address");
    drop(closed);
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

#[test]
fn ipv4_is_tried_before_ipv6() {
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = closed.local_addr().expect("address").port();
    drop(closed);
    let v4 = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port);
    let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port);
    let error = CastClient::connect(&[v6, v4], RequestIds::starting_at(1)).expect_err("closed");
    let ConnectError::Tcp { address, .. } = error else {
        panic!("{error}");
    };
    assert_eq!(address, v6, "IPv6 was tried last");
}

// covers: C3
#[test]
fn a_group_is_never_driven() {
    let group = CastDevice {
        id: DeviceId([1; 16]),
        instance: "Group._googlecast._tcp.local.".to_owned(),
        name: Some("Downstairs".to_owned()),
        model: None,
        version: None,
        icon_path: None,
        kind: DeviceKind::Group,
        port: 32187,
        addresses: vec![IpAddr::from([127, 0, 0, 1])],
        other_keys: Vec::new(),
    };
    assert!(matches!(
        CastClient::connect_device(&group),
        Err(ConnectError::Group)
    ));
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
    assert!(device.wait_for("CONNECT", Duration::from_secs(1)));
    let to_app = device
        .received()
        .into_iter()
        .find(|(destination, _, _)| destination == "transport-1")
        .expect("a message to the app");
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
    assert!(device.wait_for("CLOSE", Duration::from_secs(1)));
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
