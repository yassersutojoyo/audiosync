//! No bytes from a receiver or the network make the sender panic.
//!
//! Every parser that reads such bytes gets thousands of deterministic
//! inputs: valid messages cut at every length and split at every point,
//! random bytes, mutations of valid messages, oversized lengths and counts,
//! boundary numbers, bytes that aren't UTF-8 and mixed line ends. Each must
//! return an error, `None` or a value; a misbehaving receiver is a protocol
//! error (P140), never a crash of the host app. The session, the pairing
//! and the stream get the same kind of input as answers to their real
//! requests, so the parsers are reached the way a receiver reaches them.
//! The inputs come from a fixed-seed xorshift generator, so a failure shows
//! up the same way on every run.

mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use audiosync_airplay::clock::NtpTime;
use audiosync_airplay::digest::{self, Challenge};
use audiosync_airplay::discovery::mdns::{browse_event, instance_name};
use audiosync_airplay::discovery::txt::{parse_bool, parse_int};
use audiosync_airplay::discovery::{
    AirPlayTxt, BrowseEvent, Device, DeviceId, DeviceKey, Directory, DirectoryEvent, DnsRecord,
    Features, RaopTxt, Resolver, ScopedAddr, ServiceKind, ServiceRecord, StatusFlags, Txt,
    parse_raop_instance,
};
use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::pairing::srp::{self, SrpGroup};
use audiosync_airplay::pairing::{
    self, Credentials, LegacyVerifier, MAX_PAIRING_BODY_BYTES, Pairing, PairingEvent, PairingState,
    keys,
};
use audiosync_airplay::rtp::{self, ResendRequest, RtpHeader, SyncPacket, TimingPacket};
use audiosync_airplay::rtsp::{
    self, Headers, JackStatus, MAX_BODY_BYTES, MAX_HEAD_BYTES, Message, Method, ParseError, Parser,
    Request, Response, TransportPorts,
};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::session::{self, Session, SessionConfig, Verifier};
use audiosync_airplay::store::HISTORY_PACKETS;
use audiosync_airplay::stream::MAX_RESEND_REQUEST;
use audiosync_airplay::volume::{self, Volume};
use common::atv::{CLIENT, FakeAppleTv, TestEntropy};
use common::rig::{Rig, Rng, speaker};
use common::{FakeReceiver, PasswordGate, RAOP_PORT, RECEIVER, SENDER, config, connection, ids};
use mdns_sd::{ServiceEvent, ServiceInfo, TxtProperty};
use plist::{Dictionary, Value};

// ---- Inputs -------------------------------------------------------------

fn below(rng: &mut Rng, n: usize) -> usize {
    (rng.next_u64() % n.max(1) as u64) as usize
}

fn one_in(rng: &mut Rng, n: u64) -> bool {
    rng.next_u64().is_multiple_of(n)
}

fn pick<'a, T>(rng: &mut Rng, items: &'a [T]) -> &'a T {
    &items[below(rng, items.len())]
}

fn random_bytes(rng: &mut Rng, len: usize) -> Vec<u8> {
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// Bytes that matter to the text of an RTSP answer.
const TEXT_BYTES: &[u8] = b"\r\n\r\n\r\n:;,=  \t\"\\0123456789RTSP/1.0 OK CSeq Content-Length";

/// Mostly bytes that matter to a text protocol, now and then any byte.
fn text_like(rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let len = below(rng, max_len + 1);
    (0..len)
        .map(|_| {
            if one_in(rng, 5) {
                rng.next_u64() as u8
            } else {
                *pick(rng, TEXT_BYTES)
            }
        })
        .collect()
}

/// Numbers at and past every boundary a parser might trip over, and text
/// that only looks like a number.
const NUMBERS: &[&str] = &[
    "0",
    "-0",
    "-1",
    "+1",
    "00000000000000000000001",
    "127",
    "128",
    "255",
    "256",
    "32767",
    "65535",
    "65536",
    "1048575",
    "1048576",
    "1048577",
    "2147483647",
    "2147483648",
    "4294967295",
    "4294967296",
    "-2147483649",
    "9223372036854775807",
    "9223372036854775808",
    "18446744073709551615",
    "18446744073709551616",
    "340282366920938463463374607431768211456",
    "99999999999999999999999999999999999999999999999999",
    "0x",
    "0x10",
    "0XfF",
    "0xFFFFFFFF",
    "0x1FFFFFFFF",
    "0xFFFFFFFFFFFFFFFF",
    "0x10000000000000000",
    "1e400",
    "-1e400",
    "1e-400",
    "NaN",
    "nan",
    "inf",
    "-inf",
    "infinity",
    "1.5",
    ".",
    "-",
    "+",
    "",
    " ",
    " 7 ",
    "\u{0661}\u{0662}\u{0663}",
    "\u{FF11}\u{FF12}\u{FF13}",
    "1\u{0}",
    "\u{B2}",
];

/// Byte sequences that aren't UTF-8.
const NOT_UTF8: &[&[u8]] = &[
    b"\xFF",
    b"\x80",
    b"\xC3",
    b"\xC0\x80",
    b"\xE2\x82",
    b"\xED\xA0\x80",
    b"\xF0\x9F\x92",
    b"\xF8\x88\x80\x80\x80",
];

fn number(rng: &mut Rng) -> &'static str {
    pick(rng, NUMBERS)
}

fn replace_all(input: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut at = 0;
    while at < input.len() {
        if input[at..].starts_with(from) {
            out.extend_from_slice(to);
            at += from.len();
        } else {
            out.push(input[at]);
            at += 1;
        }
    }
    out
}

/// A run of ASCII digits, or a random spot when there is none, becomes a
/// boundary number.
fn splice_number(rng: &mut Rng, out: &mut Vec<u8>) {
    let digits: Vec<usize> = (0..out.len())
        .filter(|&i| out[i].is_ascii_digit())
        .collect();
    let (start, end) = if digits.is_empty() {
        let at = below(rng, out.len() + 1);
        (at, at)
    } else {
        let at = *pick(rng, &digits);
        let mut start = at;
        while start > 0 && out[start - 1].is_ascii_digit() {
            start -= 1;
        }
        let mut end = at;
        while end < out.len() && out[end].is_ascii_digit() {
            end += 1;
        }
        (start, end)
    };
    let replacement = number(rng).as_bytes().to_vec();
    out.splice(start..end, replacement);
}

/// A valid input changed in one to three ways.
fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..1 + below(rng, 3) {
        let at = below(rng, out.len() + 1);
        match below(rng, 10) {
            0 => out.truncate(at),
            1 => {
                let end = (at + 1 + below(rng, 16)).min(out.len());
                out.drain(at..end);
            }
            2 => {
                let len = below(rng, 9);
                let extra = random_bytes(rng, len);
                out.splice(at..at, extra);
            }
            3 => {
                let value = rng.next_u64() as u8;
                if let Some(byte) = out.get_mut(at) {
                    *byte = value;
                }
            }
            4 => {
                let end = (at + below(rng, 33)).min(out.len());
                let copy = out[at..end].to_vec();
                out.splice(at..at, copy);
            }
            5 => out = replace_all(&out, b"\r\n", b"\n"),
            6 => out = replace_all(&out, b"\n", b"\r\n"),
            7 => out = replace_all(&out, b"\r\n", b"\r"),
            8 => splice_number(rng, &mut out),
            _ => {
                let sequence = *pick(rng, NOT_UTF8);
                out.splice(at..at, sequence.iter().copied());
            }
        }
    }
    out
}

/// `input` cut into pieces of random length, as reads off a socket come.
fn random_chunks<'a>(rng: &mut Rng, input: &'a [u8]) -> Vec<&'a [u8]> {
    let mut chunks = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let len = 1 + below(rng, rest.len().min(64));
        let (chunk, tail) = rest.split_at(len);
        chunks.push(chunk);
        rest = tail;
    }
    chunks
}

// ---- RTSP -----------------------------------------------------------------

/// Answers as receivers write them, and a request, which the sender must
/// refuse without crashing.
const RTSP_CORPUS: &[&[u8]] = &[
    b"RTSP/1.0 200 OK\r\nCSeq: 0\r\nPublic: OPTIONS, ANNOUNCE, SETUP, RECORD, SET_PARAMETER, FLUSH, TEARDOWN\r\nServer: AirTunes/366.0\r\n\r\n",
    b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nTransport: RTP/AVP/UDP;unicast;mode=record;server_port=6000;control_port=6001;timing_port=6002\r\nSession: DEADBEEF;timeout=60\r\nAudio-Jack-Status: connected; type=analog\r\n\r\n",
    b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nAudio-Latency: 11025\r\nAudio-Jack-Status: disconnected\r\n\r\n",
    b"RTSP/1.0 401 Unauthorized\r\nCSeq: 5\r\nWWW-Authenticate: Digest realm=\"raop\", nonce=\"a1b\\\"2c3\", qop=\"auth,auth-int\", opaque=\"x y\", stale=TRUE\r\nWWW-Authenticate: Basic realm=\"raop\"\r\n\r\n",
    b"RTSP/1.0 200 OK\r\nCSeq: 6\r\nContent-Type: text/parameters\r\nContent-Length: 20\r\n\r\nvolume: -20.000000\r\n",
    b"RTSP/1.0 200 OK\nCSeq: 7\nX-Folded: first\n  second\n\tthird\n\n",
    b"RTSP/1.0 200 OK\r\nCSeq: 8\r\nContent-Length: 4\r\n\r\n\x00\xFF\x80\x7FRTSP/1.0 409 Conflict\r\nCSeq: 9\r\n\r\n",
    b"\r\n\r\n\nRTSP/1.0 453 Not Enough Bandwidth\r\ncseq:10\r\ncontent-length:0\r\n\r\n",
    b"RTSP/1.0 470 Connection Authorization Required\r\nCSeq: 11\r\nServer: AirTunes/\xC3\xA9 \xFF\r\n\r\n",
    b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\nhi",
];

/// What the session and the pairing read from an answer, applied to every
/// message the parser gives.
fn read_message(message: &Message) {
    match message {
        Message::Response(response) => {
            assert!((100..600).contains(&response.code), "{}", response.code);
            assert!(response.body.len() <= MAX_BODY_BYTES);
            let _ = response.cseq();
            let _ = response.is_success();
            read_headers(&response.headers);
            if let Some(db) = volume::parse_reported(&response.body) {
                assert!(db.is_finite());
            }
            let _ = response.encode();
        }
        Message::Request(request) => {
            assert!(!request.uri.is_empty());
            assert!(request.body.len() <= MAX_BODY_BYTES);
            let _ = request.encode();
        }
    }
}

fn read_headers(headers: &Headers) {
    for (_, value) in headers.iter() {
        read_header_value(value);
    }
    if let Some(challenge) = Challenge::from_headers(headers) {
        answer_challenge(&challenge);
    }
}

fn read_header_value(value: &str) {
    let _ = TransportPorts::parse(value);
    let _ = rtsp::public_methods(value);
    if let Some(id) = rtsp::session_id(value) {
        assert!(!id.is_empty());
    }
    let _ = rtsp::audio_latency(value);
    let _ = JackStatus::parse(value);
    check_challenge(value);
}

/// The `Authorization` the session would send for a challenge.
fn answer_challenge(challenge: &Challenge) {
    let _ = challenge.needs_unsupported_qop();
    let value = digest::authorization(challenge, "secret", Method::Options, "*", 1, "0a4f113b");
    assert!(value.starts_with("Digest username=\"iTunes\""), "{value}");
}

/// Reads one header value as a challenge; true when it is one.
fn check_challenge(text: &str) -> bool {
    match Challenge::parse(text) {
        Some(challenge) => {
            answer_challenge(&challenge);
            true
        }
        None => false,
    }
}

/// Every message in `chunks`, fed in order to one parser as reads off one
/// connection come, and the error that ended the stream, if any.
fn read_all(chunks: &[&[u8]]) -> (Vec<Message>, Option<ParseError>) {
    let mut parser = Parser::new();
    let mut messages = Vec::new();
    for chunk in chunks {
        parser.push(chunk);
        loop {
            match parser.next_message() {
                Ok(None) => break,
                Ok(Some(message)) => {
                    read_message(&message);
                    messages.push(message);
                }
                Err(error) => return (messages, Some(error)),
            }
        }
    }
    (messages, None)
}

// covers: P140, P146
#[test]
fn rtsp_answers_cut_split_or_fed_a_byte_at_a_time_read_the_same() {
    for &message in RTSP_CORPUS {
        let whole = read_all(&[message]);
        assert!(whole.1.is_none() && !whole.0.is_empty(), "{whole:?}");
        for cut in 0..=message.len() {
            let (head, tail) = message.split_at(cut);
            let (prefix, error) = read_all(&[head]);
            assert_eq!(error, None, "a cut valid answer is incomplete, not wrong");
            assert!(whole.0.starts_with(&prefix));
            assert_eq!(read_all(&[head, tail]), whole);
        }
        let bytes: Vec<&[u8]> = message.chunks(1).collect();
        assert_eq!(read_all(&bytes), whole);
    }
}

const STATUS_LINES: &[&str] = &[
    "RTSP/1.0 200 OK",
    "RTSP/1.0 200 OK",
    "RTSP/1.0 401 Unauthorized",
    "RTSP/1.0 470 Connection Authorization Required",
    "RTSP/1.0 099 Low",
    "RTSP/1.0 600 High",
    "RTSP/1.0 000",
    "RTSP/1.0 2000 OK",
    "RTSP/1.0 20 OK",
    "RTSP/1.0 -20 OK",
    "RTSP/1.0 +20 OK",
    "RTSP/1.0 \u{FF12}\u{FF10}\u{FF10} OK",
    "RTSP/1.0 200",
    "RTSP/1.0 ",
    "RTSP/1.0",
    "RTSP/1.1 200 OK",
    "rtsp/1.0 200 OK",
    "HTTP/1.1 200 OK",
    "OPTIONS * RTSP/1.0",
    "OPTIONS  RTSP/1.0",
    "FOO * RTSP/1.0",
    "POST /pair-verify RTSP/1.0 extra",
    "",
    " ",
    "\u{FFFD}",
];

const HEADER_NAMES: &[&str] = &[
    "CSeq",
    "Content-Length",
    "content-length",
    "Transport",
    "Session",
    "Public",
    "Audio-Latency",
    "Audio-Jack-Status",
    "WWW-Authenticate",
    "Server",
    "Content-Type",
    "",
    " ",
    "X-\u{C9}",
];

const JACK_VALUES: &[&str] = &[
    "connected",
    "disconnected; type=analog",
    "connected;type=digital",
    "connected; type",
    "connected; =",
    "connected; type=analog; type=other",
    ";",
    "",
    "CONNECTED; TYPE=DIGITAL",
];

/// A header value of the kinds the session reads, often slightly wrong.
fn header_value(rng: &mut Rng) -> Vec<u8> {
    match below(rng, 8) {
        0 => number(rng).as_bytes().to_vec(),
        1 => {
            let mut text = String::from("RTP/AVP/UDP;unicast;mode=record");
            for key in [
                "server_port",
                "control_port",
                "timing_port",
                "SERVER_PORT",
                "x",
            ] {
                if !one_in(rng, 3) {
                    text.push_str(pick(rng, &[";", "; ", ";;", ","]));
                    text.push_str(key);
                    text.push_str(pick(rng, &["=", " = ", "==", ""]));
                    text.push_str(number(rng));
                }
            }
            text.into_bytes()
        }
        2 => challenge_text(rng).into_bytes(),
        3 => pick(rng, JACK_VALUES).as_bytes().to_vec(),
        4 => {
            let id = *pick(rng, &["", "1", "DEADBEEF", " ", ";"]);
            let parameter = *pick(rng, &["", ";timeout=", ";"]);
            format!("{id}{parameter}{}", number(rng)).into_bytes()
        }
        5 => {
            let methods = [
                "OPTIONS",
                "ANNOUNCE",
                "SETUP",
                "RECORD",
                "",
                " ",
                ",",
                "SET_PARAMETER",
                "\u{E9}",
            ];
            let count = below(rng, 8);
            (0..count)
                .map(|_| *pick(rng, &methods))
                .collect::<Vec<_>>()
                .join(",")
                .into_bytes()
        }
        6 => text_like(rng, 60),
        _ => {
            let len = below(rng, 40);
            random_bytes(rng, len)
        }
    }
}

/// An answer put together from fuzzed parts: a start line, headers (some
/// folded), line ends of every kind, and a body whose Content-Length may
/// not match it.
fn synthetic_answer(rng: &mut Rng) -> Vec<u8> {
    let ends: [&[u8]; 6] = [b"\r\n", b"\r\n", b"\r\n", b"\n", b"\n", b"\r"];
    let mut out = if one_in(rng, 2) {
        b"RTSP/1.0 200 OK".to_vec()
    } else {
        pick(rng, STATUS_LINES).as_bytes().to_vec()
    };
    out.extend_from_slice(pick(rng, &ends));
    let body = match below(rng, 4) {
        0 => Vec::new(),
        1 => format!("volume: {}\r\n", number(rng)).into_bytes(),
        2 => {
            let len = below(rng, 100);
            random_bytes(rng, len)
        }
        _ => header_value(rng),
    };
    for _ in 0..below(rng, 8) {
        if one_in(rng, 8) {
            out.extend_from_slice(pick(rng, &[&b" folded"[..], b"\tfolded", b" "]));
        } else {
            out.extend_from_slice(pick(rng, HEADER_NAMES).as_bytes());
            out.extend_from_slice(pick(rng, &[&b": "[..], b": ", b": ", b":", b" : ", b""]));
            out.extend(header_value(rng));
        }
        out.extend_from_slice(pick(rng, &ends));
    }
    match below(rng, 4) {
        0 => {}
        1 => out.extend(format!("Content-Length: {}\r\n", body.len()).bytes()),
        2 => out.extend(format!("Content-Length: {}\r\n", number(rng)).bytes()),
        _ => {
            let length = (body.len() + below(rng, 5)).saturating_sub(2);
            out.extend(format!("Content-Length: {length}\r\n").bytes());
        }
    }
    out.extend_from_slice(pick(rng, &ends));
    out.extend(body);
    out
}

// covers: P140
#[test]
fn rtsp_parser_survives_mutated_synthetic_and_random_bytes() {
    let mut rng = Rng::new(0x5EED_0001);
    let mut messages = 0;
    for _ in 0..4000 {
        let base = *pick(&mut rng, RTSP_CORPUS);
        let input = mutate(&mut rng, base);
        let whole = read_all(&[&input]);
        let chunks = random_chunks(&mut rng, &input);
        assert_eq!(
            read_all(&chunks),
            whole,
            "{:?}",
            String::from_utf8_lossy(&input)
        );
    }
    for _ in 0..4000 {
        let mut input = synthetic_answer(&mut rng);
        if one_in(&mut rng, 3) {
            input.extend(synthetic_answer(&mut rng));
        }
        let whole = read_all(&[&input]);
        messages += whole.0.len();
        let chunks = random_chunks(&mut rng, &input);
        assert_eq!(
            read_all(&chunks),
            whole,
            "{:?}",
            String::from_utf8_lossy(&input)
        );
    }
    for _ in 0..3000 {
        let input = text_like(&mut rng, 300);
        let whole = read_all(&[&input]);
        let chunks = random_chunks(&mut rng, &input);
        assert_eq!(read_all(&chunks), whole);
    }
    for _ in 0..300 {
        let len = below(&mut rng, 2048);
        let input = random_bytes(&mut rng, len);
        let _ = read_all(&[&input]);
    }
    assert!(
        messages > 250,
        "the generator reaches whole answers: {messages}"
    );
}

// covers: P140, P146
#[test]
fn rtsp_parser_caps_heads_and_bodies() {
    // A head that never ends, and one with a header past the cap.
    let endless = vec![b'a'; MAX_HEAD_BYTES + 1];
    assert_eq!(read_all(&[&endless]).1, Some(ParseError::TooLarge));
    let mut long_header = b"RTSP/1.0 200 OK\r\nX-Long: ".to_vec();
    long_header.extend(std::iter::repeat_n(b'x', MAX_HEAD_BYTES));
    long_header.extend_from_slice(b"\r\n\r\n");
    assert_eq!(read_all(&[&long_header]).1, Some(ParseError::TooLarge));

    // Lengths past the cap or not a number end the stream at once, before
    // any body arrives.
    for length in [
        "1048577",
        "18446744073709551615",
        "18446744073709551616",
        "-1",
        "+-1",
        "1 2",
        "0x10",
        "",
        "\u{FF11}",
    ] {
        let input = format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {length}\r\n\r\n");
        let (messages, error) = read_all(&[input.as_bytes()]);
        assert!(messages.is_empty());
        assert!(
            matches!(
                error,
                Some(ParseError::TooLarge | ParseError::ContentLength)
            ),
            "{length}: {error:?}"
        );
    }

    // The largest body allowed arrives whole.
    let mut big = format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {MAX_BODY_BYTES}\r\n\r\n")
        .into_bytes();
    big.extend(std::iter::repeat_n(0xA5, MAX_BODY_BYTES));
    let chunks: Vec<&[u8]> = big.chunks(4096).collect();
    let (messages, error) = read_all(&chunks);
    assert_eq!(error, None);
    match messages.as_slice() {
        [Message::Response(response)] => assert_eq!(response.body.len(), MAX_BODY_BYTES),
        other => panic!("one answer, not {}", other.len()),
    }

    // Thousands of short headers under the cap, and thousands of stray
    // line ends before an answer.
    let mut many = b"RTSP/1.0 200 OK\r\n".to_vec();
    for index in 0..4000 {
        many.extend(format!("X-{index}: {index}\r\n").bytes());
    }
    many.extend_from_slice(b"\r\n");
    let (messages, error) = read_all(&[&many]);
    assert_eq!((messages.len(), error), (1, None));
    // The same head with a body that trickles in, then another answer: read
    // the same as all at once.
    many.truncate(many.len() - 2);
    many.extend(b"Content-Length: 262144\r\n\r\n");
    many.extend(std::iter::repeat_n(0x5A, 262_144));
    many.extend_from_slice(RTSP_CORPUS[1]);
    let whole = read_all(&[&many]);
    assert_eq!((whole.0.len(), whole.1), (2, None));
    let (head, body) = many.split_at(many.len() - 262_144 - RTSP_CORPUS[1].len());
    let mut chunks = vec![head];
    chunks.extend(body.chunks(97));
    assert_eq!(read_all(&chunks), whole);
    let mut stray = b"\r\n".repeat(50_000);
    stray.extend_from_slice(RTSP_CORPUS[0]);
    let (messages, error) = read_all(&random_chunks(&mut Rng::new(7), &stray));
    assert_eq!((messages.len(), error), (1, None));
}

// ---- Digest challenges ------------------------------------------------------

const CHALLENGES: &[&str] = &[
    "Digest realm=\"raop\", nonce=\"a1b2c3d4e5f60718293a4b5c6d7e8f90\"",
    "Digest realm=\"raop\", nonce=\"n\", qop=\"auth\", opaque=\"5ccc\", algorithm=MD5",
    "Digest realm=\"r\\\"q\", nonce=\"n\\\\\", stale=TRUE, qop=\"auth-int,auth\"",
    "digest   nonce=abc,realm=xyz,,  ,stale=false",
    "Digest realm=\"\u{E9}\u{4E2D}\u{1F3B5}\", nonce=\"\u{FFFD}\"",
];

const CHALLENGE_TOKENS: &[&str] = &[
    "Digest",
    "digest",
    "Basic",
    " ",
    "  ",
    "\t",
    ",",
    ", ",
    "=",
    "\"",
    "\\",
    "\\\"",
    "realm",
    "nonce",
    "opaque",
    "qop",
    "stale",
    "algorithm",
    "\"raop\"",
    "raop",
    "true",
    "MD5",
    "MD5-sess",
    "auth",
    "auth-int",
    "\u{E9}",
    "\u{4E2D}",
    "\u{1F3B5}",
    "\u{FFFD}",
    "\u{0}",
    "\r",
    "\u{85}",
    "\u{2028}",
    "\u{3000}",
];

const CHALLENGE_NAMES: &[&str] = &[
    "realm",
    "nonce",
    "opaque",
    "qop",
    "stale",
    "algorithm",
    "REALM",
    "x",
    "",
    "\u{E9}",
];

/// A challenge: either `name=value` parameters of fuzzed names, values and
/// separators, or a soup of the tokens a challenge is made of.
fn challenge_text(rng: &mut Rng) -> String {
    let mut text = String::new();
    if !one_in(rng, 4) {
        text.push_str(pick(
            rng,
            &["Digest ", "Digest", "digest\t", "Digest\u{3000}"],
        ));
    }
    if one_in(rng, 2) {
        let mut names: Vec<&str> = Vec::new();
        for needed in ["realm", "nonce"] {
            if !one_in(rng, 6) {
                names.insert(below(rng, names.len() + 1), needed);
            }
        }
        for _ in 0..below(rng, 5) {
            let name = *pick(rng, CHALLENGE_NAMES);
            names.insert(below(rng, names.len() + 1), name);
        }
        for name in names {
            text.push_str(name);
            if one_in(rng, 8) {
                text.push_str(pick(rng, &[" = ", "", "=="]));
            } else {
                text.push('=');
            }
            match below(rng, 6) {
                0 => {
                    text.push('"');
                    for _ in 0..below(rng, 4) {
                        text.push_str(pick(rng, CHALLENGE_TOKENS));
                    }
                    if !one_in(rng, 5) {
                        text.push('"');
                    }
                }
                1 => text.push_str(number(rng)),
                2 => text.push_str(pick(rng, &["auth", "\"auth,auth-int\"", "MD5", "TRUE"])),
                3 => {}
                4 => text.push_str("\"x\\\"y\\\\\""),
                _ => text.push_str("\"raop\""),
            }
            if one_in(rng, 6) {
                text.push_str(pick(rng, &[" ", ",,", " , ", ""]));
            } else {
                text.push_str(", ");
            }
        }
    } else {
        for _ in 0..below(rng, 24) {
            text.push_str(pick(rng, CHALLENGE_TOKENS));
        }
    }
    text
}

// covers: P140
#[test]
fn digest_challenges_survive_any_text() {
    let mut rng = Rng::new(0x5EED_0002);
    for challenge in CHALLENGES {
        assert!(check_challenge(challenge), "{challenge}");
        let bytes = challenge.as_bytes();
        for cut in 0..=bytes.len() {
            check_challenge(&String::from_utf8_lossy(&bytes[..cut]));
            check_challenge(&String::from_utf8_lossy(&bytes[cut..]));
        }
    }
    for _ in 0..3000 {
        let base = *pick(&mut rng, CHALLENGES);
        let text = mutate(&mut rng, base.as_bytes());
        check_challenge(&String::from_utf8_lossy(&text));
    }
    let mut parsed = 0;
    for _ in 0..6000 {
        let text = challenge_text(&mut rng);
        parsed += usize::from(check_challenge(&text));
    }
    // Several challenges in one answer.
    for _ in 0..500 {
        let mut headers = Headers::new();
        for _ in 0..below(&mut rng, 5) {
            let text = challenge_text(&mut rng);
            headers.push("WWW-Authenticate", text);
        }
        read_headers(&headers);
    }
    assert!(
        parsed > 300,
        "the generator reaches valid challenges: {parsed}"
    );
}

// ---- RTP, control and timing datagrams ----------------------------------------

/// What the sender reads from a datagram on its control or timing socket.
fn read_datagram(datagram: &[u8]) {
    if RtpHeader::parse(datagram).is_some() {
        assert!(datagram.len() >= rtp::HEADER_BYTES);
    }
    if SyncPacket::parse(datagram).is_some() {
        assert_eq!(datagram.len(), rtp::SYNC_BYTES);
    }
    if ResendRequest::parse(datagram).is_some() {
        assert!(datagram.len() >= rtp::RESEND_REQUEST_BYTES);
    }
    if let Some(timing) = TimingPacket::parse(datagram) {
        assert_eq!(datagram.len(), rtp::TIMING_BYTES);
        let _ = timing.encode();
    }
    if let Some(reply) = rtp::timing_reply(datagram, NtpTime(1), NtpTime(u64::MAX)) {
        assert!(TimingPacket::parse(&reply).is_some_and(|packet| packet.reply));
    }
}

/// First bytes of the datagrams the sender knows, and some it doesn't.
const DATAGRAM_HEADS: [[u8; 2]; 10] = [
    [0x80, 0x60],
    [0x80, 0xE0],
    [0x80, 0xD5],
    [0x80, 0xD4],
    [0x90, 0xD4],
    [0x80, 0xD2],
    [0x80, 0xD3],
    [0x40, 0xD2],
    [0xFF, 0xFF],
    [0x00, 0x00],
];

// covers: P92
#[test]
fn rtp_control_and_timing_datagrams_survive_any_bytes() {
    let mut rng = Rng::new(0x5EED_0003);
    for len in 0..=64 {
        for head in DATAGRAM_HEADS {
            for _ in 0..4 {
                let mut datagram = random_bytes(&mut rng, len);
                for (byte, value) in datagram.iter_mut().zip(head) {
                    *byte = value;
                }
                read_datagram(&datagram);
            }
        }
    }
    for _ in 0..3000 {
        let len = below(&mut rng, 1500);
        let datagram = random_bytes(&mut rng, len);
        read_datagram(&datagram);
    }
}

// covers: P92
#[test]
fn a_speakers_control_and_timing_sockets_survive_any_datagram() {
    let mut rng = Rng::new(0x5EED_0004);
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.run_for(Duration::from_millis(500));
    let last = rig.audio(id).last().expect("audio went out").header.seq;
    let source = SocketAddr::new(RECEIVER, 50_002);
    let history = HISTORY_PACKETS as u16;
    let firsts = [
        0,
        1,
        last.wrapping_sub(history),
        last.wrapping_sub(1),
        last,
        last.wrapping_add(1),
        u16::MAX,
    ];
    let counts = [
        0,
        1,
        2,
        MAX_RESEND_REQUEST - 1,
        MAX_RESEND_REQUEST,
        MAX_RESEND_REQUEST + 1,
        u16::MAX,
    ];
    // Asked again and again: each packet is resent a few times at most.
    for _ in 0..5 {
        for first in firsts {
            for count in counts {
                let request = ResendRequest {
                    seq: rng.next_u64() as u16,
                    first,
                    count,
                }
                .encode();
                let before = rig.sent.len();
                rig.stream.handle_control_packet(id, &request, rig.now);
                rig.drain();
                assert!(rig.sent.len() - before <= usize::from(count.min(MAX_RESEND_REQUEST)));
            }
        }
    }
    for _ in 0..3000 {
        let len = below(&mut rng, 64);
        let mut datagram = random_bytes(&mut rng, len);
        if len >= 2 && one_in(&mut rng, 2) {
            datagram[0] = 0x80;
            datagram[1] = *pick(&mut rng, &[0xD5u8, 0xD2, 0xD3]);
        }
        rig.stream.handle_control_packet(id, &datagram, rig.now);
        if let Some(reply) = rig
            .stream
            .answer_timing(id, &datagram, source, rig.now, rig.now)
        {
            assert_eq!(reply.bytes.len(), rtp::TIMING_BYTES);
            assert_eq!(reply.to, source);
        }
        rig.drain();
    }
    // The stream goes on, and a removed speaker's datagrams are ignored.
    rig.run_for(Duration::from_millis(100));
    assert!(rig.stream.remove_speaker(id));
    let request = ResendRequest {
        seq: 0,
        first: last,
        count: u16::MAX,
    }
    .encode();
    rig.stream.handle_control_packet(id, &request, rig.now);
    let timing = TimingPacket {
        reply: false,
        seq: 7,
        origin: NtpTime(0),
        receive: NtpTime(0),
        transmit: NtpTime(u64::MAX),
    }
    .encode();
    assert!(
        rig.stream
            .answer_timing(id, &timing, source, rig.now, rig.now)
            .is_none()
    );
}

// ---- TXT records, devices and mDNS names -----------------------------------------

const TXT_KEYS: &[&str] = &[
    "txtvers",
    "ch",
    "cn",
    "et",
    "md",
    "sr",
    "ss",
    "tp",
    "pw",
    "sv",
    "sm",
    "da",
    "am",
    "vn",
    "vs",
    "fv",
    "ov",
    "ft",
    "sf",
    "pk",
    "deviceid",
    "features",
    "flags",
    "model",
    "manufacturer",
    "srcvers",
    "protovers",
    "pi",
    "psi",
    "gid",
    "igl",
    "gcgl",
    "acl",
    "rsf",
    "osvers",
    "FT",
    "Features",
    "PW",
    "",
    "=",
    "\u{E9}",
    "x",
];

const TXT_VALUES: &[&str] = &[
    "0x5A7FFFF7,0x1E",
    "0x4A7FDFD5,0xBC155FDE",
    "0x445F8A00,0x1C340",
    ",",
    ",,",
    "0,1,2,3",
    "0,3,5",
    "0,1,2",
    "UDP",
    "TCP,UDP",
    "0x5A7FFFF7,0x1E,0x1",
    "0xFFFFFFFF,0xFFFFFFFF",
    "0x100000000",
    "true",
    "TRUE",
    "false",
    "1",
    "0",
    "yes",
    "AA:BB:CC:DD:EE:FF",
    "aa:bb:cc:dd:ee:ff",
    "AA:BB:CC:DD:EE",
    "AA:BB:CC:DD:EE:FG",
    "AAA:B:CC:DD:EE:FF",
    "::::::",
    "AA:BB:CC:DD:EE:FF:00",
    "\u{E9}:BB:CC:DD:EE:FF",
    "AppleTV3,2",
    "AudioAccessory5,1",
    "366.0",
    "0x4",
    "0x204",
    "0x88",
    "0x200",
];

/// A TXT value of the kinds the sender reads, often slightly wrong.
fn txt_value(rng: &mut Rng) -> Vec<u8> {
    match below(rng, 9) {
        0 => number(rng).as_bytes().to_vec(),
        1 => format!("{},{}", number(rng), number(rng)).into_bytes(),
        2 => pick(rng, TXT_VALUES).as_bytes().to_vec(),
        3 => {
            // `pk`: hex of every length around 32 bytes, now and then not hex.
            let len = *pick(rng, &[0usize, 1, 31, 32, 33, 63, 64, 65, 66, 128]);
            (0..len)
                .map(|_| *pick(rng, b"0123456789abcdefABCDEFgx"))
                .collect()
        }
        4 => text_like(rng, 40),
        5 => {
            let len = below(rng, 40);
            random_bytes(rng, len)
        }
        6 => {
            let mut value = b"0x".to_vec();
            for _ in 0..below(rng, 24) {
                value.push(*pick(rng, b"0123456789abcdefABCDEF"));
            }
            value
        }
        7 => vec![b'a'; below(rng, 600)],
        _ => Vec::new(),
    }
}

/// The strings of one TXT record: known and unknown keys, flags without
/// `=`, empty values, fuzzed values and repeated keys.
fn txt_record(rng: &mut Rng) -> Vec<Vec<u8>> {
    (0..below(rng, 14))
        .map(|_| {
            let mut string = pick(rng, TXT_KEYS).as_bytes().to_vec();
            match below(rng, 6) {
                0 => {}
                1 => string.push(b'='),
                _ => {
                    string.push(b'=');
                    string.extend(txt_value(rng));
                }
            }
            if one_in(rng, 20) {
                string = mutate(rng, &string);
            }
            string
        })
        .collect()
}

fn read_txt(txt: &Txt) {
    for key in TXT_KEYS {
        let _ = txt.raw(key);
        let _ = txt.string(key);
        let _ = txt.flag(key);
        let _ = txt.int(key);
        let _ = txt.int_list(key);
        let _ = txt.list(key);
        if let Some(bytes) = txt.hex_bytes(key) {
            assert!(!bytes.is_empty());
        }
        if let Some(features) = txt.features(key) {
            for bit in [0, 31, 32, 63, 64, 200, u32::MAX] {
                let _ = features.has(bit);
            }
        }
        if let Some(flags) = txt.status_flags(key) {
            for bit in [0, 31, 32, u32::MAX] {
                let _ = flags.has(bit);
            }
        }
    }
    let _ = RaopTxt::read(txt);
    let _ = AirPlayTxt::read(txt);
}

/// Everything the sender asks of a device when it lists or plays to it.
fn read_device(device: &Device) {
    let _ = device.status_flags();
    let _ = device.features();
    let _ = device.is_apple_tv();
    let _ = device.security_modes();
    let _ = device.needs_auth_setup();
    let _ = device.metadata_support();
    for pcm in [false, true] {
        let _ = device.choose_codec(pcm);
    }
    let _ = device.alac_fallback();
    for airplay2 in [false, true] {
        for outcome in [
            None,
            Some(ErrorKind::ProtocolError),
            Some(ErrorKind::PairingRequired),
            Some(ErrorKind::Refused),
        ] {
            let _ = device.choose_path(airplay2, outcome);
        }
    }
    for credentials in [false, true] {
        let _ = device.airplay1_plan(credentials);
    }
    let _ = device.connect_socket_addrs();
}

const INSTANCES: &[&str] = &[
    "0200005E0053@Kitchen",
    "0200005E0053@",
    "@Kitchen",
    "0200005E005@Kitchen",
    "0200005E00531@Kitchen",
    "GG00005E0053@Kitchen",
    "0200005E0053",
    "",
    "Kitchen",
    "\u{E9}00005E0053@x",
    "0200005E0053@\u{E9}@\u{4E2D}",
    "\u{FFFD}",
];

fn address(rng: &mut Rng) -> ScopedAddr {
    match below(rng, 5) {
        0 => ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, rng.next_u64() as u8))),
        1 => ScopedAddr::with_scope(
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            rng.next_u64() as u32,
        ),
        2 => ScopedAddr::new(IpAddr::V6(Ipv6Addr::new(
            0x2001,
            0xdb8,
            0,
            0,
            0,
            0,
            0,
            rng.next_u64() as u16,
        ))),
        3 => ScopedAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        _ => ScopedAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
    }
}

fn service_record(rng: &mut Rng, kind: ServiceKind) -> ServiceRecord {
    let instance = if one_in(rng, 3) {
        String::from_utf8_lossy(&text_like(rng, 30)).into_owned()
    } else {
        pick(rng, INSTANCES).to_string()
    };
    let count = below(rng, 4);
    ServiceRecord {
        kind,
        instance,
        host: pick(rng, &["kitchen.local.", "", "."]).to_string(),
        port: *pick(rng, &[0u16, 7000, u16::MAX]),
        txt: txt_record(rng),
        addresses: (0..count).map(|_| address(rng)).collect(),
    }
}

// covers: P13
#[test]
fn txt_records_and_devices_survive_any_strings() {
    let mut rng = Rng::new(0x5EED_0005);
    let mut directory = Directory::new();
    for _ in 0..3000 {
        let strings = txt_record(&mut rng);
        read_txt(&Txt::parse(&strings));
        let raop = (!one_in(&mut rng, 4)).then(|| service_record(&mut rng, ServiceKind::Raop));
        let airplay =
            (!one_in(&mut rng, 3)).then(|| service_record(&mut rng, ServiceKind::AirPlay));
        let key = raop
            .as_ref()
            .or(airplay.as_ref())
            .map(ServiceRecord::device_key)
            .unwrap_or(DeviceKey::Instance(ServiceKind::Raop, String::new()));
        if let Some(device) = Device::from_records(key, raop.as_ref(), airplay.as_ref()) {
            read_device(&device);
        }
        for record in [raop, airplay].into_iter().flatten() {
            let key = record.device_key();
            for event in directory.apply(BrowseEvent::Resolved(record)) {
                if let DirectoryEvent::Updated(device) = event {
                    read_device(&device);
                }
            }
            let _ = directory.codec_for(&key, true);
        }
        if one_in(&mut rng, 4) {
            let kind = *pick(&mut rng, &[ServiceKind::Raop, ServiceKind::AirPlay]);
            let instance = pick(&mut rng, INSTANCES).to_string();
            let _ = directory.apply(BrowseEvent::Removed { kind, instance });
        }
    }
    for _ in 0..6000 {
        let text = String::from_utf8_lossy(&txt_value(&mut rng)).into_owned();
        let _ = parse_int(&text);
        let _ = parse_bool(&text);
        let _ = Features::parse_txt(&text);
        let _ = StatusFlags::parse_txt(&text);
        if let Some(id) = DeviceId::from_hex12(&text) {
            assert_eq!(id.to_string(), text.to_ascii_uppercase());
        }
        let _ = DeviceId::from_colon_form(&text);
        let _ = parse_raop_instance(&text);
    }
}

const NAME_TOKENS: &[&str] = &[
    "\\",
    "\\.",
    "\\\\",
    "\\0",
    "\\00",
    "\\000",
    "\\032",
    "\\255",
    "\\256",
    "\\999",
    "\\25",
    "\\2a",
    ".",
    "..",
    "_raop._tcp.local",
    "_airplay._tcp.local",
    "._raop._tcp.local.",
    "._airplay._tcp.local.",
    ".local.",
    "_raop",
    "\u{E9}",
    "\u{4E2D}",
    "\u{1F3B5}",
    "a",
    "0200005E0053@",
    "Kitchen",
    " ",
    "\u{FFFD}",
    "\u{0}",
];

const RAOP_TYPE: &str = "_raop._tcp.local.";
const AIRPLAY_TYPE: &str = "_airplay._tcp.local.";

fn service_name(rng: &mut Rng) -> String {
    let mut name = String::new();
    for _ in 0..below(rng, 10) {
        name.push_str(pick(rng, NAME_TOKENS));
    }
    name
}

// covers: P7
#[test]
fn mdns_names_and_resolved_services_survive_any_text() {
    let mut rng = Rng::new(0x5EED_0006);
    for _ in 0..6000 {
        let name = service_name(&mut rng);
        for kind in [ServiceKind::Raop, ServiceKind::AirPlay] {
            if let Some(instance) = instance_name(&name, kind) {
                assert!(!instance.is_empty());
            }
        }
        let _ = ServiceKind::from_service_type(&name);
        let service_type = *pick(&mut rng, &[RAOP_TYPE, AIRPLAY_TYPE, "_x._tcp.local.", ""]);
        let _ = browse_event(ServiceEvent::ServiceRemoved(service_type.to_owned(), name));
    }
    let mut resolved = 0;
    for _ in 0..400 {
        let service_type = *pick(&mut rng, &[RAOP_TYPE, AIRPLAY_TYPE]);
        let instance = if one_in(&mut rng, 2) {
            pick(&mut rng, INSTANCES).to_string()
        } else {
            service_name(&mut rng)
        };
        let owned: Vec<(String, String)> = (0..below(&mut rng, 10))
            .map(|_| {
                let key = pick(&mut rng, &TXT_KEYS[..TXT_KEYS.len() - 4]).to_string();
                let value = String::from_utf8_lossy(&txt_value(&mut rng)).into_owned();
                (key, value)
            })
            .collect();
        let pairs: Vec<(&str, &str)> = owned
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let properties: Vec<TxtProperty> = pairs.iter().map(TxtProperty::from).collect();
        let addresses: Vec<IpAddr> = (0..below(&mut rng, 3))
            .map(|_| address(&mut rng).ip)
            .collect();
        let port = *pick(&mut rng, &[0u16, 7000, u16::MAX]);
        let Ok(info) = ServiceInfo::new(
            service_type,
            &instance,
            "kitchen.local.",
            addresses.as_slice(),
            port,
            properties,
        ) else {
            continue;
        };
        let event = ServiceEvent::ServiceResolved(Box::new(info.as_resolved_service()));
        if let Some(BrowseEvent::Resolved(record)) = browse_event(event) {
            resolved += 1;
            read_txt(&Txt::parse(&record.txt));
            let key = record.device_key();
            let (raop, airplay) = match record.kind {
                ServiceKind::Raop => (Some(&record), None),
                ServiceKind::AirPlay => (None, Some(&record)),
            };
            if let Some(device) = Device::from_records(key, raop, airplay) {
                read_device(&device);
            }
        }
    }
    assert!(
        resolved > 80,
        "the generator reaches resolved services: {resolved}"
    );
}

// covers: P18
#[test]
fn the_resolver_survives_any_records_and_ttls() {
    let mut rng = Rng::new(0x5EED_0007);
    let mut resolver = Resolver::new();
    let mut now = Instant::now();
    let ttls = [0, 1, 120, 4500, u32::MAX / 2, u32::MAX];
    let hosts = [
        "kitchen.local.",
        "KITCHEN.local",
        "kitchen.local",
        "",
        ".",
        "\u{E9}.local.",
    ];
    let instances = ["0200005E0053@Kitchen", "Kitchen", "", "\u{E9}"];
    for _ in 0..4000 {
        let kind = *pick(&mut rng, &[ServiceKind::Raop, ServiceKind::AirPlay]);
        let instance = pick(&mut rng, &instances).to_string();
        let ttl = if one_in(&mut rng, 3) {
            rng.next_u64() as u32
        } else {
            *pick(&mut rng, &ttls)
        };
        let record = match below(&mut rng, 4) {
            0 => DnsRecord::Ptr {
                kind,
                instance,
                ttl,
            },
            1 => DnsRecord::Srv {
                kind,
                instance,
                target: pick(&mut rng, &hosts).to_string(),
                port: rng.next_u64() as u16,
                ttl,
            },
            2 => DnsRecord::Txt {
                kind,
                instance,
                strings: txt_record(&mut rng),
                ttl,
            },
            _ => DnsRecord::Address {
                host: pick(&mut rng, &hosts).to_string(),
                address: address(&mut rng),
                ttl,
            },
        };
        for event in resolver.record(record, now) {
            if let BrowseEvent::Resolved(record) = event {
                let _ = record.device_key();
            }
        }
        now += Duration::from_secs(rng.next_u64() % 600);
        if one_in(&mut rng, 10) {
            let _ = resolver.expire(now);
        }
        let _ = resolver.next_expiry();
    }
    let _ = resolver.expire(now + Duration::from_secs(2 * u64::from(u32::MAX)));
    assert_eq!(resolver.next_expiry(), None);
}

// ---- Tampered answers ---------------------------------------------------------------

const STATUS_CODES: &[u16] = &[
    0, 99, 100, 199, 200, 201, 204, 299, 300, 400, 401, 403, 404, 409, 453, 470, 500, 501, 503,
    599, 600, 999, 65_535,
];

const ANSWER_HEADERS: &[&str] = &[
    "CSeq",
    "Transport",
    "Session",
    "Public",
    "Audio-Latency",
    "Audio-Jack-Status",
    "WWW-Authenticate",
    "Server",
    "Content-Type",
    "Content-Length",
];

/// `response` with its `name` header replaced by `value`.
fn with_header(response: &mut Response, name: &str, value: String) {
    let mut headers = Headers::new();
    for (known, existing) in response.headers.iter() {
        if !known.eq_ignore_ascii_case(name) {
            headers.push(known, existing);
        }
    }
    headers.push(name, value);
    response.headers = headers;
}

/// One way a receiver gets an answer wrong: bytes mutated or cut, another
/// status, a fuzzed header or challenge, a body from `body`, the answer
/// twice, a request of its own, or trailing bytes.
fn tamper(rng: &mut Rng, reply: Vec<u8>, body: &mut dyn FnMut(&mut Rng) -> Vec<u8>) -> Vec<u8> {
    let parsed = match read_all(&[&reply]).0.pop() {
        Some(Message::Response(response)) => Some(response),
        _ => None,
    };
    let Some(mut response) = parsed else {
        return mutate(rng, &reply);
    };
    match below(rng, 10) {
        0 => return mutate(rng, &reply),
        1 => {
            let cut = below(rng, reply.len());
            return reply[..cut].to_vec();
        }
        2 => response.code = *pick(rng, STATUS_CODES),
        3 => {
            let name = *pick(rng, ANSWER_HEADERS);
            let value = String::from_utf8_lossy(&header_value(rng)).into_owned();
            with_header(&mut response, name, value);
        }
        4 => {
            response.code = 401;
            let value = challenge_text(rng);
            with_header(&mut response, "WWW-Authenticate", value);
        }
        5 | 6 => response.body = body(rng),
        7 => {
            let mut twice = reply.clone();
            twice.extend_from_slice(&reply);
            return twice;
        }
        8 => {
            let mut out = pick(
                rng,
                &[
                    &b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n"[..],
                    b"FOO * RTSP/1.0\r\n\r\n",
                    b"\r\n\r\n",
                ],
            )
            .to_vec();
            out.extend_from_slice(&reply);
            return out;
        }
        _ => {
            let mut out = reply;
            out.extend(text_like(rng, 40));
            return out;
        }
    }
    response.encode()
}

// ---- The pairing ----------------------------------------------------------------------

fn plist_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    value.to_writer_binary(&mut out).expect("a binary plist");
    out
}

/// Data of `len` bytes: zeros, 0xFF, the 2048-bit prime N (with leading
/// zeros when longer), or random.
fn data_of(rng: &mut Rng, len: usize) -> Vec<u8> {
    let prime = SrpGroup::RFC5054_2048.prime();
    match below(rng, 4) {
        0 => vec![0; len],
        1 => vec![0xFF; len],
        2 if len >= prime.len() => {
            let mut data = vec![0; len - prime.len()];
            data.extend_from_slice(prime);
            data
        }
        _ => random_bytes(rng, len),
    }
}

/// A value of any type and size for a key the pairing reads.
fn fuzz_value(rng: &mut Rng) -> Value {
    match below(rng, 9) {
        0..=3 => {
            let len = *pick(
                rng,
                &[
                    0usize, 1, 15, 16, 17, 20, 31, 32, 33, 64, 255, 256, 257, 384, 1024,
                ],
            );
            Value::Data(data_of(rng, len))
        }
        4 => Value::String(String::from_utf8_lossy(&text_like(rng, 20)).into_owned()),
        5 => Value::Integer((rng.next_u64() as i64).into()),
        6 => Value::Boolean(one_in(rng, 2)),
        7 => Value::Real(*pick(rng, &[-0.0, 0.5, 1e308])),
        _ => Value::Array(vec![
            Value::Data(vec![1, 2, 3]),
            Value::Dictionary(Dictionary::new()),
        ]),
    }
}

/// A dictionary with some of the keys the pairing reads, each of a fuzzed
/// type and size.
fn fuzz_dictionary(rng: &mut Rng) -> Value {
    let mut dictionary = Dictionary::new();
    for key in ["pk", "salt", "proof", "epk", "authTag", "x"] {
        if !one_in(rng, 4) {
            dictionary.insert(key.to_owned(), fuzz_value(rng));
        }
    }
    Value::Dictionary(dictionary)
}

/// A binary plist of one object, written by hand, so its header and counts
/// can say anything.
fn bplist_of(object: &[u8], objects: u64) -> Vec<u8> {
    let mut body = b"bplist00".to_vec();
    body.extend_from_slice(object);
    let table = body.len() as u64;
    body.push(8);
    body.extend_from_slice(&[0; 6]);
    body.push(1);
    body.push(1);
    body.extend_from_slice(&objects.to_be_bytes());
    body.extend_from_slice(&0u64.to_be_bytes());
    body.extend_from_slice(&table.to_be_bytes());
    body
}

/// Binary plists whose objects claim impossible sizes, refer to themselves
/// or to objects that aren't there.
fn hostile_bplists() -> Vec<Vec<u8>> {
    let huge = [0x13, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let mut bodies = Vec::new();
    for marker in [0x4F, 0x5F, 0x6F, 0x8F, 0xAF, 0xCF, 0xDF] {
        let mut object = vec![marker];
        object.extend_from_slice(&huge);
        bodies.push(bplist_of(&object, 1));
        bodies.push(bplist_of(&[marker, 0x10, 0xFF], 1));
    }
    bodies.push(bplist_of(&[0xA1, 0x00], 1));
    bodies.push(bplist_of(&[0xD1, 0x00, 0x00], 1));
    bodies.push(bplist_of(&[0xA2, 0x00, 0x05], 1));
    bodies.push(bplist_of(&[0xD1, 0x05, 0x06], 1));
    bodies.push(bplist_of(&[0x14, 0, 0, 0, 0, 0, 0, 0, 1], 1));
    bodies.push(bplist_of(&[0x15, 1, 2, 3], 1));
    bodies.push(bplist_of(&[0x21, 1, 2], 1));
    bodies.push(bplist_of(&[0x33, 1, 2], 1));
    bodies.push(bplist_of(&[0x80, 1], 1));
    bodies.push(bplist_of(&[0xD0], u64::MAX));
    bodies.push(bplist_of(&[0xD0], 0));
    bodies.push(bplist_of(&[0xD0], 1 << 40));
    bodies.push(bplist_of(&[], 1));
    bodies
}

/// A valid binary plist whose trailer (offset sizes, object count, top
/// object, offset table) says something impossible.
fn broken_trailer(rng: &mut Rng) -> Vec<u8> {
    let mut body = plist_bytes(&fuzz_dictionary(rng));
    let len = body.len();
    let trailer = len - 32;
    match below(rng, 5) {
        0 => body[trailer + 6] = *pick(rng, &[0u8, 3, 5, 9, 0xFF]),
        1 => body[trailer + 7] = *pick(rng, &[0u8, 3, 5, 9, 0xFF]),
        _ => {
            let field = trailer + 8 + 8 * below(rng, 3);
            let value = *pick(
                rng,
                &[
                    0u64,
                    1,
                    0x7F,
                    u64::from(u32::MAX),
                    i64::MAX as u64,
                    u64::MAX,
                    len as u64 - 1,
                    len as u64,
                ],
            );
            body[field..field + 8].copy_from_slice(&value.to_be_bytes());
        }
    }
    body
}

/// A body for a pairing answer: random bytes, broken or valid property
/// lists of fuzzed contents, binary or XML.
fn pairing_body(rng: &mut Rng) -> Vec<u8> {
    match below(rng, 9) {
        0 => {
            let len = below(rng, 200);
            random_bytes(rng, len)
        }
        1 => {
            let mut body = b"bplist00".to_vec();
            let len = below(rng, 120);
            body.extend(random_bytes(rng, len));
            body
        }
        2 => {
            let valid = plist_bytes(&fuzz_dictionary(rng));
            mutate(rng, &valid)
        }
        3 => {
            let valid = plist_bytes(&fuzz_dictionary(rng));
            let cut = below(rng, valid.len() + 1);
            valid[..cut].to_vec()
        }
        4 => broken_trailer(rng),
        5 => {
            let mut xml = Vec::new();
            fuzz_dictionary(rng)
                .to_writer_xml(&mut xml)
                .expect("an XML plist");
            if one_in(rng, 2) {
                mutate(rng, &xml)
            } else {
                xml
            }
        }
        6 => plist_bytes(&fuzz_value(rng)),
        _ => plist_bytes(&fuzz_dictionary(rng)),
    }
}

/// Who answers the pairing's requests: a script that brings it to the PIN
/// with a B the sender accepts and answers the proof with a wrong M2, or a
/// fake receiver with a fake Apple TV that pairs for real.
enum Answerer {
    Script { parser: Parser, setup_pins: usize },
    Receiver(Box<FakeReceiver>),
}

impl Answerer {
    fn script() -> Answerer {
        Answerer::Script {
            parser: Parser::new(),
            setup_pins: 0,
        }
    }

    fn answer(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Answerer::Receiver(receiver) => receiver.feed(bytes),
            Answerer::Script { parser, setup_pins } => {
                parser.push(bytes);
                let mut out = Vec::new();
                while let Some(message) = parser.next_message().expect("valid RTSP") {
                    let Message::Request(request) = message else {
                        panic!("the sender sent a response");
                    };
                    out.extend(scripted_answer(&request, setup_pins).encode());
                }
                out
            }
        }
    }
}

fn scripted_answer(request: &Request, setup_pins: &mut usize) -> Response {
    let mut response = Response::new(200, "OK");
    response
        .headers
        .push("CSeq", request.headers.get("CSeq").unwrap_or("0"));
    match (request.method, request.uri.as_str()) {
        (Method::Options, _) => response.headers.push("Public", "ANNOUNCE, SETUP, RECORD"),
        (Method::Post, "/pair-setup-pin") => {
            *setup_pins += 1;
            let mut body = Dictionary::new();
            if *setup_pins == 1 {
                body.insert("pk".to_owned(), Value::Data(vec![2; 256]));
                body.insert("salt".to_owned(), Value::Data(vec![7; 16]));
            } else {
                body.insert("proof".to_owned(), Value::Data(vec![9; 20]));
            }
            response.body = plist_bytes(&Value::Dictionary(body));
            response
                .headers
                .push("Content-Type", "application/x-apple-binary-plist");
        }
        _ => {}
    }
    response
}

/// A pairing driven to the end, the `target`-th answer (0-based) tampered
/// with.
struct PairRun {
    pairing: Pairing,
    answerer: Answerer,
    now: Instant,
    answers: usize,
    events: Vec<PairingEvent>,
}

impl PairRun {
    fn new(answerer: Answerer) -> PairRun {
        let now = Instant::now();
        PairRun {
            pairing: Pairing::new(
                vec![SocketAddr::new(RECEIVER, RAOP_PORT)],
                ids(),
                CLIENT,
                now,
            ),
            answerer,
            now,
            answers: 0,
            events: Vec::new(),
        }
    }

    /// The same run with the speaker's password, so a challenge is answered
    /// with `Authorization` (P46).
    fn with_password(mut self) -> PairRun {
        self.pairing = self.pairing.with_password(Some("secret".to_owned()));
        self
    }

    fn pump(&mut self, target: usize, tamper: &mut dyn FnMut(Vec<u8>) -> Vec<u8>) {
        let mut rounds = 0;
        loop {
            let mut connect = false;
            while let Some(output) = self.pairing.poll_output() {
                rounds += 1;
                assert!(rounds < 10_000, "the pairing settles");
                match output {
                    pairing::Output::Send(bytes) => {
                        let mut reply = self.answerer.answer(&bytes);
                        if reply.is_empty() {
                            continue;
                        }
                        if self.answers == target {
                            reply = tamper(reply);
                        }
                        self.answers += 1;
                        self.pairing.handle_input(&reply, self.now);
                    }
                    pairing::Output::Connect(_) => connect = true,
                    pairing::Output::Event(event) => self.events.push(event),
                    pairing::Output::Close => {}
                }
            }
            if !connect {
                break;
            }
            self.pairing.handle_connected(self.now);
        }
    }

    /// Runs the pairing, types the PIN when `pin` and it is asked for, and
    /// lets its timers run out.
    fn run(
        mut self,
        target: usize,
        tamper: &mut dyn FnMut(Vec<u8>) -> Vec<u8>,
        pin: bool,
    ) -> PairRun {
        self.pump(target, tamper);
        if pin && self.pairing.state() == PairingState::WaitingForPin {
            let _ = self
                .pairing
                .submit_pin("4821", &mut TestEntropy::new(), self.now);
            self.pump(target, tamper);
        }
        for _ in 0..3 {
            self.now += Duration::from_secs(10);
            self.pairing.handle_timeout(self.now);
            self.pump(target, tamper);
        }
        self
    }
}

fn untouched(reply: Vec<u8>) -> Vec<u8> {
    reply
}

// covers: P140
#[test]
fn the_pairing_survives_any_answer_before_the_pin() {
    let mut rng = Rng::new(0x5EED_0008);
    let run = PairRun::new(Answerer::script()).run(usize::MAX, &mut untouched, false);
    assert_eq!(run.pairing.state(), PairingState::WaitingForPin);
    assert_eq!(run.events, [PairingEvent::PinRequested]);
    for case in 0..1500 {
        let mut tamper = |reply| tamper(&mut rng, reply, &mut pairing_body);
        let run = PairRun::new(Answerer::script());
        let run = if case % 2 == 1 {
            run.with_password()
        } else {
            run
        };
        let _ = run.run(case % 3, &mut tamper, false);
    }
    // The answer that carries B and the salt, with a body of any kind.
    for _ in 0..2500 {
        let mut tamper = |reply: Vec<u8>| {
            let Some(Message::Response(mut response)) = read_all(&[&reply]).0.pop() else {
                panic!("the script answers");
            };
            response.body = pairing_body(&mut rng);
            response.encode()
        };
        let _ = PairRun::new(Answerer::script()).run(2, &mut tamper, false);
    }
    for body in hostile_bplists() {
        let mut tamper = |reply: Vec<u8>| {
            let Some(Message::Response(mut response)) = read_all(&[&reply]).0.pop() else {
                panic!("the script answers");
            };
            response.body = body.clone();
            response.encode()
        };
        let run = PairRun::new(Answerer::script()).run(2, &mut tamper, false);
        assert_ne!(run.pairing.state(), PairingState::WaitingForPin);
    }
}

/// A binary plist of `objects` written by hand, the first one on top;
/// references take 2 bytes and offsets 4.
fn bplist_objects(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut body = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for object in objects {
        offsets.push(u32::try_from(body.len()).expect("a small body"));
        body.extend_from_slice(object);
    }
    let table = body.len() as u64;
    for offset in offsets {
        body.extend_from_slice(&offset.to_be_bytes());
    }
    body.extend_from_slice(&[0; 6]);
    // Offset size, then reference size.
    body.push(4);
    body.push(2);
    body.extend_from_slice(&(objects.len() as u64).to_be_bytes());
    // The top object, then where the offset table starts.
    body.extend_from_slice(&0u64.to_be_bytes());
    body.extend_from_slice(&table.to_be_bytes());
    body
}

/// The answer with B and the salt plus `x`: arrays `levels` deep, each
/// holding the next one twice. As shared objects that is a few bytes a
/// level; read out, it is 2^`levels` empty arrays.
fn shared_answer(levels: u16) -> Vec<u8> {
    // {pk: 4, salt: 5, x: 6}, its keys in objects 1 to 3.
    let mut top = vec![0xD3];
    for object in 1u16..=6 {
        top.extend_from_slice(&object.to_be_bytes());
    }
    let mut objects = vec![
        top,
        b"\x52pk".to_vec(),
        b"\x54salt".to_vec(),
        b"\x51x".to_vec(),
        // Data of 256 and of 16 bytes, each length a 2- or 1-byte integer.
        [&[0x4F, 0x11, 0x01, 0x00][..], &[2; 256]].concat(),
        [&[0x4F, 0x10, 0x10][..], &[7; 16]].concat(),
    ];
    for level in 0..levels {
        let next = (7 + level).to_be_bytes();
        objects.push([&[0xA2][..], &next, &next].concat());
    }
    objects.push(vec![0xA0]);
    bplist_objects(&objects)
}

/// The answer with B and the salt plus padding data, `len` bytes in all.
fn padded_answer(len: usize) -> Vec<u8> {
    let with_padding = |padding: usize| {
        let mut body = Dictionary::new();
        body.insert("pk".to_owned(), Value::Data(vec![2; 256]));
        body.insert("salt".to_owned(), Value::Data(vec![7; 16]));
        body.insert("padding".to_owned(), Value::Data(vec![0; padding]));
        plist_bytes(&Value::Dictionary(body))
    };
    let first = len - 1024;
    let body = with_padding(first + len - with_padding(first).len());
    assert_eq!(body.len(), len);
    body
}

/// A pairing driven to the PIN whose answer to the first `/pair-setup-pin`
/// carries `body`.
fn answered_with(body: &[u8]) -> PairRun {
    let mut tamper = |reply: Vec<u8>| {
        let Some(Message::Response(mut response)) = read_all(&[&reply]).0.pop() else {
            panic!("the script answers");
        };
        response.body = body.to_vec();
        response.encode()
    };
    PairRun::new(Answerer::script()).run(2, &mut tamper, false)
}

fn assert_protocol_error(run: &PairRun) {
    assert_eq!(run.pairing.state(), PairingState::Closed);
    assert!(
        matches!(
            run.events.as_slice(),
            [PairingEvent::Failed { error, wrong_pin: false }]
                if error.kind == ErrorKind::ProtocolError
        ),
        "{:?}",
        run.events
    );
}

// covers: P140, P148
#[test]
fn a_pairing_answer_over_the_body_cap_is_refused_unread() {
    // The largest real answer, B and the salt, is far below the cap as a
    // binary plist and as XML.
    let mut real = Dictionary::new();
    real.insert("pk".to_owned(), Value::Data(vec![0xFF; 256]));
    real.insert("salt".to_owned(), Value::Data(vec![0xFF; 16]));
    let real = Value::Dictionary(real);
    let mut xml = Vec::new();
    real.to_writer_xml(&mut xml).expect("an XML plist");
    assert!(
        plist_bytes(&real).len() < 400,
        "{}",
        plist_bytes(&real).len()
    );
    assert!(xml.len() < 1024, "{} bytes of XML", xml.len());
    assert!(4 * xml.len() < MAX_PAIRING_BODY_BYTES);

    let run = answered_with(&padded_answer(MAX_PAIRING_BODY_BYTES));
    assert_eq!(run.pairing.state(), PairingState::WaitingForPin);
    assert_eq!(run.events, [PairingEvent::PinRequested]);
    assert_protocol_error(&answered_with(&padded_answer(MAX_PAIRING_BODY_BYTES + 1)));
}

// covers: P140, P148
#[test]
fn a_pairing_answer_whose_shared_objects_expand_without_bound_is_refused() {
    // A few shared levels read like any other answer.
    let run = answered_with(&shared_answer(3));
    assert_eq!(run.pairing.state(), PairingState::WaitingForPin);
    assert_eq!(run.events, [PairingEvent::PinRequested]);
    // Eighteen are a few hundred bytes that would read out as 2^18 arrays,
    // and a thousand would never finish: refused once the reading passes
    // its budget.
    let body = shared_answer(18);
    assert!(body.len() < 1024, "{} bytes", body.len());
    assert_protocol_error(&answered_with(&body));
}

// covers: P140
#[test]
fn the_pairing_survives_any_answer_to_the_proof_and_the_seal() {
    let mut rng = Rng::new(0x5EED_0009);
    // The proof's answer (the fourth), against the script; each run works
    // out the client's SRP values once.
    for _ in 0..4 {
        let mut tamper = |reply| tamper(&mut rng, reply, &mut pairing_body);
        let run = PairRun::new(Answerer::script()).run(3, &mut tamper, true);
        assert!(run.answers >= 4);
    }
    // The seal's answer (the fifth), after a real pairing with the fake
    // Apple TV.
    for _ in 0..2 {
        let mut receiver = FakeReceiver::new();
        receiver.apple_tv = Some(FakeAppleTv::new("4821"));
        let mut tamper = |reply| tamper(&mut rng, reply, &mut pairing_body);
        let run = PairRun::new(Answerer::Receiver(Box::new(receiver))).run(4, &mut tamper, true);
        assert!(run.answers >= 5);
    }
}

// covers: P55, P56
#[test]
fn pairing_values_from_the_receiver_survive_any_length() {
    let mut rng = Rng::new(0x5EED_000A);
    let group = SrpGroup::RFC5054_2048;
    let prime = group.prime();
    let mut plus_one = prime.to_vec();
    *plus_one.last_mut().expect("a prime") += 1;
    let mut minus_one = prime.to_vec();
    *minus_one.last_mut().expect("a prime") -= 1;
    let mut values = vec![
        Vec::new(),
        vec![0],
        vec![0; 300],
        prime.to_vec(),
        [vec![0; 44], prime.to_vec()].concat(),
        plus_one,
        minus_one,
        vec![0xFF; 256],
        vec![0xFF; 257],
        vec![1],
    ];
    for _ in 0..300 {
        let len = below(&mut rng, 300);
        values.push(data_of(&mut rng, len));
    }
    for b_pub in &values {
        let _ = srp::check_server_value(&group, b_pub);
        // Without a usable private value, the big arithmetic never starts.
        for private in [&[][..], &[1; 65][..]] {
            assert!(
                srp::client_values(&group, b"0123456789ABCDEF", b"4821", &[], b_pub, private)
                    .is_err()
            );
        }
        let key = srp::session_key(b_pub);
        let m1 = srp::client_proof(&group, b"0123456789ABCDEF", b_pub, b_pub, b_pub, &key);
        let _ = srp::server_proof(b_pub, &m1, &key);
        let _ = group.pad(b_pub);
    }

    // The receiver's sealed key, of every length.
    for sealed_len in 0..=40 {
        for tag_len in 0..=20 {
            let key_len = below(&mut rng, 64);
            let session_key = random_bytes(&mut rng, key_len);
            let sealed = random_bytes(&mut rng, sealed_len);
            let tag = random_bytes(&mut rng, tag_len);
            assert!(keys::open_receiver_key(&session_key, &sealed, &tag).is_none());
        }
    }

    // The pair-verify answer, of every length and some low-order keys.
    let credentials = Credentials::new("0123456789ABCDEF", [0x42; 32]).expect("credentials");
    let receiver_keys = [None, Some([0xFF; 32]), Some(credentials.public_key())];
    for receiver_key in receiver_keys {
        let credentials = match receiver_key {
            Some(key) => credentials.clone().with_receiver_key(key),
            None => credentials.clone(),
        };
        let mut verifier =
            LegacyVerifier::new(&credentials, &mut TestEntropy::new()).expect("entropy");
        for len in 0..=200 {
            if len != 96 {
                let answer = random_bytes(&mut rng, len);
                assert!(verifier.second_request(&answer).is_none());
            }
        }
        let mut answer = vec![0; 32];
        answer.extend(random_bytes(&mut rng, 64));
        assert!(
            verifier.second_request(&answer).is_none(),
            "a low-order point"
        );
        for _ in 0..8 {
            let answer = random_bytes(&mut rng, 96);
            if let Some(second) = verifier.second_request(&answer) {
                assert_eq!(second.len(), 68);
            }
            let _ = verifier.take_warning();
        }
    }

    // A stored credential blob, of every length and version.
    for len in 0..=100 {
        for version in [0u8, 1, 2, 3, 0xFF] {
            let mut blob = random_bytes(&mut rng, len);
            if let Some(first) = blob.first_mut() {
                *first = version;
            }
            if len > 17 && one_in(&mut rng, 2) {
                blob[1..17].copy_from_slice(b"0123456789ABCDEF");
            }
            if let Ok(credentials) = Credentials::from_bytes(&blob) {
                assert_eq!(credentials.to_bytes().as_slice(), blob.as_slice());
            }
        }
    }
}

// ---- The session ----------------------------------------------------------------------

/// A body for a session answer: random, a volume, a pair-verify answer of
/// the right length (random, all 0x5A, or a low-order key), or text.
fn session_body(rng: &mut Rng) -> Vec<u8> {
    match below(rng, 6) {
        0 => {
            let len = below(rng, 200);
            random_bytes(rng, len)
        }
        1 => format!("volume: {}\r\n", number(rng)).into_bytes(),
        2 => random_bytes(rng, 96),
        3 => vec![0x5A; 96],
        4 => {
            let mut body = vec![0; 32];
            body.extend(random_bytes(rng, 64));
            body
        }
        _ => text_like(rng, 80),
    }
}

/// The configurations whose answers get tampered with: plain; behind a
/// password with `qop`; PCM with the ALAC fallback to an Apple TV without
/// credentials, on software volume; and with pair-verify and `/auth-setup`
/// first.
const SESSION_VARIANTS: usize = 4;

fn session_setup(variant: usize) -> (SessionConfig, FakeReceiver) {
    let mut config = config();
    let mut receiver = FakeReceiver::new();
    match variant {
        0 => {}
        1 => {
            config.password = Some("secret".to_owned());
            let mut gate = PasswordGate::new("secret");
            gate.qop = true;
            receiver.gate = Some(gate);
        }
        2 => {
            config.codec = Codec::Pcm;
            config.alac_fallback = true;
            config.apple_tv_without_credentials = true;
            config.software_volume = true;
            config.software_mute = true;
        }
        _ => {
            let credentials =
                Credentials::new("0123456789ABCDEF", [0x42; 32]).expect("credentials");
            let verifier =
                LegacyVerifier::new(&credentials, &mut TestEntropy::new()).expect("entropy");
            config.verifier = Some(Box::new(verifier) as Box<dyn Verifier>);
            config.auth_setup_key = Some([7; 32]);
        }
    }
    (config, receiver)
}

/// A session against the fake receiver, the `target`-th answer (0-based)
/// tampered with.
struct SessionRun {
    session: Session,
    receiver: FakeReceiver,
    now: Instant,
    answers: usize,
}

impl SessionRun {
    fn pump(&mut self, target: usize, tamper: &mut dyn FnMut(Vec<u8>) -> Vec<u8>) {
        let mut rounds = 0;
        loop {
            let mut connect = false;
            while let Some(output) = self.session.poll_output() {
                rounds += 1;
                assert!(rounds < 10_000, "the session settles");
                match output {
                    session::Output::Send(bytes) => {
                        let mut reply = self.receiver.feed(&bytes);
                        if reply.is_empty() {
                            continue;
                        }
                        if self.answers == target {
                            reply = tamper(reply);
                        }
                        self.answers += 1;
                        self.session.handle_input(&reply, self.now);
                    }
                    session::Output::Connect(_) => connect = true,
                    session::Output::Event(_) | session::Output::Close => {}
                }
            }
            if !connect {
                break;
            }
            self.session.handle_connected(connection(SENDER), self.now);
        }
    }

    /// Moves the clock in 2 s steps; the receiver keeps asking for the
    /// time, as a live one does (P131).
    fn advance(&mut self, by: Duration, target: usize, tamper: &mut dyn FnMut(Vec<u8>) -> Vec<u8>) {
        let end = self.now + by;
        while self.now < end {
            self.now = (self.now + Duration::from_secs(2)).min(end);
            self.session.handle_timing_request(self.now);
            self.session.handle_timeout(self.now);
            self.pump(target, tamper);
        }
    }
}

/// Runs a session from connect to teardown, with the host's commands and
/// the keep-alive timer on the way; the number of answers it got.
fn run_session(variant: usize, target: usize, tamper: &mut dyn FnMut(Vec<u8>) -> Vec<u8>) -> usize {
    let (config, receiver) = session_setup(variant);
    let now = Instant::now();
    let mut run = SessionRun {
        session: Session::new(config, ids(), now),
        receiver,
        now,
        answers: 0,
    };
    run.pump(target, tamper);
    run.session.set_volume(Volume::Db(-10.0), run.now);
    run.session.read_volume(run.now);
    run.session.handle_timing_request(run.now);
    run.pump(target, tamper);
    // Past the response timeout, then idle into a keep-alive (P131).
    for seconds in [11, 31, 11] {
        run.advance(Duration::from_secs(seconds), target, tamper);
    }
    run.session.flush(17_100, 2_000_100, run.now);
    run.pump(target, tamper);
    run.session.teardown(run.now);
    run.pump(target, tamper);
    run.advance(Duration::from_secs(3), target, tamper);
    run.answers
}

// covers: P140
#[test]
fn a_session_survives_any_answer_at_any_step() {
    let mut rng = Rng::new(0x5EED_000B);
    for variant in 0..SESSION_VARIANTS {
        let total = run_session(variant, usize::MAX, &mut untouched);
        assert!(total >= 8, "variant {variant}: {total} answers");
        // Pair-verify does curve arithmetic on every run, so fewer runs.
        let rounds = if variant == SESSION_VARIANTS - 1 {
            3
        } else {
            10
        };
        for target in 0..total {
            for _ in 0..rounds {
                let mut tamper = |reply| tamper(&mut rng, reply, &mut session_body);
                run_session(variant, target, &mut tamper);
            }
        }
    }
}
