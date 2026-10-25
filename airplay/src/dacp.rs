//! Remote control from the receiver over DACP (P186 to P195): the sender's
//! DACP ID and Active-Remote values, its `_dacp._tcp` record, and how a
//! receiver's HTTP request is read and answered. No sockets here; the
//! command server's listeners and threads are in [`crate::sender`].

use std::collections::HashSet;
use std::fmt;
use std::time::{Duration, Instant};

use crate::ids::{Entropy, EntropyUnavailable};
use crate::volume::{MAX_DB, MIN_DB, MUTED_DB, Volume};

/// The DNS-SD service type the command server registers (P188).
pub const SERVICE_TYPE: &str = "_dacp._tcp.local.";
/// Open connections the server keeps at most (P194).
pub const MAX_CONNECTIONS: usize = 16;
/// The largest request line and headers the server reads (P194).
pub const MAX_HEAD: usize = 8 * 1024;
/// The largest body the server reads and ignores (P194).
pub const MAX_BODY: usize = 4 * 1024;
/// How long a request's head and body may take to arrive (P194).
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a connection may sit without a request (P189).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the server outlives the last session (P189).
pub const LINGER: Duration = Duration::from_secs(10);
/// Events each speaker may give per second (P194).
pub const EVENTS_PER_SECOND: u32 = 10;
/// The wait after an accept error (P194).
pub const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The sender's DACP ID: 64 random bits, chosen once per run, whose first
/// hex digit is not 0 (P187). `Debug` hides the value, which never goes in
/// a log (P195).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DacpId(u64);

impl DacpId {
    /// An ID from `value`, unless its first hex digit is 0.
    pub fn new(value: u64) -> Option<DacpId> {
        (value >> 60 != 0).then_some(DacpId(value))
    }

    /// Draws a fresh ID. A draw whose first digit is 0 is drawn again; a
    /// source that keeps giving such draws gets its first digit set to 1.
    pub fn generate(entropy: &mut impl Entropy) -> Result<DacpId, EntropyUnavailable> {
        let mut value = 0;
        for _ in 0..8 {
            let mut bytes = [0u8; 8];
            entropy.fill(&mut bytes)?;
            value = u64::from_be_bytes(bytes);
            if let Some(id) = DacpId::new(value) {
                return Ok(id);
            }
        }
        Ok(DacpId(value | (1 << 60)))
    }

    pub fn value(self) -> u64 {
        self.0
    }

    /// The 16 uppercase hex digits of `DACP-ID` and `Client-Instance`.
    pub fn hex(self) -> String {
        format!("{:016X}", self.0)
    }
}

impl fmt::Debug for DacpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DacpId(..)")
    }
}

/// The Active-Remote values issued in one run, none twice (P187).
#[derive(Default)]
pub struct ActiveRemotes {
    issued: HashSet<u32>,
}

impl fmt::Debug for ActiveRemotes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ActiveRemotes({} issued)", self.issued.len())
    }
}

impl ActiveRemotes {
    pub fn new() -> ActiveRemotes {
        ActiveRemotes::default()
    }

    /// A random value not issued before in this run.
    pub fn issue(&mut self, entropy: &mut impl Entropy) -> Result<u32, EntropyUnavailable> {
        // A working source repeats a 32-bit value almost never; one that
        // keeps repeating is broken.
        for _ in 0..64 {
            let mut bytes = [0u8; 4];
            entropy.fill(&mut bytes)?;
            let value = u32::from_be_bytes(bytes);
            if self.issued.insert(value) {
                return Ok(value);
            }
        }
        Err(EntropyUnavailable)
    }

    pub fn was_issued(&self, value: u32) -> bool {
        self.issued.contains(&value)
    }
}

/// The `_dacp._tcp` record of the command server (P188).
#[derive(Clone, PartialEq, Eq)]
pub struct DacpRecord {
    /// `iTunes_Ctrl_<DACP ID>`.
    pub instance: String,
    pub port: u16,
    /// `txtvers`, `Ver`, `DbId` and `OSsi`, in that order.
    pub txt: Vec<(String, String)>,
}

impl DacpRecord {
    pub fn new(id: DacpId, port: u16) -> DacpRecord {
        let hex = id.hex();
        DacpRecord {
            instance: format!("iTunes_Ctrl_{hex}"),
            port,
            txt: vec![
                ("txtvers".to_owned(), "1".to_owned()),
                ("Ver".to_owned(), "131075".to_owned()),
                ("DbId".to_owned(), hex),
                ("OSsi".to_owned(), "0x1F5".to_owned()),
            ],
        }
    }
}

impl fmt::Debug for DacpRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DacpRecord")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// A DNS-SD registration failed; the sessions go on (P188).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertiseError(pub String);

impl fmt::Display for AdvertiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the remote-control service could not be registered: {}",
            self.0
        )
    }
}

impl std::error::Error for AdvertiseError {}

/// Registers and withdraws the command server's `_dacp._tcp` record (P188):
/// [`crate::discovery::mdns::MdnsAdvertiser`] on the network, a fake in
/// tests.
pub trait DacpAdvertiser {
    /// Registers `record`, or replaces the one registered before.
    fn register(&mut self, record: &DacpRecord) -> Result<(), AdvertiseError>;
    /// Withdraws the registered record with a goodbye (RFC 6762 §10.1).
    fn withdraw(&mut self);
    /// The names given up to another device since the last call (RFC 6762
    /// §9). Receivers look the record up by its exact name (P188), so a
    /// renamed record silently stops the speaker's remote buttons.
    fn conflicts(&mut self) -> Vec<NameConflict> {
        Vec::new()
    }
}

/// A name of the advertised record that another device on the network
/// claimed, so the advertiser announces it with a suffix now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameConflict {
    /// The host name rather than the service instance name.
    pub host: bool,
    /// The network interface the conflict was seen on.
    pub interface: String,
}

/// What a receiver's command asks the host to do (P192).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemoteCommand {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    MuteToggle,
    /// `dmcp.device-volume`: the level the receiver asks for.
    DeviceVolume(Volume),
}

/// The request line and headers of one request (P191).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    /// The request target: the path and its query.
    pub target: String,
    /// HTTP/1.0 rather than HTTP/1.1.
    pub http10: bool,
    pub headers: Vec<(String, String)>,
    pub content_length: usize,
}

impl RequestHead {
    /// The first header named `name`, compared without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The connection closes after the answer (P189).
    pub fn wants_close(&self) -> bool {
        let connection = self.header("Connection").map(str::to_ascii_lowercase);
        match connection.as_deref() {
            Some(value) if value.contains("close") => true,
            Some(value) if value.contains("keep-alive") => false,
            _ => self.http10,
        }
    }
}

/// What the bytes read so far hold (P191, P194).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadRead {
    /// The head hasn't ended yet and is within the limit.
    Partial,
    /// A whole head, and how many bytes it took with its blank line.
    Complete {
        head: RequestHead,
        length: usize,
    },
    Malformed,
    /// The head is longer than [`MAX_HEAD`], or its body than [`MAX_BODY`].
    TooLarge,
}

/// Where the head in `buffer` ends: after its blank line, which may end in
/// CRLF or a bare LF.
fn head_end(buffer: &[u8]) -> Option<usize> {
    let mut at = 0;
    while let Some(offset) = buffer[at..].iter().position(|&b| b == b'\n') {
        let line_end = at + offset + 1;
        match buffer.get(line_end) {
            Some(b'\n') => return Some(line_end + 1),
            Some(b'\r') if buffer.get(line_end + 1) == Some(&b'\n') => return Some(line_end + 2),
            _ => at = line_end,
        }
    }
    None
}

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Reads the head at the start of `buffer`.
pub fn read_head(buffer: &[u8]) -> HeadRead {
    let Some(length) = head_end(buffer) else {
        return if buffer.len() > MAX_HEAD {
            HeadRead::TooLarge
        } else {
            HeadRead::Partial
        };
    };
    if length > MAX_HEAD {
        return HeadRead::TooLarge;
    }
    let Ok(text) = std::str::from_utf8(&buffer[..length]) else {
        return HeadRead::Malformed;
    };
    let mut lines = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line));
    let Some(request_line) = lines.next() else {
        return HeadRead::Malformed;
    };
    let parts: Vec<&str> = request_line.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return HeadRead::Malformed;
    };
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        _ => return HeadRead::Malformed,
    };
    if !is_token(method) || target.is_empty() || target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return HeadRead::Malformed;
    }
    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return HeadRead::Malformed;
        };
        if !is_token(name) {
            return HeadRead::Malformed;
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("Transfer-Encoding") {
            return HeadRead::Malformed;
        }
        if name.eq_ignore_ascii_case("Content-Length") {
            if value.is_empty() || value.len() > 9 || !value.bytes().all(|b| b.is_ascii_digit()) {
                return HeadRead::Malformed;
            }
            let parsed: usize = value.parse().unwrap_or(usize::MAX);
            if content_length.is_some_and(|earlier| earlier != parsed) {
                return HeadRead::Malformed;
            }
            content_length = Some(parsed);
        }
        headers.push((name.to_owned(), value.to_owned()));
    }
    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_BODY {
        return HeadRead::TooLarge;
    }
    HeadRead::Complete {
        head: RequestHead {
            method: method.to_owned(),
            target: target.to_owned(),
            http10,
            headers,
            content_length,
        },
        length,
    }
}

/// An `Active-Remote` value: a decimal 32-bit number (P191).
pub fn parse_active_remote(value: &str) -> Option<u32> {
    if value.is_empty() || value.len() > 10 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// How the session an `Active-Remote` names relates to the request (P191).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCheck {
    /// A running session whose speaker has the connection's peer address.
    Match,
    /// No running session, or one whose speaker has another address.
    Mismatch,
}

/// The answer to one request and what it gives the host (P191, P192).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Answer {
    pub status: u16,
    /// The event for the host, before the rate limit (P194).
    pub command: Option<RemoteCommand>,
    /// The session's Active-Remote, when it matched.
    pub remote: Option<u32>,
}

/// Answers a request whose head parsed, checking in the order of P191:
/// method, Active-Remote, path, command.
pub fn answer(head: &RequestHead, session: impl FnOnce(u32) -> SessionCheck) -> Answer {
    let refused = |status| Answer {
        status,
        command: None,
        remote: None,
    };
    if head.method != "GET" {
        return refused(405);
    }
    let Some(remote) = head.header("Active-Remote").and_then(parse_active_remote) else {
        return refused(403);
    };
    if session(remote) == SessionCheck::Mismatch {
        return refused(403);
    }
    let (path, query) = match head.target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (head.target.as_str(), None),
    };
    let Some(name) = command_name(path) else {
        return refused(404);
    };
    match command(name, query) {
        Some(command) => Answer {
            status: 204,
            command,
            remote: Some(remote),
        },
        None => Answer {
            status: 400,
            command: None,
            remote: Some(remote),
        },
    }
}

/// The part of `path` after `/ctrl-int/<n>/`, or `None` outside it.
fn command_name(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/ctrl-int/")?;
    let (number, name) = rest.split_once('/')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(name)
}

/// The event a command under `/ctrl-int/<n>/` gives (P192): `Some(None)`
/// for a command answered 204 without one, `None` for a path that isn't a
/// command, which gets 400.
pub fn command(name: &str, query: Option<&str>) -> Option<Option<RemoteCommand>> {
    let command = match name {
        "playpause" => Some(RemoteCommand::PlayPause),
        "play" | "playresume" => Some(RemoteCommand::Play),
        "pause" | "stop" => Some(RemoteCommand::Pause),
        "nextitem" => Some(RemoteCommand::Next),
        "previtem" => Some(RemoteCommand::Previous),
        "volumeup" => Some(RemoteCommand::VolumeUp),
        "volumedown" => Some(RemoteCommand::VolumeDown),
        "mutetoggle" => Some(RemoteCommand::MuteToggle),
        "beginff" | "beginrew" | "shuffle_songs" => None,
        "setproperty" => device_volume_value(query)
            .and_then(device_volume)
            .map(RemoteCommand::DeviceVolume),
        _ => return None,
    };
    Some(command)
}

/// The last `dmcp.device-volume` value in a query, percent-decoded.
pub fn device_volume_value(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(key)?, percent_decode(value)?))
        })
        .filter(|(key, _)| key == "dmcp.device-volume")
        .map(|(_, value)| value)
        .next_back()
}

/// A `dmcp.device-volume` level (P192): −144 is muted, other numbers are
/// clamped to −30 to 0; text that isn't a decimal number gives `None`.
pub fn device_volume(value: String) -> Option<Volume> {
    let digits = value.strip_prefix(['-', '+']).unwrap_or(&value);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let decimal = !(whole.is_empty() && fraction.is_empty())
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.bytes().all(|b| b.is_ascii_digit());
    if !decimal {
        return None;
    }
    let db: f64 = value.parse().ok()?;
    if !db.is_finite() {
        return None;
    }
    if db == MUTED_DB {
        return Some(Volume::Muted);
    }
    Some(Volume::Db(db.clamp(MIN_DB, MAX_DB)))
}

/// RFC 3986 §2.1 percent-decoding; `None` for a bad escape or text that
/// isn't UTF-8.
pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The command as the log names it (P195): the path without its query,
/// plus the `dmcp.device-volume` value when there is one, in printable
/// ASCII and cut to a readable length.
pub fn log_command(target: &str) -> String {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let mut text = printable(path, 96);
    if let Some(value) = device_volume_value(query) {
        text.push_str(" dmcp.device-volume=");
        text.push_str(&printable(&value, 24));
    }
    text
}

fn printable(text: &str, limit: usize) -> String {
    let mut out: String = text
        .chars()
        .take(limit)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect();
    if text.chars().count() > limit {
        out.push_str("...");
    }
    out
}

/// The reason phrase of a status the server sends.
fn reason(status: u16) -> &'static str {
    match status {
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

/// An answer's bytes: no body, `Content-Length: 0` and `DAAP-Server`
/// (P191).
pub fn response(status: u16, close: bool) -> Vec<u8> {
    let mut text = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: 0\r\nDAAP-Server: {}\r\n",
        reason(status),
        crate::user_agent()
    );
    if status == 405 {
        text.push_str("Allow: GET\r\n");
    }
    if close {
        text.push_str("Connection: close\r\n");
    }
    text.push_str("\r\n");
    text.into_bytes()
}

/// One speaker's events in the current second (P194).
#[derive(Debug, Clone, Copy, Default)]
pub struct RateLimit {
    start: Option<Instant>,
    count: u32,
    over: u32,
}

impl RateLimit {
    /// Whether an event at `now` goes to the host, and how many requests
    /// went over the limit in a second that ended before it.
    pub fn admit(&mut self, now: Instant) -> (bool, Option<u32>) {
        let ended = self.roll(now);
        if self.start.is_none() {
            self.start = Some(now);
        }
        if self.count < EVENTS_PER_SECOND {
            self.count += 1;
            (true, ended)
        } else {
            self.over += 1;
            (false, ended)
        }
    }

    /// Ends the second once it is over: how many requests went over the
    /// limit in it, when any did.
    pub fn roll(&mut self, now: Instant) -> Option<u32> {
        let start = self.start?;
        if now.saturating_duration_since(start) < Duration::from_secs(1) {
            return None;
        }
        let over = std::mem::take(&mut self.over);
        self.start = None;
        self.count = 0;
        (over > 0).then_some(over)
    }
}
