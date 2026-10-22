//! The HTTP server the device fetches the stream from (C55 to C61, C64).
//!
//! One listener on the local address of the channel, one random path per
//! session, one stream response at a time, or for a group one per source
//! address (C103). A pump thread takes 20 ms blocks from the host's ring at
//! real-time pace and queues them for each response; each connection runs
//! on its own thread.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{AudioReader, Codec, FRAME_BYTES, StreamFormat};
use crate::group::GroupRecords;

/// The queue in front of a reader that doesn't keep up (C64).
pub const QUEUE_LIMIT: Duration = Duration::from_secs(2);
/// One block of audio on the wire (C64).
pub const BLOCK: Duration = Duration::from_millis(20);
/// Audio the ring may hold beyond what real time allows before the oldest
/// is dropped: the host's clock running ahead of the pump's.
const RING_BACKLOG: Duration = Duration::from_millis(100);
/// How far the ring may fall behind real time before silence fills in, as
/// when the host's input stops (C52).
const UNDERRUN: Duration = Duration::from_millis(100);
const ACCEPT_POLL: Duration = Duration::from_millis(20);
/// The whole request head must arrive within this, however it trickles.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections served at once for a single device: the stream, its
/// replacement (C61), artwork; a group serves more (C103).
const MAX_CONNECTIONS: usize = 4;
/// How long a refused connection is kept open to read the rest of its
/// request, so the close is not a reset that would lose the answer.
const REFUSED_LINGER: Duration = Duration::from_secs(1);
/// Refused connections kept lingering at once; the oldest is closed first.
const MAX_REFUSED: usize = 16;
/// How long a stop waits for the connection threads (C87).
const STOP_GRACE: Duration = Duration::from_millis(500);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST: usize = 8 * 1024;
/// Server events kept for a host that doesn't poll.
pub const MAX_SERVER_EVENTS: usize = 64;
/// The largest artwork the device takes (C53).
pub const MAX_ARTWORK: (u32, u32) = (1280, 720);

/// Why the stream server could not start or take artwork.
#[derive(Debug)]
pub enum StreamError {
    /// No listener on the channel's local address (C56).
    Bind(io::Error),
    /// No randomness for the path token (C57); a guessable token is never
    /// used instead.
    Random,
    /// Artwork that is not PNG or JPEG.
    ArtworkFormat,
    /// Artwork larger than [`MAX_ARTWORK`] (C53).
    ArtworkTooLarge { width: u32, height: u32 },
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Bind(error) => write!(f, "could not open the stream server: {error}"),
            StreamError::Random => f.write_str("no randomness for the stream address"),
            StreamError::ArtworkFormat => f.write_str("artwork must be PNG or JPEG"),
            StreamError::ArtworkTooLarge { width, height } => {
                write!(f, "artwork of {width}x{height} is larger than 1280x720")
            }
        }
    }
}

impl std::error::Error for StreamError {}

/// What the server reports for the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerEvent {
    /// A stream `GET` or `HEAD`, with what the device asked for (G6); a
    /// `Range` header is ignored (C60).
    Fetch {
        from: IpAddr,
        head: bool,
        headers: FetchHeaders,
    },
    /// A request answered with an error status: 403 (C58), 404 (C57), 405,
    /// or 503 when the connections a session serves at once are open already.
    Refused { from: IpAddr, status: u16 },
    /// A stream response ended.
    ResponseEnded(ResponseEnd),
    /// The listener failed; the server stops accepting.
    AcceptFailed(String),
}

/// The request headers of a stream fetch, for the log (G6): the names,
/// but only the values that tell a device's behavior apart. Other values,
/// such as a `Referer` holding the stream URL, are never kept. Kept text is
/// percent-decoded, the path token in any case is replaced, and text that
/// still holds a run of [`MAX_HEX_RUN`] hex digits is replaced whole, so a
/// token split across several names never reaches the log either.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchHeaders {
    /// Header names in request order, at most [`MAX_LOGGED_NAMES`].
    pub names: Vec<String>,
    /// Names past [`MAX_LOGGED_NAMES`].
    pub more: usize,
    pub user_agent: Option<String>,
    pub range: Option<String>,
    pub icy_metadata: Option<String>,
}

/// Header names a fetch keeps for the log; a hostile client could send
/// hundreds in one 8 KiB request.
pub const MAX_LOGGED_NAMES: usize = 24;
/// Characters kept of one header name or value.
pub const MAX_LOGGED_TEXT: usize = 96;
/// Hex digits in a row that make a kept name or value `<redacted>`.
pub const MAX_HEX_RUN: usize = 8;

impl FetchHeaders {
    fn parse(lines: impl Iterator<Item = &str>) -> FetchHeaders {
        let mut headers = FetchHeaders::default();
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim();
            let slot = if name.eq_ignore_ascii_case("user-agent") {
                Some(&mut headers.user_agent)
            } else if name.eq_ignore_ascii_case("range") {
                Some(&mut headers.range)
            } else if name.eq_ignore_ascii_case("icy-metadata") {
                Some(&mut headers.icy_metadata)
            } else {
                None
            };
            if let Some(slot) = slot.filter(|slot| slot.is_none()) {
                *slot = Some(value.trim().to_owned());
            }
            if headers.names.len() < MAX_LOGGED_NAMES {
                headers.names.push(name.to_owned());
            } else {
                headers.more += 1;
            }
        }
        headers
    }

    /// Scrubs `token` before capping, so a cut never leaves part of it.
    fn for_log(mut self, token: &str) -> FetchHeaders {
        let scrub = |text: &mut String| {
            *text = replace_ignoring_case(&percent_decoded(text), token, "<token>");
            if has_hex_run(text) {
                *text = "<redacted>".to_owned();
            }
            if text.chars().count() > MAX_LOGGED_TEXT {
                *text = text.chars().take(MAX_LOGGED_TEXT).collect();
            }
        };
        self.names.iter_mut().for_each(scrub);
        [
            &mut self.user_agent,
            &mut self.range,
            &mut self.icy_metadata,
        ]
        .into_iter()
        .flatten()
        .for_each(scrub);
        self
    }
}

/// Decodes `%XX` escapes until none is left, so a token escaped twice is
/// found too; each pass shortens the text, so the loop ends.
fn percent_decoded(text: &str) -> String {
    let mut text = text.to_owned();
    loop {
        let bytes = text.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let digit = |at: usize| {
                bytes
                    .get(at)
                    .and_then(|byte| char::from(*byte).to_digit(16))
            };
            match (bytes[index], digit(index + 1), digit(index + 2)) {
                (b'%', Some(high), Some(low)) => {
                    decoded.push((high * 16 + low) as u8);
                    index += 3;
                }
                (byte, _, _) => {
                    decoded.push(byte);
                    index += 1;
                }
            }
        }
        if decoded.len() == bytes.len() {
            return text;
        }
        text = String::from_utf8_lossy(&decoded).into_owned();
    }
}

fn replace_ignoring_case(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    // ASCII lowercasing keeps every byte offset, so matches index `text` too.
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (at, _) in lower.match_indices(&needle.to_ascii_lowercase()) {
        out.push_str(&text[last..at]);
        out.push_str(with);
        last = at + needle.len();
    }
    out.push_str(&text[last..]);
    out
}

fn has_hex_run(text: &str) -> bool {
    let mut run = 0;
    text.bytes().any(|byte| {
        run = if byte.is_ascii_hexdigit() { run + 1 } else { 0 };
        run >= MAX_HEX_RUN
    })
}

/// How a stream response ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseEnd {
    /// The device closed or reset the connection: normal, not an error
    /// (C61).
    ClosedByDevice,
    /// A newer `GET` took over (C61).
    Replaced,
    /// The server stopped.
    Stopped,
    /// The encoder failed; the device may fetch again.
    EncodeFailed,
}

/// Counters the session reads for its diagnostics (C78, C79, C85).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Stream `GET`s so far.
    pub fetches: u64,
    /// Audio frames written to responses.
    pub frames_sent: u64,
    /// Frames dropped: queue overflow (C64) and ring backlog.
    pub frames_dropped: u64,
    /// Frames waiting in the queue now.
    pub frames_queued: u64,
    /// Whether a stream response is open now.
    pub streaming: bool,
}

/// The blocks waiting for the response, capped at a duration of audio
/// with the oldest dropped first (C64).
#[derive(Debug)]
pub struct BlockQueue {
    blocks: VecDeque<Vec<u8>>,
    frames: usize,
    limit_frames: usize,
    dropped: u64,
}

impl BlockQueue {
    pub fn new(limit_frames: usize) -> BlockQueue {
        BlockQueue {
            blocks: VecDeque::new(),
            frames: 0,
            limit_frames,
            dropped: 0,
        }
    }

    /// Queues one block of whole frames, dropping the oldest beyond the
    /// limit.
    pub fn push(&mut self, block: Vec<u8>) {
        self.frames += block.len() / FRAME_BYTES;
        self.blocks.push_back(block);
        while self.frames > self.limit_frames {
            let Some(oldest) = self.blocks.pop_front() else {
                break;
            };
            let frames = oldest.len() / FRAME_BYTES;
            self.frames -= frames;
            self.dropped += frames as u64;
        }
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let block = self.blocks.pop_front()?;
        self.frames -= block.len() / FRAME_BYTES;
        Some(block)
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Frames dropped on overflow so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.frames = 0;
    }
}

struct Artwork {
    path: String,
    content_type: &'static str,
    bytes: Arc<Vec<u8>>,
}

#[derive(Debug)]
struct Response {
    id: u64,
    from: IpAddr,
    queue: BlockQueue,
}

/// What [`Responses::pop`] found for a response.
#[derive(Debug, PartialEq, Eq)]
pub enum Popped {
    /// The next block; `primary` when the response is the oldest open one,
    /// whose audio the diagnostics count.
    Block {
        block: Vec<u8>,
        primary: bool,
    },
    Empty,
    /// The response was ended by a newer one (C61, C103).
    Ended,
}

/// The open stream responses and their queues. A new `GET` ends the older
/// response it replaces: for a single device every older one (C61); for a
/// group only the one from the same address, and beyond the group's limit
/// (one per known member plus one) the oldest (C103).
#[derive(Debug)]
pub struct Responses {
    open: Vec<Response>,
    next_id: u64,
    queue_frames: usize,
    /// `None` for a single device.
    group_limit: Option<usize>,
    /// Frames the oldest response's queue dropped, over the session (C64).
    dropped: u64,
}

impl Responses {
    /// Each response queues up to `queue_frames` frames (C64).
    pub fn new(queue_frames: usize) -> Responses {
        Responses {
            open: Vec::new(),
            next_id: 1,
            queue_frames,
            group_limit: None,
            dropped: 0,
        }
    }

    /// Serves a group: one response per address, at most `limit` at once.
    pub fn set_group_limit(&mut self, limit: usize) {
        self.group_limit = Some(limit.max(1));
    }

    /// Opens a response for a `GET` from `from` and returns its id, and
    /// whether no other response is left open, so the pump starts again at
    /// the newest audio (C61).
    pub fn open(&mut self, from: IpAddr) -> (u64, bool) {
        match self.group_limit {
            None => self.open.clear(),
            Some(limit) => {
                self.open.retain(|response| response.from != from);
                while self.open.len() >= limit {
                    self.open.remove(0);
                }
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let restart = self.open.is_empty();
        self.open.push(Response {
            id,
            from,
            queue: BlockQueue::new(self.queue_frames),
        });
        (id, restart)
    }

    pub fn close(&mut self, id: u64) {
        self.open.retain(|response| response.id != id);
    }

    pub fn pop(&mut self, id: u64) -> Popped {
        let primary = self.open.first().map(|response| response.id) == Some(id);
        let Some(response) = self.open.iter_mut().find(|response| response.id == id) else {
            return Popped::Ended;
        };
        match response.queue.pop() {
            Some(block) => Popped::Block { block, primary },
            None => Popped::Empty,
        }
    }

    /// Queues a block for every open response.
    pub fn push(&mut self, block: &[u8]) {
        for (index, response) in self.open.iter_mut().enumerate() {
            let before = response.queue.dropped();
            response.queue.push(block.to_vec());
            if index == 0 {
                self.dropped += response.queue.dropped() - before;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// The addresses of the open responses, oldest first.
    pub fn addresses(&self) -> Vec<IpAddr> {
        self.open.iter().map(|response| response.from).collect()
    }

    /// Frames waiting for the oldest response.
    pub fn frames_queued(&self) -> usize {
        self.open
            .first()
            .map_or(0, |response| response.queue.frames())
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Connections served at once: the responses, a replacement and the
    /// artwork.
    fn connection_limit(&self) -> usize {
        self.group_limit
            .map_or(MAX_CONNECTIONS, |limit| MAX_CONNECTIONS.max(limit + 2))
    }
}

/// The state behind the lock that the pump and the responses share.
struct Live {
    responses: Responses,
    /// Set by a response that found none open: the pump starts again at the
    /// newest audio.
    restart: bool,
}

/// The stream's format and the path it is served under; the codec can
/// change once, for the fallback of C84.
struct Route {
    format: StreamFormat,
    path: String,
}

struct Shared {
    /// The pump's rate and block size, the same in every codec.
    pump_format: StreamFormat,
    route: Mutex<Route>,
    allowed: Mutex<Vec<IpAddr>>,
    /// A group's records, read on every request so a handover or a new
    /// member is answered at once (C97, C103).
    group: Mutex<Option<GroupRecords>>,
    artwork: Mutex<Option<Artwork>>,
    live: Mutex<Live>,
    wake: Condvar,
    stopped: AtomicBool,
    fetches: AtomicU64,
    frames_sent: AtomicU64,
    ring_dropped: AtomicU64,
    events: Mutex<VecDeque<ServerEvent>>,
    connections: Mutex<Vec<(TcpStream, JoinHandle<()>)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn report(&self, event: ServerEvent) {
        let mut events = lock(&self.events);
        if events.len() >= MAX_SERVER_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
    }

    fn admits(&self, from: IpAddr) -> bool {
        // A static entry would outlive a handover until the next recover
        // (C97), so a group's records alone decide.
        match lock(&self.group).clone() {
            Some(records) => records.allowed().contains(&from),
            None => lock(&self.allowed).contains(&from),
        }
    }
}

/// What the stream server needs from the session.
#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// The local address of the channel to the device (C55).
    pub local_ip: IpAddr,
    /// The device's known addresses; nobody else gets an answer (C58).
    pub allowed: Vec<IpAddr>,
    pub format: StreamFormat,
}

/// The running stream server of one session.
pub struct StreamServer {
    shared: Arc<Shared>,
    address: SocketAddr,
    token: String,
    accept: Option<JoinHandle<()>>,
    pump: Option<JoinHandle<()>>,
}

impl fmt::Debug for StreamServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamServer")
            .field("address", &self.address)
            .finish()
    }
}

/// A random token of 128 bits as 32 hex digits (C9, C57).
pub fn random_token() -> Result<String, StreamError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| StreamError::Random)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `ip:port` as a URL authority (RFC 3986 §3.2.2, RFC 6874 for a zone).
fn authority(address: SocketAddr) -> String {
    match address {
        SocketAddr::V4(v4) => v4.to_string(),
        SocketAddr::V6(v6) if v6.scope_id() != 0 => {
            format!("[{}%25{}]:{}", v6.ip(), v6.scope_id(), v6.port())
        }
        SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()),
    }
}

impl StreamServer {
    /// Opens the listener on `config.local_ip` with a port the system picks
    /// (C56) and starts the pump on `reader`.
    pub fn start(config: StreamConfig, reader: AudioReader) -> Result<StreamServer, StreamError> {
        let token = random_token()?;
        let listener = TcpListener::bind((config.local_ip, 0)).map_err(StreamError::Bind)?;
        let address = listener.local_addr().map_err(StreamError::Bind)?;
        listener.set_nonblocking(true).map_err(StreamError::Bind)?;
        let format = config.format;
        let limit_frames = format.rate() as usize * QUEUE_LIMIT.as_millis() as usize / 1000;
        let shared = Arc::new(Shared {
            pump_format: format,
            route: Mutex::new(Route {
                format,
                path: format!("/{token}.{}", format.extension()),
            }),
            allowed: Mutex::new(config.allowed),
            group: Mutex::new(None),
            artwork: Mutex::new(None),
            live: Mutex::new(Live {
                responses: Responses::new(limit_frames),
                restart: false,
            }),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            fetches: AtomicU64::new(0),
            frames_sent: AtomicU64::new(0),
            ring_dropped: AtomicU64::new(0),
            events: Mutex::new(VecDeque::new()),
            connections: Mutex::new(Vec::new()),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("cast-http".to_owned())
                .spawn(move || accept_loop(listener, shared))
                .map_err(StreamError::Bind)?
        };
        let pump = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("cast-pump".to_owned())
                .spawn(move || pump(reader, shared))
                .map_err(StreamError::Bind)?
        };
        Ok(StreamServer {
            shared,
            address,
            token,
            accept: Some(accept),
            pump: Some(pump),
        })
    }

    /// The listener's address.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The session's random token (C57).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The stream URL for `contentId` (C47, C55).
    pub fn url(&self) -> String {
        format!(
            "http://{}{}",
            authority(self.address),
            lock(&self.shared.route).path
        )
    }

    pub fn format(&self) -> StreamFormat {
        lock(&self.shared.route).format
    }

    /// Replaces the addresses that get an answer.
    pub fn set_allowed(&self, allowed: Vec<IpAddr>) {
        *lock(&self.shared.allowed) = allowed;
    }

    /// Answers whatever `records` hold at the time of each request, a
    /// group's current addresses and its members' (C103), in place of the
    /// static list.
    pub fn follow_group(&self, records: GroupRecords) {
        *lock(&self.shared.group) = Some(records);
    }

    /// Serves a group: one response per source address, at most `limit`
    /// at once (C103).
    pub fn set_group_limit(&self, limit: usize) {
        lock(&self.shared.live).responses.set_group_limit(limit);
    }

    /// Serves the stream in `codec` from now on, under the same token with
    /// that codec's extension, and returns the new URL (C84). The old URL
    /// gets 404; a response already open runs on until a new `GET`
    /// replaces it (C61).
    pub fn set_codec(&self, codec: Codec) -> String {
        {
            let mut route = lock(&self.shared.route);
            route.format = route.format.with_codec(codec);
            route.path = format!("/{}.{}", self.token, codec.name());
        }
        self.url()
    }

    /// Serves `bytes` as the track artwork under a new random path and
    /// returns its URL (C53). PNG and JPEG up to 1280 by 720 pixels.
    pub fn set_artwork(&self, bytes: Vec<u8>) -> Result<String, StreamError> {
        let (content_type, width, height) = image_size(&bytes).ok_or(StreamError::ArtworkFormat)?;
        if width > MAX_ARTWORK.0 || height > MAX_ARTWORK.1 {
            return Err(StreamError::ArtworkTooLarge { width, height });
        }
        let extension = if content_type == "image/png" {
            "png"
        } else {
            "jpg"
        };
        let path = format!("/{}.{extension}", random_token()?);
        let url = format!("http://{}{path}", authority(self.address));
        *lock(&self.shared.artwork) = Some(Artwork {
            path,
            content_type,
            bytes: Arc::new(bytes),
        });
        Ok(url)
    }

    pub fn stats(&self) -> StreamStats {
        let live = lock(&self.shared.live);
        StreamStats {
            fetches: self.shared.fetches.load(Ordering::Relaxed),
            frames_sent: self.shared.frames_sent.load(Ordering::Relaxed),
            frames_dropped: live.responses.dropped()
                + self.shared.ring_dropped.load(Ordering::Relaxed),
            frames_queued: live.responses.frames_queued() as u64,
            streaming: !live.responses.is_empty(),
        }
    }

    pub fn poll_event(&self) -> Option<ServerEvent> {
        lock(&self.shared.events).pop_front()
    }

    /// Ends the stream response and closes the listener (C87); the token
    /// stops working (C57).
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// [`StreamServer::stop`] for an owner that keeps the server.
    pub fn stop_in_place(&mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        let connections = std::mem::take(&mut *lock(&self.shared.connections));
        for (stream, _) in &connections {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        // A send blocked on a device that stopped reading may not return
        // when its socket is shut (WinSock); it ends within its write
        // timeout on its own, and the teardown does not wait for it.
        let until = Instant::now() + STOP_GRACE;
        for (_, thread) in connections {
            while !thread.is_finished() && Instant::now() < until {
                thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for StreamServer {
    fn drop(&mut self) {
        if self.accept.is_some() || self.pump.is_some() {
            self.shutdown();
        }
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    let mut refused: VecDeque<(TcpStream, Instant)> = VecDeque::new();
    while !shared.stopped.load(Ordering::SeqCst) {
        drain_refused(&mut refused);
        match listener.accept() {
            Ok((stream, peer)) => {
                // Accepted sockets inherit the listener's non-blocking mode
                // on WinSock and BSD.
                let _ = stream.set_nonblocking(false);
                if !shared.admits(peer.ip()) {
                    shared.report(ServerEvent::Refused {
                        from: peer.ip(),
                        status: 403,
                    });
                    linger(&mut refused, refuse(stream, "403 Forbidden"));
                    continue;
                }
                let limit = lock(&shared.live).responses.connection_limit();
                let mut connections = lock(&shared.connections);
                connections.retain(|(_, thread)| !thread.is_finished());
                if connections.len() >= limit {
                    drop(connections);
                    shared.report(ServerEvent::Refused {
                        from: peer.ip(),
                        status: 503,
                    });
                    linger(&mut refused, refuse(stream, "503 Service Unavailable"));
                    continue;
                }
                drop(connections);
                let Ok(handle) = stream.try_clone() else {
                    continue;
                };
                let connection = Arc::clone(&shared);
                let spawned = thread::Builder::new()
                    .name("cast-http-conn".to_owned())
                    .spawn(move || {
                        let closer = stream.try_clone();
                        serve(stream, peer, connection);
                        // The clone kept for the teardown would hold the
                        // connection open after the response is done.
                        if let Ok(closer) = closer {
                            let _ = closer.shutdown(std::net::Shutdown::Both);
                        }
                    });
                if let Ok(thread) = spawned {
                    let mut connections = lock(&shared.connections);
                    connections.retain(|(_, thread)| !thread.is_finished());
                    connections.push((handle, thread));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                shared.report(ServerEvent::AcceptFailed(error.to_string()));
                thread::sleep(ACCEPT_POLL);
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    headers: FetchHeaders,
}

/// An error status from the accept thread, without a thread of its own.
/// The connection stays open for [`REFUSED_LINGER`] while the accept loop
/// reads what the client still sends: closing with unread data would reset
/// the connection, and a reset can discard the answer before the client
/// reads it.
fn refuse(mut stream: TcpStream, status: &str) -> Option<TcpStream> {
    stream.set_nonblocking(true).ok()?;
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.write(response.as_bytes());
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Some(stream)
}

fn linger(refused: &mut VecDeque<(TcpStream, Instant)>, stream: Option<TcpStream>) {
    let Some(stream) = stream else {
        return;
    };
    if refused.len() >= MAX_REFUSED {
        refused.pop_front();
    }
    refused.push_back((stream, Instant::now() + REFUSED_LINGER));
}

/// Reads and discards what refused clients sent; closes each once the
/// client closed its side or its time is up.
fn drain_refused(refused: &mut VecDeque<(TcpStream, Instant)>) {
    let now = Instant::now();
    let mut buffer = [0u8; 1024];
    refused.retain_mut(|(stream, until)| {
        if now >= *until {
            return false;
        }
        for _ in 0..8 {
            match stream.read(&mut buffer) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        true
    });
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut data = Vec::new();
    let mut buffer = [0u8; 1024];
    let end = loop {
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if data.len() > MAX_REQUEST {
            return None;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return None;
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return None,
            Ok(count) => data.extend_from_slice(&buffer[..count]),
        }
    };
    let text = String::from_utf8_lossy(&data[..end]);
    let mut lines = text.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let method = parts.next()?.to_owned();
    let target = parts.next()?;
    let path = target.split('?').next().unwrap_or(target).to_owned();
    Some(Request {
        method,
        path,
        headers: FetchHeaders::parse(lines),
    })
}

fn simple_response(stream: &mut TcpStream, status: &str, extra: &str) {
    let response =
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n{extra}Connection: close\r\n\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn serve(mut stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let from = peer.ip();
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    if !shared.admits(from) {
        shared.report(ServerEvent::Refused { from, status: 403 });
        return simple_response(&mut stream, "403 Forbidden", "");
    }
    let head = request.method == "HEAD";
    if !head && request.method != "GET" {
        shared.report(ServerEvent::Refused { from, status: 405 });
        return simple_response(
            &mut stream,
            "405 Method Not Allowed",
            "Allow: GET, HEAD\r\n",
        );
    }
    if shared.stopped.load(Ordering::SeqCst) {
        return simple_response(&mut stream, "404 Not Found", "");
    }
    let stream_format = {
        let route = lock(&shared.route);
        (request.path == route.path).then_some(route.format)
    };
    if let Some(format) = stream_format {
        let token = request.path.trim_start_matches('/');
        let token = token.split('.').next().unwrap_or_default();
        shared.report(ServerEvent::Fetch {
            from,
            head,
            headers: request.headers.for_log(token),
        });
        return serve_stream(stream, from, head, format, &shared);
    }
    let artwork = lock(&shared.artwork)
        .as_ref()
        .filter(|artwork| artwork.path == request.path)
        .map(|artwork| (artwork.content_type, Arc::clone(&artwork.bytes)));
    match artwork {
        Some((content_type, bytes)) => {
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            if !head {
                let _ = stream.write_all(&bytes);
            }
            let _ = stream.flush();
        }
        None => {
            shared.report(ServerEvent::Refused { from, status: 404 });
            simple_response(&mut stream, "404 Not Found", "");
        }
    }
}

fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    stream.write_all(format!("{:X}\r\n", data.len()).as_bytes())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")
}

fn serve_stream(
    mut stream: TcpStream,
    from: IpAddr,
    head: bool,
    format: StreamFormat,
    shared: &Shared,
) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        format.content_type()
    );
    if stream.write_all(headers.as_bytes()).is_err() || head {
        let _ = stream.flush();
        return;
    }
    shared.fetches.fetch_add(1, Ordering::Relaxed);
    let mine = {
        let mut live = lock(&shared.live);
        let (mine, restart) = live.responses.open(from);
        live.restart |= restart;
        mine
    };
    shared.wake.notify_all();
    let end = stream_body(&mut stream, mine, format, shared);
    lock(&shared.live).responses.close(mine);
    if end != ResponseEnd::ClosedByDevice {
        let _ = stream.write_all(b"0\r\n\r\n");
        let _ = stream.flush();
    }
    shared.report(ServerEvent::ResponseEnded(end));
}

fn stream_body(
    stream: &mut TcpStream,
    mine: u64,
    format: StreamFormat,
    shared: &Shared,
) -> ResponseEnd {
    let Ok(mut encoder) = format.encoder() else {
        return ResponseEnd::EncodeFailed;
    };
    if write_chunk(stream, &format.stream_header()).is_err() {
        return ResponseEnd::ClosedByDevice;
    }
    loop {
        let (block, primary) = {
            let mut live = lock(&shared.live);
            loop {
                if shared.stopped.load(Ordering::SeqCst) {
                    return ResponseEnd::Stopped;
                }
                match live.responses.pop(mine) {
                    Popped::Ended => return ResponseEnd::Replaced,
                    Popped::Block { block, primary } => break (block, primary),
                    Popped::Empty => {}
                }
                live = shared
                    .wake
                    .wait_timeout(live, Duration::from_millis(100))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        let frames = (block.len() / FRAME_BYTES) as u64;
        let Ok(bytes) = encoder.encode(block) else {
            return ResponseEnd::EncodeFailed;
        };
        if write_chunk(stream, &bytes)
            .and_then(|()| stream.flush())
            .is_err()
        {
            return ResponseEnd::ClosedByDevice;
        }
        if primary {
            shared.frames_sent.fetch_add(frames, Ordering::Relaxed);
        }
    }
}

/// Moves 20 ms blocks from the ring to the queue, never ahead of real time
/// (C64). Without a response the ring is drained, so a new response starts
/// at the newest audio (C61).
fn pump(mut reader: AudioReader, shared: Arc<Shared>) {
    let format = shared.pump_format;
    let block_frames = format.block_frames();
    let backlog_frames = format.rate() as usize * RING_BACKLOG.as_millis() as usize / 1000;
    let underrun_blocks = (UNDERRUN.as_millis() / BLOCK.as_millis()) as u64;
    let queue_blocks = (QUEUE_LIMIT.as_millis() / BLOCK.as_millis()) as u64;
    let mut start = Instant::now();
    let mut emitted: u64 = 0;
    while !shared.stopped.load(Ordering::SeqCst) {
        let streaming = {
            let mut live = lock(&shared.live);
            if live.restart {
                live.restart = false;
                start = Instant::now();
                emitted = 0;
                reader.skip_to_newest(0);
            }
            !live.responses.is_empty()
        };
        if !streaming {
            reader.skip_to_newest(0);
            thread::sleep(BLOCK);
            continue;
        }
        let mut due = (start.elapsed().as_millis() / BLOCK.as_millis()) as u64;
        if due - emitted > queue_blocks {
            // After a stall longer than the queue (a sleeping computer), the
            // missed time is gone; silence for all of it would only be
            // dropped again.
            start = Instant::now();
            emitted = 0;
            due = 0;
        }
        let mut blocks = Vec::new();
        while emitted < due {
            if reader.available() >= block_frames {
                let mut block = Vec::with_capacity(block_frames * FRAME_BYTES);
                reader.read_into(&mut block, block_frames);
                blocks.push(block);
            } else if due - emitted > underrun_blocks {
                blocks.push(vec![0; block_frames * FRAME_BYTES]);
            } else {
                break;
            }
            emitted += 1;
        }
        let excess = reader.available().saturating_sub(backlog_frames);
        if excess > 0 {
            let dropped = reader.skip_to_newest(block_frames);
            shared
                .ring_dropped
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        if !blocks.is_empty() {
            let mut live = lock(&shared.live);
            if !live.restart {
                for block in &blocks {
                    live.responses.push(block);
                }
            }
            drop(live);
            shared.wake.notify_all();
        }
        let next = start + BLOCK * (emitted as u32 + 1);
        thread::sleep(
            next.saturating_duration_since(Instant::now())
                .min(BLOCK)
                .max(Duration::from_millis(2)),
        );
    }
}

/// The content type and pixel size of a PNG or JPEG image.
pub fn image_size(bytes: &[u8]) -> Option<(&'static str, u32, u32)> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.starts_with(PNG) {
        if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
            return None;
        }
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some(("image/png", width, height));
    }
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            return None;
        }
        let marker = bytes[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        let length = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
        let is_frame = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_frame {
            let segment = bytes.get(at + 4..at + 9)?;
            let height = u16::from_be_bytes([segment[1], segment[2]]) as u32;
            let width = u16::from_be_bytes([segment[3], segment[4]]) as u32;
            return Some(("image/jpeg", width, height));
        }
        if length < 2 {
            return None;
        }
        at += 2 + length;
    }
    None
}
