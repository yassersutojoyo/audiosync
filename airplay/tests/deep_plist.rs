//! A pairing answer nested a hundred thousand levels deep, as a binary or
//! an XML property list, ends the pairing with a protocol error instead of
//! overflowing the stack of the thread that reads it (P140). The pairing is
//! driven on a thread spawned the way the sender spawns its driver thread,
//! so it has the same stack. A stack overflow aborts the whole process
//! rather than failing one test, so these tests have their own binary:
//! an abort shows up as this binary failing and hides no other test.

mod common;

use std::net::SocketAddr;
use std::thread;
use std::time::Instant;

use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::pairing::{
    self, MAX_PAIRING_BODY_BYTES, Pairing, PairingEvent, PairingState,
};
use audiosync_airplay::rtsp::{MAX_BODY_BYTES, Message, Method, Parser, Request, Response};
use common::atv::CLIENT;
use common::{RAOP_PORT, RECEIVER, ids};

const LEVELS: u32 = 100_000;
/// The stack of a thread spawned without a size, like the sender's driver
/// thread: 2 MiB, std's default on every tier-1 platform.
const DRIVER_STACK: usize = 2 * 1024 * 1024;

/// Arrays nested `levels` deep as a binary plist written by hand: object
/// `i` is an array holding object `i + 1`, the last one is empty, and the
/// top object is the outermost array. References and offsets take `size`
/// bytes (2 or 4).
fn deep_binary(levels: u32, size: usize) -> Vec<u8> {
    let sized = |value: u32| value.to_be_bytes()[4 - size..].to_vec();
    let mut body = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(levels as usize);
    for index in 0..levels {
        offsets.push(u32::try_from(body.len()).expect("a small body"));
        if index + 1 < levels {
            body.push(0xA1);
            body.extend(sized(index + 1));
        } else {
            body.push(0xA0);
        }
    }
    let table = body.len() as u64;
    for offset in offsets {
        body.extend(sized(offset));
    }
    body.extend_from_slice(&[0; 6]);
    // Offset size, then reference size.
    let size = u8::try_from(size).expect("2 or 4");
    body.push(size);
    body.push(size);
    body.extend_from_slice(&u64::from(levels).to_be_bytes());
    // The top object, then where the offset table starts.
    body.extend_from_slice(&0u64.to_be_bytes());
    body.extend_from_slice(&table.to_be_bytes());
    body
}

/// Arrays nested `levels` deep as an XML plist.
fn deep_xml(levels: usize) -> Vec<u8> {
    let mut body = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">".to_vec();
    body.extend(b"<array>".repeat(levels));
    body.extend(b"</array>".repeat(levels));
    body.extend_from_slice(b"</plist>\n");
    body
}

/// What the receiver answers: OPTIONS with the methods AirPlay 1 needs,
/// `/pair-pin-start` with an empty 200, and `/pair-setup-pin` with `body`.
fn answer(request: &Request, body: &[u8]) -> Response {
    let mut response = Response::new(200, "OK");
    response
        .headers
        .push("CSeq", request.headers.get("CSeq").unwrap_or("0"));
    match (request.method, request.uri.as_str()) {
        (Method::Options, _) => response.headers.push("Public", "ANNOUNCE, SETUP, RECORD"),
        (Method::Post, "/pair-setup-pin") => {
            response.body = body.to_vec();
            response
                .headers
                .push("Content-Type", "application/x-apple-binary-plist");
        }
        _ => {}
    }
    response
}

/// A pairing whose first `/pair-setup-pin` answer carries `body`, driven
/// until it settles: its events and where it ended.
fn pair_with(body: &[u8]) -> (Vec<PairingEvent>, PairingState) {
    let now = Instant::now();
    let mut pairing = Pairing::new(
        vec![SocketAddr::new(RECEIVER, RAOP_PORT)],
        ids(),
        CLIENT,
        now,
    );
    let mut parser = Parser::new();
    let mut events = Vec::new();
    let mut rounds = 0;
    loop {
        let mut connect = false;
        while let Some(output) = pairing.poll_output() {
            rounds += 1;
            assert!(rounds < 1_000, "the pairing settles");
            match output {
                pairing::Output::Send(bytes) => {
                    parser.push(&bytes);
                    let mut reply = Vec::new();
                    while let Some(message) = parser.next_message().expect("valid RTSP") {
                        let Message::Request(request) = message else {
                            panic!("the sender sent a response");
                        };
                        reply.extend(answer(&request, body).encode());
                    }
                    pairing.handle_input(&reply, now);
                }
                pairing::Output::Connect(_) => connect = true,
                pairing::Output::Event(event) => events.push(event),
                pairing::Output::Close => {}
            }
        }
        if !connect {
            break;
        }
        pairing.handle_connected(now);
    }
    (events, pairing.state())
}

/// [`pair_with`] on a thread spawned like the sender's driver thread, with
/// a name and the default stack size.
fn pair_on_a_driver_thread(body: Vec<u8>) -> (Vec<PairingEvent>, PairingState) {
    thread::Builder::new()
        .name("deep-plist".to_owned())
        .spawn(move || pair_with(&body))
        .expect("a thread")
        .join()
        .expect("the pairing doesn't panic")
}

fn assert_protocol_error(events: &[PairingEvent], state: PairingState) {
    assert_eq!(state, PairingState::Closed);
    assert!(
        matches!(
            events,
            [PairingEvent::Failed { error, wrong_pin: false }]
                if error.kind == ErrorKind::ProtocolError
        ),
        "{events:?}"
    );
}

// covers: P140
#[test]
fn a_binary_pairing_answer_nested_a_hundred_thousand_deep_is_a_protocol_error() {
    let body = deep_binary(LEVELS, 4);
    assert!(body.len() <= MAX_BODY_BYTES, "{} bytes", body.len());
    let (events, state) = pair_on_a_driver_thread(body);
    assert_protocol_error(&events, state);
}

// covers: P140
#[test]
fn an_xml_pairing_answer_nested_sixty_thousand_deep_is_a_protocol_error() {
    let body = deep_xml(60_000);
    assert!(body.len() <= MAX_BODY_BYTES, "{} bytes", body.len());
    let (events, state) = pair_on_a_driver_thread(body);
    assert_protocol_error(&events, state);
}

// covers: P140
#[test]
fn the_deepest_answer_within_the_body_cap_is_read_on_a_quarter_of_the_stack() {
    // With 2-byte references and offsets a level takes 5 bytes, and the
    // header, the empty innermost array and the trailer 38 more.
    let levels = u32::try_from((MAX_PAIRING_BODY_BYTES - 38) / 5).expect("a small count");
    let body = deep_binary(levels, 2);
    assert!(body.len() <= MAX_PAIRING_BODY_BYTES, "{} bytes", body.len());
    assert!(
        body.len() + 5 > MAX_PAIRING_BODY_BYTES,
        "{} bytes",
        body.len()
    );
    // Read whole and found to be no dictionary, then dropped, with a
    // quarter of the driver thread's stack.
    let (events, state) = thread::Builder::new()
        .name("deep-plist".to_owned())
        .stack_size(DRIVER_STACK / 4)
        .spawn(move || pair_with(&body))
        .expect("a thread")
        .join()
        .expect("the pairing doesn't panic");
    assert_protocol_error(&events, state);
}
