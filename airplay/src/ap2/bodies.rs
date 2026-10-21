//! The binary property list bodies of the AirPlay 2 session (P41 to P44,
//! P106, P164): what the sender writes into `/info`, the session and stream
//! SETUP, SETPEERS and TEARDOWN, and the values it reads from the answers.
//! A value of an unexpected type in an answer is ignored as if it were
//! absent (P41).

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::net::IpAddr;

use plist::{Dictionary, Integer, Value};

use crate::discovery::Features;
use crate::discovery::txt::{AirPlayTxt, Txt};

pub const PLIST_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
pub const PEERS_CONTENT_TYPE: &str = "/peer-list-changed";
pub const PROTOCOL_VERSION_HEADER: &str = "X-Apple-ProtocolVersion";
/// The name and model the sender gives itself (P43).
pub const SENDER_NAME: &str = "AudioSync";
/// Realtime audio (P44); 103, buffered audio, is never set up (P168).
pub const STREAM_TYPE_REALTIME: i64 = 96;
/// ALAC (P44).
pub const COMPRESSION_ALAC: i64 = 2;
/// ALAC at 44 100 Hz, 16-bit stereo (P44).
pub const AUDIO_FORMAT_ALAC_44100_16_2: i64 = 0x40000;
pub const FRAMES_PER_PACKET: i64 = 352;
pub const SAMPLE_RATE: i64 = 44_100;
/// The latencies of the published capture, in frames (P44).
pub const LATENCY_MIN: i64 = 11_025;
pub const LATENCY_MAX: i64 = 88_200;
/// Bytes of the stream key `shk` (P44, P156).
pub const STREAM_KEY_BYTES: usize = 32;
/// The largest answer body read as a property list. Answers are small
/// dictionaries; a cap this low also bounds how deep a hostile body can
/// nest, so reading and dropping it fit the driver thread's stack.
pub const MAX_PLIST_BYTES: usize = 8 * 1024;
const PLIST_READ_BUDGET: usize = 8 * MAX_PLIST_BYTES;
const BINARY_MAGIC: &[u8] = b"bplist00";

/// The timing a session SETUP asks for (P104, P157).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timing {
    Ntp,
    Ptp,
}

/// The sender's PTP clock as a session SETUP names it (P159, P164). It
/// comes from the PTP service, which the host doesn't run yet; without it a
/// session asks for NTP only (P157).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtpPeer {
    /// The 8-byte clock identity, 0xFFFF then 6 random bytes (P159).
    pub clock_id: u64,
    /// A random UUID string made when the PTP service starts (P164).
    pub id: String,
}

/// A random UUID (RFC 9562 version 4) in lowercase text form.
pub fn uuid_text(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// The device ID of `deviceID` and `macAddress` (P5, P43): six random bytes
/// in colon form, marked as a locally administered unicast address so it
/// can't be mistaken for a hardware one.
pub fn device_id_text(mut bytes: [u8; 6]) -> String {
    bytes[0] = (bytes[0] | 0x02) & !0x01;
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn int(value: i64) -> Value {
    Value::Integer(Integer::from(value))
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn dict(entries: Vec<(&str, Value)>) -> Dictionary {
    let mut dictionary = Dictionary::new();
    for (key, value) in entries {
        dictionary.insert(key.to_owned(), value);
    }
    dictionary
}

/// A value as a binary property list. Writing to memory fails only on
/// values the plist format can't hold, which these builders never make.
pub fn binary(value: Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = value.to_writer_binary(&mut bytes);
    bytes
}

/// The body of the first `/info`: {`qualifier`: [`txtAirPlay`]} (P41).
pub fn info_request() -> Vec<u8> {
    binary(Value::Dictionary(dict(vec![(
        "qualifier",
        Value::Array(vec![text("txtAirPlay")]),
    )])))
}

/// What the sender identifies itself with in the session SETUP (P43).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSetupBody<'a> {
    pub device_id: &'a str,
    pub session_uuid: &'a str,
    pub group_uuid: &'a str,
    /// The sender's timing port, sent under NTP (P43).
    pub timing_port: u16,
    pub timing: Timing,
    /// The PTP clock and the sender's own address on the connection, sent
    /// under PTP (P164).
    pub ptp: Option<(&'a PtpPeer, IpAddr)>,
}

/// The session SETUP body (P43, P164): `et` 0 and no `ekey` or `eiv`; NTP
/// with the sender's `timingPort`, or PTP with `timingPeerInfo` and
/// `timingPeerList` and no `timingPort`.
pub fn session_setup(body: &SessionSetupBody<'_>) -> Vec<u8> {
    let mut entries = vec![
        ("deviceID", text(body.device_id)),
        ("macAddress", text(body.device_id)),
        ("sessionUUID", text(body.session_uuid)),
        ("name", text(SENDER_NAME)),
        ("model", text(SENDER_NAME)),
        ("sourceVersion", text(crate::VERSION)),
        ("groupUUID", text(body.group_uuid)),
        ("groupContainsGroupLeader", Value::Boolean(false)),
        ("isMultiSelectAirPlay", Value::Boolean(false)),
        ("et", int(0)),
    ];
    match (body.timing, body.ptp) {
        (Timing::Ptp, Some((peer, local))) => {
            let info = Value::Dictionary(dict(vec![
                (
                    "Addresses",
                    Value::Array(vec![text(&local.to_canonical().to_string())]),
                ),
                ("ID", text(&peer.id)),
                ("DeviceType", int(0)),
                // The clock identity's 64 bits, which a plist reads as a
                // signed integer (P164).
                ("ClockID", int(peer.clock_id as i64)),
                ("SupportsClockPortMatchingOverride", Value::Boolean(false)),
            ]));
            entries.push(("timingProtocol", text("PTP")));
            entries.push(("timingPeerList", Value::Array(vec![info.clone()])));
            entries.push(("timingPeerInfo", info));
        }
        _ => {
            entries.push(("timingProtocol", text("NTP")));
            entries.push(("timingPort", int(i64::from(body.timing_port))));
        }
    }
    binary(Value::Dictionary(dict(entries)))
}

/// The stream SETUP body (P44): one realtime ALAC stream with the stream
/// key, the sender's control port and the session ID as
/// `streamConnectionID`.
pub fn stream_setup(
    stream_key: &[u8; STREAM_KEY_BYTES],
    control_port: u16,
    session_id: u32,
) -> Vec<u8> {
    let stream = dict(vec![
        ("type", int(STREAM_TYPE_REALTIME)),
        ("ct", int(COMPRESSION_ALAC)),
        ("audioFormat", int(AUDIO_FORMAT_ALAC_44100_16_2)),
        ("spf", int(FRAMES_PER_PACKET)),
        ("shk", Value::Data(stream_key.to_vec())),
        ("controlPort", int(i64::from(control_port))),
        ("latencyMin", int(LATENCY_MIN)),
        ("latencyMax", int(LATENCY_MAX)),
        ("isMedia", Value::Boolean(true)),
        ("audioMode", text("default")),
        ("sr", int(SAMPLE_RATE)),
        ("supportsDynamicStreamID", Value::Boolean(false)),
        ("streamConnectionID", int(i64::from(session_id))),
    ]);
    binary(Value::Dictionary(dict(vec![(
        "streams",
        Value::Array(vec![Value::Dictionary(stream)]),
    )])))
}

/// SETPEERS (P106, P165): the receiver's address first, then the sender's.
pub fn peers(receiver: IpAddr, sender: &[IpAddr]) -> Vec<u8> {
    let mut addresses = vec![text(&receiver.to_canonical().to_string())];
    addresses.extend(
        sender
            .iter()
            .map(|address| text(&address.to_canonical().to_string())),
    );
    binary(Value::Array(addresses))
}

/// The TEARDOWN of the stream: {`streams`: [{`type`: 96, `streamID`: id}]}
/// (P42).
pub fn stream_teardown(stream_id: i64) -> Vec<u8> {
    let stream = dict(vec![
        ("type", int(STREAM_TYPE_REALTIME)),
        ("streamID", int(stream_id)),
    ]);
    binary(Value::Dictionary(dict(vec![(
        "streams",
        Value::Array(vec![Value::Dictionary(stream)]),
    )])))
}

/// The TEARDOWN of the session: an empty dictionary (P42).
pub fn session_teardown() -> Vec<u8> {
    binary(Value::Dictionary(Dictionary::new()))
}

struct Budgeted<'a> {
    cursor: Cursor<&'a [u8]>,
    left: usize,
}

impl Read for Budgeted<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 && !buf.is_empty() {
            return Err(io::Error::other("the plist read budget is spent"));
        }
        let len = buf.len().min(self.left);
        let count = self.cursor.read(&mut buf[..len])?;
        self.left -= count;
        Ok(count)
    }
}

impl Seek for Budgeted<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(position)
    }
}

/// A binary property list body as a dictionary, or `None` for anything
/// else: another format, a body over [`MAX_PLIST_BYTES`], or one whose
/// reading runs past the budget. Only the binary format is read, the one
/// AirPlay 2 bodies use, so the text parsers never see a receiver's bytes.
pub fn read_dictionary(body: &[u8]) -> Option<Dictionary> {
    if body.len() > MAX_PLIST_BYTES || !body.starts_with(BINARY_MAGIC) {
        return None;
    }
    let mut reader = Budgeted {
        cursor: Cursor::new(body),
        left: PLIST_READ_BUDGET,
    };
    Value::from_reader(&mut reader)
        .ok()
        .and_then(Value::into_dictionary)
}

fn unsigned(dictionary: &Dictionary, key: &str) -> Option<u64> {
    dictionary.get(key)?.as_unsigned_integer()
}

fn port(dictionary: &Dictionary, key: &str) -> Option<u16> {
    u16::try_from(unsigned(dictionary, key)?).ok()
}

fn string(dictionary: &Dictionary, key: &str) -> Option<String> {
    dictionary.get(key)?.as_string().map(str::to_owned)
}

/// The values the sender takes from an `/info` answer (P41).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InfoAnswer {
    pub features: Option<Features>,
    /// `statusFlags`, or the TXT `flags` (`sf` for `txtRAOP`) when the
    /// answer carries only the TXT record (P61).
    pub status_flags: Option<u64>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub source_version: Option<String>,
    /// From the second `/info`.
    pub initial_volume: Option<f64>,
}

/// A TXT record in DNS wire form: each string a length byte, then the
/// string. A length that runs past the end drops the rest.
fn wire_strings(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut strings = Vec::new();
    while let Some((&length, rest)) = bytes.split_first() {
        let length = usize::from(length);
        if rest.len() < length {
            break;
        }
        let (string, rest) = rest.split_at(length);
        strings.push(string);
        bytes = rest;
    }
    strings
}

/// Reads an `/info` answer: the dictionary of values, or a dictionary whose
/// useful entry is `txtAirPlay` (or `txtRAOP`), the TXT record parsed as
/// P11 says. Anything unreadable gives an answer without values.
pub fn read_info(body: &[u8]) -> InfoAnswer {
    let Some(dictionary) = read_dictionary(body) else {
        return InfoAnswer::default();
    };
    let mut answer = InfoAnswer {
        features: unsigned(&dictionary, "features").map(Features::from_info),
        status_flags: unsigned(&dictionary, "statusFlags"),
        name: string(&dictionary, "name"),
        model: string(&dictionary, "model"),
        source_version: string(&dictionary, "sourceVersion"),
        initial_volume: dictionary.get("initialVolume").and_then(|value| {
            value
                .as_real()
                .or_else(|| value.as_signed_integer().map(|v| v as f64))
        }),
    };
    if let Some(data) = dictionary.get("txtAirPlay").and_then(Value::as_data) {
        let txt = AirPlayTxt::read(&Txt::parse(&wire_strings(data)));
        answer.features = answer.features.or(txt.features);
        answer.status_flags = answer
            .status_flags
            .or(txt.flags.map(|flags| u64::from(flags.0)));
        answer.model = answer.model.or(txt.model);
        answer.source_version = answer.source_version.or(txt.source_version);
    } else if let Some(data) = dictionary.get("txtRAOP").and_then(Value::as_data) {
        let txt = Txt::parse(&wire_strings(data));
        answer.features = answer.features.or(txt.features("ft"));
        answer.status_flags = answer
            .status_flags
            .or(txt.status_flags("sf").map(|flags| u64::from(flags.0)));
    }
    answer
}

/// What the session SETUP answer gives (P43, P70, P164).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSetupAnswer {
    pub event_port: Option<u16>,
    pub timing_port: Option<u16>,
    /// The receiver's `timingPeerInfo`, recorded under PTP (P164).
    pub timing_peer_info: Option<Dictionary>,
}

pub fn read_session_setup(body: &[u8]) -> SessionSetupAnswer {
    let Some(dictionary) = read_dictionary(body) else {
        return SessionSetupAnswer::default();
    };
    SessionSetupAnswer {
        event_port: port(&dictionary, "eventPort"),
        timing_port: port(&dictionary, "timingPort"),
        timing_peer_info: dictionary
            .get("timingPeerInfo")
            .and_then(Value::as_dictionary)
            .cloned(),
    }
}

/// What the stream SETUP answer gives for the stream (P44).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamSetupAnswer {
    pub data_port: Option<u16>,
    pub control_port: Option<u16>,
    pub stream_id: Option<i64>,
}

pub fn read_stream_setup(body: &[u8]) -> StreamSetupAnswer {
    let stream = read_dictionary(body).and_then(|dictionary| {
        dictionary
            .get("streams")?
            .as_array()?
            .first()?
            .as_dictionary()
            .cloned()
    });
    let Some(stream) = stream else {
        return StreamSetupAnswer::default();
    };
    StreamSetupAnswer {
        data_port: port(&stream, "dataPort"),
        control_port: port(&stream, "controlPort"),
        stream_id: stream.get("streamID").and_then(Value::as_signed_integer),
    }
}
