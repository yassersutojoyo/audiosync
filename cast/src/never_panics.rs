//! No bytes a Cast device or a peer on the network sends make the sender
//! panic, hang or keep more than its limits.
//!
//! Every parser a peer reaches gets thousands of deterministic inputs: random
//! bytes, valid frames and payloads cut short or mutated, and edges picked by
//! hand (length prefixes at and past 64 KiB, JSON nested past the parser's
//! depth limit, lists that fill a whole frame, numbers out of range, escapes
//! nested thousands deep). Each must return a value or an error, and what it
//! keeps stays within its limits. The inputs come from a fixed-seed xorshift
//! generator, so a failure shows up the same way on every run.

use std::time::{Duration, Instant};

use prost::Message;
use serde_json::{Map, Value, json};

use crate::channel::Channel;
use crate::discovery::{DeviceId, Txt};
use crate::framing::{FrameError, FrameReader, MAX_MESSAGE, decode_message, encode_frame};
use crate::group::{MAX_GROUP_MEMBERS, MemberList, MultizoneMessage};
use crate::ids::RequestIds;
use crate::media::{LoadRequest, Metadata};
use crate::message::{
    self, BROADCAST, CastMessage, NS_CONNECTION, NS_DEVICE_AUTH, NS_HEARTBEAT, NS_MEDIA,
    NS_MULTIZONE, NS_RECEIVER, PLATFORM, SENDER,
};
use crate::stream::{FetchHeaders, MAX_HEX_RUN, MAX_LOGGED_NAMES, MAX_LOGGED_TEXT};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.next().is_multiple_of(n)
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }

    fn pick<'a, T: ?Sized>(&mut self, items: &[&'a T]) -> &'a T {
        items[self.below(items.len())]
    }
}

/// Mostly bytes from `alphabet`, now and then any byte.
fn text_like(rng: &mut Rng, alphabet: &[u8], max_len: usize) -> Vec<u8> {
    let len = rng.below(max_len + 1);
    (0..len)
        .map(|_| {
            if rng.one_in(6) {
                rng.byte()
            } else {
                alphabet[rng.below(alphabet.len())]
            }
        })
        .collect()
}

fn mutate(rng: &mut Rng, base: &[u8], alphabet: &[u8]) -> Vec<u8> {
    let mut out = base.to_vec();
    for _ in 0..=rng.below(4) {
        let at = rng.below(out.len() + 1);
        match rng.below(6) {
            0 if at < out.len() => out[at] = rng.byte(),
            1 => out.insert(at, alphabet[rng.below(alphabet.len())]),
            2 if at < out.len() => {
                out.remove(at);
            }
            3 => out.truncate(at),
            4 => {
                let end = (at + rng.below(16)).min(out.len());
                let piece = out[at..end].to_vec();
                out.splice(at..at, piece);
            }
            _ => out.insert(at, rng.byte()),
        }
    }
    out
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Sorted cut points inside `0..len`, for feeding a stream in pieces.
fn random_cuts(rng: &mut Rng, len: usize) -> Vec<usize> {
    let mut cuts: Vec<usize> = (0..rng.below(6)).map(|_| rng.below(len + 1)).collect();
    cuts.sort_unstable();
    cuts
}

// ---- Payloads ------------------------------------------------------------

const NAMESPACES: &[&str] = &[
    NS_CONNECTION,
    NS_HEARTBEAT,
    NS_RECEIVER,
    NS_MEDIA,
    NS_MULTIZONE,
    NS_DEVICE_AUTH,
    "urn:x-cast:com.google.cast.media ",
    "URN:X-CAST:COM.GOOGLE.CAST.MEDIA",
    "urn:x-cast:com.example",
    "",
];

const TYPES: &[&str] = &[
    "RECEIVER_STATUS",
    "MEDIA_STATUS",
    "PING",
    "PONG",
    "CLOSE",
    "CONNECT",
    "LAUNCH_ERROR",
    "INVALID_REQUEST",
    "LOAD_FAILED",
    "LOAD_CANCELLED",
    "INVALID_PLAYER_STATE",
    "MULTIZONE_STATUS",
    "DEVICE_ADDED",
    "DEVICE_UPDATED",
    "DEVICE_REMOVED",
    "receiver_status",
    "",
];

const KEYS: &[&str] = &[
    "type",
    "requestId",
    "status",
    "volume",
    "level",
    "muted",
    "controlType",
    "stepInterval",
    "applications",
    "appId",
    "sessionId",
    "transportId",
    "namespaces",
    "name",
    "displayName",
    "statusText",
    "mediaSessionId",
    "playerState",
    "idleReason",
    "currentTime",
    "media",
    "contentId",
    "reason",
    "detailedErrorCode",
    "devices",
    "device",
    "deviceId",
    "x",
];

const STRINGS: &[&str] = &[
    "CC1AD845",
    "web-7",
    "sess-1",
    "PLAYING",
    "BUFFERING",
    "IDLE",
    "PAUSED",
    "FINISHED",
    "ERROR",
    "INTERRUPTED",
    "CANCELLED",
    "attenuation",
    "fixed",
    "master",
    "4f6c2c1e-7a1b-4c39-9b44-0a1b2c3d4e5f",
    "4F6C2C1E7A1B4C399B440A1B2C3D4E5F",
    "receiver-0",
    "sender-0",
    "*",
    "",
    "\u{0}",
    "\u{202e}\u{1f50a}",
];

const JSON_BYTES: &[u8] = b"{}[]\",:0123456789.eE-+ \\u truefalsenull type requestId status";

fn random_number(rng: &mut Rng, last_id: u32) -> Value {
    match rng.below(14) {
        0 => json!(0),
        1 => json!(1),
        2 => json!(-1),
        3 => json!(0.5),
        4 => json!(1.5),
        5 => json!(1e308),
        6 => json!(-1e308),
        7 => json!(u64::MAX),
        8 => json!(i64::MIN),
        9 => json!(u64::from(u32::MAX) + 1),
        10 => json!(5e-324),
        11 => json!(last_id),
        12 => json!(rng.next() as i64),
        _ => json!((rng.next() % 2_000) as f64 / 1_000.0 - 0.5),
    }
}

fn random_string(rng: &mut Rng) -> String {
    if rng.one_in(4) {
        lossy(&text_like(rng, b"abcdef0123456789-_ %", 40))
    } else {
        rng.pick(STRINGS).to_owned()
    }
}

fn random_value(rng: &mut Rng, depth: usize, last_id: u32) -> Value {
    let kinds = if depth == 0 { 4 } else { 7 };
    match rng.below(kinds) {
        0 => Value::Null,
        1 => Value::Bool(rng.one_in(2)),
        2 => random_number(rng, last_id),
        3 => Value::String(random_string(rng)),
        4 => Value::Array(
            (0..rng.below(4))
                .map(|_| random_value(rng, depth - 1, last_id))
                .collect(),
        ),
        _ => {
            let mut object = Map::new();
            for _ in 0..rng.below(5) {
                let key = rng.pick(KEYS).to_owned();
                object.insert(key, random_value(rng, depth - 1, last_id));
            }
            Value::Object(object)
        }
    }
}

/// Puts random keys with random values into the objects of `value`.
fn sprinkle(rng: &mut Rng, value: &mut Value, last_id: u32) {
    match value {
        Value::Object(object) => {
            if rng.one_in(3) {
                let key = rng.pick(KEYS).to_owned();
                object.insert(key, random_value(rng, 2, last_id));
            }
            for (_, child) in object.iter_mut() {
                if rng.one_in(2) {
                    sprinkle(rng, child, last_id);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                if rng.one_in(2) {
                    sprinkle(rng, item, last_id);
                }
            }
        }
        _ => {}
    }
}

fn app_entry(rng: &mut Rng) -> Value {
    json!({
        "appId": if rng.one_in(4) { random_string(rng) } else { "CC1AD845".to_owned() },
        "displayName": "Default Media Receiver",
        "sessionId": if rng.one_in(3) { random_string(rng) } else { "sess-1".to_owned() },
        "transportId": if rng.one_in(3) { random_string(rng) } else { "web-7".to_owned() },
        "namespaces": [{"name": NS_MEDIA}],
        "statusText": random_string(rng),
    })
}

fn member(rng: &mut Rng) -> Value {
    json!({"deviceId": random_string(rng), "name": random_string(rng)})
}

/// A payload shaped like one a device sends, with the request id and the
/// values varied, or now and then any JSON at all.
fn device_payload(rng: &mut Rng, last_id: u32) -> Value {
    let id = if rng.one_in(4) {
        random_number(rng, last_id)
    } else {
        json!(last_id)
    };
    let mut payload = match rng.below(9) {
        0 => json!({
            "type": "RECEIVER_STATUS",
            "requestId": id,
            "status": {
                "volume": {
                    "level": random_number(rng, last_id),
                    "muted": rng.one_in(2),
                    "controlType": rng.pick(&["attenuation", "fixed", "master", "other"]),
                    "stepInterval": random_number(rng, last_id),
                },
                "applications": (0..rng.below(3)).map(|_| app_entry(rng)).collect::<Vec<_>>(),
            },
        }),
        1 => json!({
            "type": "MEDIA_STATUS",
            "requestId": id,
            "status": (0..rng.below(3)).map(|_| json!({
                "mediaSessionId": random_number(rng, last_id),
                "playerState": rng.pick(&["PLAYING", "BUFFERING", "IDLE", "PAUSED", "?"]),
                "idleReason": random_string(rng),
                "currentTime": random_number(rng, last_id),
                "media": {"contentId": random_string(rng)},
            })).collect::<Vec<_>>(),
        }),
        2 => json!({
            "type": "MULTIZONE_STATUS",
            "requestId": id,
            "status": {"devices": (0..rng.below(4)).map(|_| member(rng)).collect::<Vec<_>>()},
        }),
        3 => json!({
            "type": rng.pick(&["DEVICE_ADDED", "DEVICE_UPDATED"]),
            "device": member(rng),
        }),
        4 => json!({"type": "DEVICE_REMOVED", "deviceId": random_string(rng)}),
        5 => json!({"type": rng.pick(&["PING", "CLOSE"])}),
        6 => json!({
            "type": rng.pick(&["LAUNCH_ERROR", "INVALID_REQUEST", "LOAD_FAILED",
                "LOAD_CANCELLED", "INVALID_PLAYER_STATE"]),
            "requestId": id,
            "reason": random_string(rng),
            "detailedErrorCode": random_number(rng, last_id),
        }),
        7 => {
            let mut object = Map::new();
            object.insert("type".to_owned(), json!(rng.pick(TYPES)));
            object.insert("requestId".to_owned(), id);
            Value::Object(object)
        }
        _ => random_value(rng, 4, last_id),
    };
    if rng.one_in(2) {
        sprinkle(rng, &mut payload, last_id);
    }
    payload
}

fn payload_text(rng: &mut Rng, last_id: u32) -> String {
    let text = device_payload(rng, last_id).to_string();
    match rng.below(10) {
        0 => lossy(&mutate(rng, text.as_bytes(), JSON_BYTES)),
        1 => lossy(&text_like(rng, JSON_BYTES, 64)),
        _ => text,
    }
}

fn random_message(rng: &mut Rng, last_id: u32) -> CastMessage {
    let source = if rng.one_in(5) {
        random_string(rng)
    } else {
        rng.pick(&[PLATFORM, "web-7", "web-7", SENDER]).to_owned()
    };
    let destination = if rng.one_in(8) {
        random_string(rng)
    } else {
        rng.pick(&[SENDER, BROADCAST]).to_owned()
    };
    let namespace = if rng.one_in(6) {
        rng.pick(NAMESPACES).to_owned()
    } else {
        rng.pick(&[
            NS_RECEIVER,
            NS_MEDIA,
            NS_MULTIZONE,
            NS_HEARTBEAT,
            NS_CONNECTION,
        ])
        .to_owned()
    };
    let payload_type = if rng.one_in(12) {
        *rng.pick(&[&1, &2, &-1, &i32::MAX])
    } else {
        0
    };
    CastMessage {
        protocol_version: if rng.one_in(12) { 1 } else { 0 },
        source_id: source,
        destination_id: destination,
        namespace,
        payload_type,
        payload_utf8: (!rng.one_in(15)).then(|| payload_text(rng, last_id)),
        payload_binary: rng.one_in(10).then(|| text_like(rng, JSON_BYTES, 32)),
    }
}

// ---- Frames ----------------------------------------------------------------

/// Bytes as a device might send them: whole frames, frames whose body is
/// garbage, length prefixes at the edges, and loose bytes.
fn random_stream(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..rng.below(5) {
        match rng.below(10) {
            0..=4 => out.extend(encode_frame(&random_message(rng, 1)).unwrap_or_default()),
            5 | 6 => {
                let body = text_like(rng, JSON_BYTES, 48);
                out.extend_from_slice(&(body.len() as u32).to_be_bytes());
                out.extend(body);
            }
            7 => {
                let length = *rng.pick(&[
                    &0u32,
                    &1,
                    &(MAX_MESSAGE as u32),
                    &(MAX_MESSAGE as u32 + 1),
                    &u32::MAX,
                    &0x8000_0000,
                ]);
                out.extend_from_slice(&length.to_be_bytes());
            }
            _ => out.extend((0..rng.below(12)).map(|_| rng.byte())),
        }
    }
    if rng.one_in(3) {
        out = mutate(rng, &out, &[0, 0, 0, 0x0a, 0x12, 0x22, 0x08, 0xff]);
    }
    out
}

/// What a reader gives for `data` pushed in the pieces between `cuts`: the
/// messages, then the error that ends the channel, if one does.
fn read_frames(data: &[u8], cuts: &[usize]) -> (Vec<CastMessage>, Option<FrameError>) {
    let mut reader = FrameReader::new();
    let mut messages = Vec::new();
    let mut start = 0;
    for &end in cuts.iter().chain(std::iter::once(&data.len())) {
        reader.push(&data[start..end]);
        start = end;
        loop {
            match reader.next_message() {
                Ok(Some(message)) => messages.push(message),
                Ok(None) => break,
                Err(error) => return (messages, Some(error)),
            }
        }
        assert!(
            reader.buffered() < 4 + MAX_MESSAGE,
            "{} bytes held for an incomplete frame",
            reader.buffered()
        );
    }
    (messages, None)
}

#[test]
fn frames_from_any_bytes_decode_the_same_however_they_arrive() {
    let mut rng = Rng::new(0xca57_f4a3);
    for _ in 0..8_000 {
        let data = random_stream(&mut rng);
        let whole = read_frames(&data, &[]);
        let cuts = random_cuts(&mut rng, data.len());
        assert_eq!(read_frames(&data, &cuts), whole, "{data:?} cut at {cuts:?}");
        if rng.one_in(50) {
            let every: Vec<usize> = (1..data.len()).collect();
            assert_eq!(read_frames(&data, &every), whole);
        }
        for message in whole.0 {
            assert_eq!(decode_message(&message.encode_to_vec()), Ok(message));
        }
    }
}

#[test]
fn frame_edges_get_the_documented_answers() {
    let frame = |length: u32, body: &[u8]| {
        let mut out = length.to_be_bytes().to_vec();
        out.extend_from_slice(body);
        out
    };
    let valid = encode_frame(&CastMessage::json(
        SENDER,
        message::Namespace::Heartbeat,
        &json!({"type": "PING"}),
    ))
    .unwrap();
    let mut valid_then_huge = valid.clone();
    valid_then_huge.extend_from_slice(&u32::MAX.to_be_bytes());
    let cases: Vec<(Vec<u8>, usize, Option<FrameError>)> = vec![
        (Vec::new(), 0, None),
        (vec![0, 0, 1], 0, None),
        (frame(0, &[]), 1, None),
        (
            frame(MAX_MESSAGE as u32, &[0; MAX_MESSAGE]),
            0,
            Some(FrameError::Malformed),
        ),
        (frame(MAX_MESSAGE as u32, &[0; 100]), 0, None),
        (
            frame(MAX_MESSAGE as u32 + 1, &[]),
            0,
            Some(FrameError::TooLarge(MAX_MESSAGE + 1)),
        ),
        (
            frame(u32::MAX, &[]),
            0,
            Some(FrameError::TooLarge(u32::MAX as usize)),
        ),
        (
            valid_then_huge,
            1,
            Some(FrameError::TooLarge(u32::MAX as usize)),
        ),
        (
            frame(3, &[0xff, 0xff, 0xff]),
            0,
            Some(FrameError::Malformed),
        ),
        // Field 6 (payload_utf8) holding bytes that aren't UTF-8.
        (
            frame(3, &[0x32, 0x01, 0xff]),
            0,
            Some(FrameError::Malformed),
        ),
    ];
    for (data, count, error) in cases {
        let (messages, got) = read_frames(&data, &[]);
        assert_eq!(
            (messages.len(), got),
            (count, error),
            "{:?}",
            &data[..data.len().min(16)]
        );
    }
}

// ---- The channel -------------------------------------------------------------

/// The last request the channel sent.
#[derive(Default)]
struct Sent {
    id: u32,
    namespace: String,
    kind: String,
}

/// Empties the channel's queues, checking what it would send, and notes
/// the last request in `sent`.
fn drain(channel: &mut Channel, sent: &mut Sent) {
    while let Some(outbound) = channel.poll_outbound() {
        // An answer echoes the device's source id, so one to a source id
        // that filled a whole frame can come out a few bytes over; the
        // channel then ends, as for any frame it can't send.
        assert_ne!(encode_frame(&outbound).err(), Some(FrameError::Malformed));
        if let Ok(payload) = outbound.json_payload() {
            let id = message::request_id(&payload);
            if id != 0 {
                sent.id = id;
                sent.namespace = outbound.namespace.clone();
                sent.kind = message::message_type(&payload).unwrap_or("").to_owned();
            }
        }
    }
    while channel.poll_event().is_some() {}
}

/// The answer a working device gives to the last request, so the channel
/// gets past launching and loading, now and then with a value changed.
fn answer(rng: &mut Rng, sent: &Sent) -> CastMessage {
    let (source, namespace, mut payload) = if sent.namespace == NS_MEDIA {
        let state = rng.pick(&["PLAYING", "PLAYING", "BUFFERING", "IDLE"]);
        let entry = json!({"mediaSessionId": 1, "playerState": state,
            "idleReason": rng.pick(&["FINISHED", "ERROR", "INTERRUPTED"])});
        let payload = json!({"type": "MEDIA_STATUS", "requestId": sent.id, "status": [entry]});
        ("web-7", NS_MEDIA, payload)
    } else {
        let apps = if sent.kind == "STOP" && rng.one_in(2) {
            Vec::new()
        } else {
            vec![app_entry(rng)]
        };
        let payload = json!({"type": "RECEIVER_STATUS", "requestId": sent.id,
            "status": {"volume": {"level": 0.5, "stepInterval": 0.05}, "applications": apps}});
        (PLATFORM, NS_RECEIVER, payload)
    };
    if rng.one_in(4) {
        sprinkle(rng, &mut payload, sent.id);
    }
    CastMessage {
        source_id: source.to_owned(),
        namespace: namespace.to_owned(),
        ..CastMessage::json(SENDER, message::Namespace::Receiver, &payload)
    }
}

fn load_request() -> LoadRequest {
    LoadRequest {
        url: "http://192.0.2.1:8008/0123456789abcdef.wav".to_owned(),
        content_type: "audio/wav".to_owned(),
        metadata: Metadata::default(),
    }
}

/// Host calls mixed with device messages, so the messages reach the
/// channel in every phase: before the first status, launching, loading,
/// playing, stopping and ended.
#[test]
fn a_channel_takes_any_device_message_in_any_phase() {
    let mut rng = Rng::new(0x0c4a_77e1);
    let load = load_request();
    let mut handled = 0;
    for _ in 0..2_000 {
        let mut now = Instant::now();
        let mut channel = Channel::open(now, RequestIds::starting_at(rng.next() as u32));
        let mut sent = Sent::default();
        drain(&mut channel, &mut sent);
        for _ in 0..rng.below(50) {
            match rng.below(24) {
                0 | 1 => {
                    let _ = channel.launch(now);
                }
                2 | 3 => {
                    let _ = channel.load(now, &load);
                }
                4 => {
                    let level = *rng.pick(&[&0.0, &0.25, &1.0, &-1.0, &2.0, &f64::MAX]);
                    let _ = channel.set_volume(now, level);
                }
                5 => {
                    let _ = channel.step_volume(now, rng.one_in(2));
                }
                6 => {
                    let _ = channel.set_muted(rng.one_in(2));
                }
                7 if rng.one_in(4) => channel.stop(now),
                8 => {
                    let step = if rng.one_in(8) { 20_000 } else { 3_000 };
                    now += Duration::from_millis(rng.below(step) as u64);
                    channel.tick(now);
                }
                9 => channel.request_members(),
                10 => {
                    if let Some(app) = channel.app().cloned() {
                        let session = (!rng.one_in(3)).then_some(rng.next() as i64);
                        channel.resume(now, &app, session, &load.url);
                    }
                }
                11..=16 => {
                    channel.handle(now, answer(&mut rng, &sent));
                    handled += 1;
                }
                _ => {
                    channel.handle(now, random_message(&mut rng, sent.id));
                    handled += 1;
                }
            }
            let _ = channel.next_deadline();
            let _ = (channel.status(), channel.is_ready(), channel.is_ended());
            let _ = (channel.media_session_id(), channel.is_loading());
            drain(&mut channel, &mut sent);
        }
    }
    assert!(handled > 25_000, "only {handled} messages handled");
}

/// A message with `payload` that fills its frame, as it arrives off the
/// wire.
fn through_frame(namespace: &str, payload: String) -> CastMessage {
    let message = CastMessage {
        namespace: namespace.to_owned(),
        payload_utf8: Some(payload),
        ..CastMessage::json(SENDER, message::Namespace::Receiver, &json!({}))
    };
    let frame = encode_frame(&message).expect("the case fits a frame");
    let (mut messages, error) = read_frames(&frame, &[]);
    assert_eq!((messages.len(), error), (1, None));
    messages.remove(0)
}

/// Fills about 60 KiB with `item`, joined by commas.
fn repeated(item: &str) -> String {
    vec![item; 60_000 / (item.len() + 1)].join(",")
}

#[test]
fn deep_and_huge_payloads_inside_a_frame_are_read_or_dropped() {
    let nested = |depth: usize| {
        format!(
            "{{\"type\":\"RECEIVER_STATUS\",\"x\":{}{}}}",
            "[".repeat(depth),
            "]".repeat(depth)
        )
    };
    let app = app_entry(&mut Rng::new(1)).to_string();
    let entry = r#"{"mediaSessionId":1,"playerState":"PLAYING","currentTime":1e308}"#;
    let device = r#"{"deviceId":"4f6c2c1e-7a1b-4c39-9b44-0a1b2c3d4e5f","name":"x"}"#;
    let cases: Vec<(&str, String)> = vec![
        (NS_RECEIVER, "[".repeat(65_000)),
        (NS_RECEIVER, "{\"a\":".repeat(10_000)),
        (NS_RECEIVER, nested(126)),
        (NS_RECEIVER, nested(127)),
        (NS_RECEIVER, nested(128)),
        (NS_RECEIVER, nested(20_000)),
        (
            NS_RECEIVER,
            format!(
                r#"{{"type":"RECEIVER_STATUS","requestId":1,"status":{{"applications":[{}]}}}}"#,
                repeated(&app)
            ),
        ),
        (
            NS_RECEIVER,
            format!(
                r#"{{"type":"RECEIVER_STATUS","status":{{"volume":{{"level":1e308,"stepInterval":1e308}},{}}}}}"#,
                repeated(r#""k":"v""#)
            ),
        ),
        (
            NS_RECEIVER,
            format!(
                r#"{{"type":"LAUNCH_ERROR","reason":"{}"}}"#,
                "\u{e9}".repeat(30_000)
            ),
        ),
        (
            NS_MEDIA,
            format!(
                r#"{{"type":"MEDIA_STATUS","status":[{}]}}"#,
                repeated(entry)
            ),
        ),
        (
            NS_MULTIZONE,
            format!(
                r#"{{"type":"MULTIZONE_STATUS","status":{{"devices":[{}]}}}}"#,
                repeated(device)
            ),
        ),
        (
            NS_RECEIVER,
            r#"{"type":"RECEIVER_STATUS","requestId":1e400}"#.to_owned(),
        ),
        (
            NS_RECEIVER,
            r#"{"type":"RECEIVER_STATUS","requestId":-0}"#.to_owned(),
        ),
        (
            NS_RECEIVER,
            r#"{"type":"RECEIVER_STATUS","requestId":18446744073709551616}"#.to_owned(),
        ),
        (NS_HEARTBEAT, r#"{"type":"PING","type":"CLOSE"}"#.to_owned()),
        (NS_HEARTBEAT, r#"{"type":"PING\ud800"}"#.to_owned()),
        (NS_HEARTBEAT, "{\"type\":\"PING\u{0}\"}".to_owned()),
        (NS_HEARTBEAT, String::new()),
    ];
    for (namespace, payload) in cases {
        let label = payload.chars().take(48).collect::<String>();
        let message = through_frame(namespace, payload);
        let started = Instant::now();
        let mut now = Instant::now();
        let mut channel = Channel::open(now, RequestIds::starting_at(1));
        channel.request_members();
        drain(&mut channel, &mut Sent::default());
        let status = json!({"type": "RECEIVER_STATUS", "requestId": 1,
            "status": {"applications": [{"appId": "CC1AD845", "sessionId": "sess-1",
            "transportId": "web-7"}]}});
        channel.handle(
            now,
            CastMessage::json(SENDER, message::Namespace::Receiver, &status),
        );
        let mut sources = [PLATFORM, "web-7"].into_iter();
        for round in 0..4 {
            let source = sources.next().unwrap_or("web-7");
            channel.handle(
                now,
                CastMessage {
                    source_id: source.to_owned(),
                    ..message.clone()
                },
            );
            drain(&mut channel, &mut Sent::default());
            if round == 1 {
                let _ = channel.launch(now);
                let _ = channel.load(now, &load_request());
            }
            now += Duration::from_secs(1);
            channel.tick(now);
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{label:?} took {:?}",
            started.elapsed()
        );
    }
}

// ---- Discovery ---------------------------------------------------------------

const TXT_BYTES: &[u8] = b"===idmdfnrsvecaIDMDFN0123456789-_ \xc3\xa9";

#[test]
fn txt_strings_parse_into_unique_lowercase_keys() {
    let mut rng = Rng::new(0x7e57_0d15);
    for _ in 0..6_000 {
        let strings: Vec<Vec<u8>> = (0..rng.below(12))
            .map(|_| {
                if rng.one_in(3) {
                    let key = rng.pick(&["id", "ID", "fn", "md", "ca", "st", "rs", "Fn"]);
                    let mut string = format!("{key}=").into_bytes();
                    string.extend(text_like(&mut rng, TXT_BYTES, 40));
                    string
                } else {
                    text_like(&mut rng, TXT_BYTES, 24)
                }
            })
            .collect();
        let txt = Txt::parse(&strings);
        let keys: Vec<&str> = txt.keys().collect();
        for (at, key) in keys.iter().enumerate() {
            assert!(!key.is_empty());
            assert_eq!(key.to_ascii_lowercase(), *key);
            assert!(!keys[..at].iter().any(|k| k.eq_ignore_ascii_case(key)));
            let first = strings.iter().find_map(|string| {
                let split = string.iter().position(|&byte| byte == b'=')?;
                let name = std::str::from_utf8(&string[..split]).ok()?;
                name.eq_ignore_ascii_case(key).then(|| &string[split + 1..])
            });
            assert_eq!(txt.get(&key.to_ascii_uppercase()), first);
            let _ = txt.text(key);
        }
    }
}

#[test]
fn device_ids_parse_only_as_uuids_and_print_back() {
    let mut rng = Rng::new(0xd1d_1d5);
    let pieces: &[&str] = &[
        "0", "9", "a", "f", "A", "F", "g", "-", "\u{e9}", "\u{0}", " ",
    ];
    for _ in 0..12_000 {
        let base = "4f6c2c1e-7a1b-4c39-9b44-0a1b2c3d4e5f";
        let text = match rng.below(4) {
            0 => base.to_owned(),
            1 => base.replace('-', ""),
            2 => (0..rng.pick(&[&31usize, &32, &33, &35, &36, &37]).to_owned())
                .map(|_| rng.pick(pieces))
                .collect::<String>(),
            _ => (0..rng.below(40)).map(|_| rng.pick(pieces)).collect(),
        };
        let text = if rng.one_in(2) {
            let mut bytes = text.into_bytes();
            let at = rng.below(bytes.len() + 1);
            bytes.insert(at, *rng.pick(&[&b'-', &b'0', &b'G', &b'b']));
            if rng.one_in(2) && !bytes.is_empty() {
                let at = rng.below(bytes.len());
                bytes.remove(at);
            }
            lossy(&bytes)
        } else {
            text
        };
        let Some(id) = DeviceId::parse(&text) else {
            continue;
        };
        assert!(text.len() == 32 || text.len() == 36, "{text:?}");
        let printed = id.to_string();
        assert_eq!(printed, text.replace('-', "").to_ascii_lowercase());
        assert_eq!(DeviceId::parse(&printed), Some(id));
    }
}

// ---- Groups --------------------------------------------------------------------

#[test]
fn multizone_messages_never_grow_a_member_list_past_its_cap() {
    let mut rng = Rng::new(0x006a_0ce5);
    for _ in 0..1_500 {
        let mut members = MemberList::new();
        for _ in 0..rng.below(12) {
            let mut payload = match device_payload(&mut rng, 1) {
                Value::Object(object) => object,
                other => Map::from_iter([("status".to_owned(), other)]),
            };
            if rng.one_in(4) {
                let devices: Vec<Value> = (0..rng.below(100)).map(|_| member(&mut rng)).collect();
                payload.insert("status".to_owned(), json!({"devices": devices}));
            }
            let kind = message::message_type(&payload)
                .unwrap_or_else(|| rng.pick(TYPES))
                .to_owned();
            let parsed = MultizoneMessage::from_payload(&kind, &payload);
            members.apply(&parsed);
            assert!(
                members
                    .members()
                    .is_none_or(|list| list.len() <= MAX_GROUP_MEMBERS)
            );
        }
    }
}

// ---- The stream server's request headers ---------------------------------------

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const HEADER_BYTES: &[u8] = b"%%%2525250123456789abcdefABCDEF:: -=,bytes/\t";

fn random_header_line(rng: &mut Rng) -> String {
    let name = if rng.one_in(5) {
        lossy(&text_like(rng, HEADER_BYTES, 24))
    } else {
        rng.pick(&[
            "User-Agent",
            "Range",
            "Icy-MetaData",
            "Referer",
            "Host",
            "range",
        ])
        .to_owned()
    };
    let value = match rng.below(8) {
        0 => format!("/{TOKEN}.wav"),
        1 => format!("%2F{}", TOKEN.to_ascii_uppercase()),
        2 => TOKEN
            .bytes()
            .map(|byte| format!("%25{byte:02x}"))
            .collect::<String>(),
        3 => format!("bytes={}-{}", rng.next(), rng.next() % 100),
        4 => format!("%{}41", "25".repeat(rng.below(200))),
        5 => lossy(&text_like(rng, HEADER_BYTES, 120)),
        _ => rng
            .pick(&["Lavf/58.76.100", "1", "bytes=0-", "x"])
            .to_owned(),
    };
    let separator = rng.pick(&[": ", ":", " : ", ""]);
    format!("{name}{separator}{value}")
}

fn check_logged(text: &str, is_range: bool) {
    assert!(text.chars().count() <= MAX_LOGGED_TEXT, "{text:?}");
    assert!(!text.to_ascii_lowercase().contains(TOKEN), "{text:?}");
    let mut run = 0;
    let hex_run = text.bytes().any(|byte| {
        run = if byte.is_ascii_hexdigit() { run + 1 } else { 0 };
        run >= MAX_HEX_RUN
    });
    if hex_run {
        let byte_range = text.strip_prefix("bytes=").is_some_and(|ranges| {
            ranges
                .bytes()
                .all(|byte| byte.is_ascii_digit() || b" ,-".contains(&byte))
        });
        assert!(is_range && byte_range, "{text:?} kept a hex run");
    }
}

#[test]
fn fetch_headers_from_any_lines_stay_capped_and_scrubbed() {
    let mut rng = Rng::new(0x0005_7ea3);
    for _ in 0..5_000 {
        let lines: Vec<String> = (0..rng.below(40))
            .map(|_| {
                let line = random_header_line(&mut rng);
                if rng.one_in(6) {
                    lossy(&mutate(&mut rng, line.as_bytes(), HEADER_BYTES))
                } else {
                    line
                }
            })
            .collect();
        let headers = FetchHeaders::parse(lines.iter().map(String::as_str));
        let with_colon = lines.iter().filter(|line| line.contains(':')).count();
        assert!(headers.names.len() <= MAX_LOGGED_NAMES);
        assert_eq!(headers.names.len() + headers.more, with_colon);
        let logged = headers.for_log(TOKEN);
        logged
            .names
            .iter()
            .for_each(|name| check_logged(name, false));
        [&logged.user_agent, &logged.icy_metadata]
            .into_iter()
            .flatten()
            .for_each(|value| check_logged(value, false));
        if let Some(range) = &logged.range {
            check_logged(range, true);
        }
    }
}

/// A request is at most 8 KiB plus one read, and each nesting of `%25`
/// costs the decoder a pass over the text.
#[test]
fn escapes_nested_through_a_whole_request_are_logged_quickly() {
    let deep = format!("%{}41", "25".repeat(4_000));
    let half = format!("%{}41", "25".repeat(2_000));
    let requests = [
        vec![format!("User-Agent: {deep}")],
        vec![format!("{deep}: x")],
        vec![format!("{half}: {half}")],
        vec![format!("Range: {half}"), format!("Icy-MetaData: {half}")],
    ];
    for lines in requests {
        let headers = FetchHeaders::parse(lines.iter().map(String::as_str));
        let started = Instant::now();
        let logged = headers.for_log(TOKEN);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "for_log took {elapsed:?}"
        );
        logged
            .names
            .iter()
            .for_each(|name| check_logged(name, false));
    }
}
