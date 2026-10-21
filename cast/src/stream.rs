//! The HTTP server the device fetches the stream from (C55 to C61, C64).
//!
//! One listener on the local address of the channel, one random path per
//! session, one stream response at a time. A pump thread takes 20 ms blocks
//! from the host's ring at real-time pace and queues them for the response;
//! each connection runs on its own thread.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{AudioReader, FRAME_BYTES, StreamFormat};

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
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
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
    /// A stream `GET` or `HEAD`; `range` when it carried a `Range` header,
    /// which is ignored (C60).
    Fetch {
        from: IpAddr,
        head: bool,
        range: bool,
    },
    /// A request answered with an error status (C57, C58).
    Refused { from: IpAddr, status: u16 },
    /// A stream response ended.
    ResponseEnded(ResponseEnd),
    /// The listener failed; the server stops accepting.
    AcceptFailed(String),
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

/// The state behind the lock that the pump and the responses share.
struct Live {
    /// The response allowed to write; a newer `GET` raises it (C61).
    generation: u64,
    streaming: bool,
    /// Set by a new response: the pump starts again at the newest audio.
    restart: bool,
    queue: BlockQueue,
}

struct Shared {
    format: StreamFormat,
    stream_path: String,
    allowed: Vec<IpAddr>,
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
            format,
            stream_path: format!("/{token}.{}", format.extension()),
            allowed: config.allowed,
            artwork: Mutex::new(None),
            live: Mutex::new(Live {
                generation: 0,
                streaming: false,
                restart: false,
                queue: BlockQueue::new(limit_frames),
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
            self.shared.stream_path
        )
    }

    pub fn format(&self) -> StreamFormat {
        self.shared.format
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
            frames_dropped: live.queue.dropped() + self.shared.ring_dropped.load(Ordering::Relaxed),
            frames_queued: live.queue.frames() as u64,
            streaming: live.streaming,
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
        for (stream, thread) in connections {
            // A response blocked on a device that stopped reading returns
            // once its socket is shut.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            let _ = thread.join();
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
    while !shared.stopped.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let Ok(handle) = stream.try_clone() else {
                    continue;
                };
                let connection = Arc::clone(&shared);
                let spawned = thread::Builder::new()
                    .name("cast-http-conn".to_owned())
                    .spawn(move || serve(stream, peer, connection));
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
    range: bool,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut data = Vec::new();
    let mut buffer = [0u8; 1024];
    let end = loop {
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if data.len() > MAX_REQUEST {
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
    let range = lines.any(|line| {
        line.split(':')
            .next()
            .is_some_and(|name| name.trim().eq_ignore_ascii_case("range"))
    });
    Some(Request {
        method,
        path,
        range,
    })
}

fn simple_response(stream: &mut TcpStream, status: &str, extra: &str) {
    let response =
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n{extra}Connection: close\r\n\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn serve(mut stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let from = peer.ip();
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    if !shared.allowed.contains(&from) {
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
    if request.path == shared.stream_path {
        shared.report(ServerEvent::Fetch {
            from,
            head,
            range: request.range,
        });
        return serve_stream(stream, head, &shared);
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

fn serve_stream(mut stream: TcpStream, head: bool, shared: &Shared) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        shared.format.content_type()
    );
    if stream.write_all(headers.as_bytes()).is_err() || head {
        let _ = stream.flush();
        return;
    }
    shared.fetches.fetch_add(1, Ordering::Relaxed);
    let mine = {
        let mut live = lock(&shared.live);
        live.generation += 1;
        live.streaming = true;
        live.restart = true;
        live.queue.clear();
        live.generation
    };
    shared.wake.notify_all();
    let end = stream_body(&mut stream, mine, shared);
    {
        let mut live = lock(&shared.live);
        if live.generation == mine {
            live.streaming = false;
            live.queue.clear();
        }
    }
    if end != ResponseEnd::ClosedByDevice {
        let _ = stream.write_all(b"0\r\n\r\n");
        let _ = stream.flush();
    }
    shared.report(ServerEvent::ResponseEnded(end));
}

fn stream_body(stream: &mut TcpStream, mine: u64, shared: &Shared) -> ResponseEnd {
    if write_chunk(stream, &shared.format.stream_header()).is_err() {
        return ResponseEnd::ClosedByDevice;
    }
    loop {
        let block = {
            let mut live = lock(&shared.live);
            loop {
                if shared.stopped.load(Ordering::SeqCst) {
                    return ResponseEnd::Stopped;
                }
                if live.generation != mine {
                    return ResponseEnd::Replaced;
                }
                if let Some(block) = live.queue.pop() {
                    break block;
                }
                live = shared
                    .wake
                    .wait_timeout(live, Duration::from_millis(100))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        if write_chunk(stream, &block)
            .and_then(|()| stream.flush())
            .is_err()
        {
            return ResponseEnd::ClosedByDevice;
        }
        shared
            .frames_sent
            .fetch_add((block.len() / FRAME_BYTES) as u64, Ordering::Relaxed);
    }
}

/// Moves 20 ms blocks from the ring to the queue, never ahead of real time
/// (C64). Without a response the ring is drained, so a new response starts
/// at the newest audio (C61).
fn pump(mut reader: AudioReader, shared: Arc<Shared>) {
    let format = shared.format;
    let block_frames = format.block_frames();
    let backlog_frames = format.rate() as usize * RING_BACKLOG.as_millis() as usize / 1000;
    let underrun_blocks = (UNDERRUN.as_millis() / BLOCK.as_millis()) as u64;
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
            live.streaming
        };
        if !streaming {
            reader.skip_to_newest(0);
            thread::sleep(BLOCK);
            continue;
        }
        let due = (start.elapsed().as_millis() / BLOCK.as_millis()) as u64;
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
                for block in blocks {
                    live.queue.push(block);
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
