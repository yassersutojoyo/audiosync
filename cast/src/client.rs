//! The channel over a real TLS connection (C19 to C21), driven on the
//! caller's thread with blocking sockets and short read timeouts.
//!
//! A read times out only in a peek, then reads queued bytes ([`PeekedTcp`]):
//! a cancel can't drop bytes a peek never took (Windows `SO_RCVTIMEO`).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use native_tls::{HandshakeError, Protocol, TlsConnector, TlsStream};
use sha2::{Digest, Sha256};

use crate::auth::{self, Outcome};
use crate::channel::{Channel, Event, STOP_STEP_TIMEOUT};
use crate::device_keys::{Decision, DeviceKeyBook, Verdict, auth_wait, decide};
use crate::discovery::{CastDevice, DeviceId, DeviceKind};
use crate::error::{ConnectError, EndReason, LaunchError};
use crate::framing::{FrameReader, encode_frame};
use crate::group::MultizoneMessage;
use crate::ids::RequestIds;
use crate::media::{LoadRequest, MediaError, VolumeError};
use crate::status::{AppSession, DeviceVolume, ReceiverStatus};

/// TCP connect and TLS handshake limit per address.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest one frame or TLS close waits on a device that stopped
/// reading: short enough that a quit's teardown fits its margin (C88).
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
/// The longest a read waits before timers run again.
const READ_SLICE: Duration = Duration::from_millis(50);
/// How often a TLS handshake waiting on the device looks again.
const HANDSHAKE_POLL: Duration = Duration::from_millis(5);
/// Events kept for a host that doesn't poll: beyond this, the oldest
/// status and drop reports go first, then group messages and stall reports
/// (C110); launch and load results and the end are kept.
pub const MAX_EVENTS: usize = 64;
/// The most addresses one connect tries (C109).
pub const MAX_CONNECT_ADDRESSES: usize = 8;
/// The longest teardown at host quit (C88).
pub const QUIT_BUDGET: Duration = Duration::from_secs(3);

/// A TCP connect that stops waiting once `selected` is cleared. The
/// connect itself can't be cut short, so it runs on a thread of its own,
/// which finishes within [`CONNECT_TIMEOUT`] and drops what it got.
fn tcp_connect(address: SocketAddr, selected: &AtomicBool) -> Result<TcpStream, ConnectError> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("cast-connect".to_owned())
        .spawn(move || {
            let _ = sender.send(TcpStream::connect_timeout(&address, CONNECT_TIMEOUT));
        })
        .map_err(|error| ConnectError::Tcp { address, error })?;
    loop {
        match receiver.recv_timeout(READ_SLICE) {
            Ok(result) => return result.map_err(|error| ConnectError::Tcp { address, error }),
            Err(RecvTimeoutError::Timeout) if !selected.load(Ordering::SeqCst) => {
                return Err(ConnectError::Ended(EndReason::Stopped));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let error = io::Error::other("the connect thread ended");
                return Err(ConnectError::Tcp { address, error });
            }
        }
    }
}

/// The SHA-256 of a device's DER certificate (C21).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(der: &[u8]) -> Fingerprint {
        Fingerprint(Sha256::digest(der).into())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// What a fingerprint seen at connect means against the one recorded
/// before (C21). None of them blocks the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerprintChange {
    First,
    Same,
    /// Logged, never refused.
    Changed {
        before: Fingerprint,
    },
}

/// What hears the book's first failed write.
type SaveErrorReport = Box<dyn FnOnce(io::Error) + Send>;
/// How the host replaces the book's file with new bytes.
type BookWriter = Box<dyn FnMut(&Path, &[u8]) -> io::Result<()> + Send>;

/// Where the book is kept across launches, and what writes it there.
struct BookFile {
    path: PathBuf,
    write: BookWriter,
    report_save_error: Option<SaveErrorReport>,
}

impl BookFile {
    fn save(&mut self, text: &str) {
        // Diagnostics only: a book that can't be written stays in memory.
        if let Err(error) = (self.write)(&self.path, text.as_bytes())
            && let Some(report) = self.report_save_error.take()
        {
            report(error);
        }
    }
}

/// Puts the file back in the book when its writer panics.
struct Saving<'a> {
    book: &'a Mutex<FingerprintBook>,
    file: Option<BookFile>,
}

impl Drop for Saving<'_> {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let mut book = self.book.lock().unwrap_or_else(PoisonError::into_inner);
            book.file = Some(file);
            book.saving = false;
        }
    }
}

/// The fingerprints seen per device, for diagnostics only (C21).
#[derive(Default)]
pub struct FingerprintBook {
    seen: HashMap<DeviceId, Fingerprint>,
    /// `None` keeps the book in memory, or a record is writing it.
    file: Option<BookFile>,
    /// Set while a record writes the file outside the book's lock.
    saving: bool,
    /// The newest text, which the record writing the file writes next.
    unsaved: Option<String>,
}

impl fmt::Debug for FingerprintBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FingerprintBook")
            .field("seen", &self.seen)
            .field("file", &self.file.as_ref().map(|file| &file.path))
            .finish_non_exhaustive()
    }
}

impl FingerprintBook {
    pub fn new() -> FingerprintBook {
        FingerprintBook::default()
    }

    /// The book kept at `path`, taken through `read`, which every new or
    /// changed fingerprint rewrites whole through `write`. A file that is
    /// there but can't be read starts an empty book, with the error for the
    /// host's log; a missing one starts it with none.
    pub fn open(
        path: PathBuf,
        read: impl FnOnce(&Path) -> io::Result<Vec<u8>>,
        write: impl FnMut(&Path, &[u8]) -> io::Result<()> + Send + 'static,
    ) -> (FingerprintBook, Option<io::Error>) {
        let parsed = read(&path).and_then(|bytes| {
            String::from_utf8(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
                .and_then(|text| parse_book(&text))
        });
        let (seen, error) = match parsed {
            Ok(seen) => (seen, None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (HashMap::new(), None),
            Err(error) => (HashMap::new(), Some(error)),
        };
        let book = FingerprintBook {
            seen,
            file: Some(BookFile {
                path,
                write: Box::new(write),
                report_save_error: None,
            }),
            saving: false,
            unsaved: None,
        };
        (book, error)
    }

    /// Hands the first write that fails to `report`, for the host's log;
    /// later ones go unreported, so a file that stays locked logs once.
    pub fn on_save_error(mut self, report: impl FnOnce(io::Error) + Send + 'static) -> Self {
        if let Some(file) = &mut self.file {
            file.report_save_error = Some(Box::new(report));
        }
        self
    }

    pub fn record(&mut self, id: DeviceId, fingerprint: Fingerprint) -> FingerprintChange {
        let (change, file) = self.note(id, fingerprint);
        if let Some(file) = file {
            self.file = Some(file);
            self.saving = false;
            if let (Some(text), Some(file)) = (self.unsaved.take(), &mut self.file) {
                file.save(&text);
            }
        }
        change
    }

    /// [`FingerprintBook::record`] on a book the sessions share: the file
    /// is written with the lock dropped, by one record at a time, which
    /// writes the newest text last.
    pub fn record_in(
        book: &Mutex<FingerprintBook>,
        id: DeviceId,
        fingerprint: Fingerprint,
    ) -> FingerprintChange {
        let lock = || book.lock().unwrap_or_else(PoisonError::into_inner);
        let mut held = lock();
        let (change, file) = held.note(id, fingerprint);
        if let Some(file) = file {
            let mut saving = Saving {
                book,
                file: Some(file),
            };
            while let Some(text) = held.unsaved.take() {
                drop(held);
                if let Some(file) = &mut saving.file {
                    file.save(&text);
                }
                held = lock();
            }
            held.file = saving.file.take();
            held.saving = false;
        }
        change
    }

    /// Notes `fingerprint` and the text to write for it; the file when no
    /// other record is writing it.
    fn note(
        &mut self,
        id: DeviceId,
        fingerprint: Fingerprint,
    ) -> (FingerprintChange, Option<BookFile>) {
        let change = match self.seen.insert(id, fingerprint) {
            None => FingerprintChange::First,
            Some(before) if before == fingerprint => FingerprintChange::Same,
            Some(before) => FingerprintChange::Changed { before },
        };
        if change == FingerprintChange::Same || (self.file.is_none() && !self.saving) {
            return (change, None);
        }
        // One `<id> <fingerprint>` line per device, as they are logged.
        let text: String = (self.seen.iter())
            .map(|(id, fingerprint)| format!("{id} {fingerprint}\n"))
            .collect();
        self.unsaved = Some(text);
        self.saving = true;
        (change, self.file.take())
    }
}

fn parse_book(text: &str) -> io::Result<HashMap<DeviceId, Fingerprint>> {
    // `lines` also takes CRLF endings, as an editor on Windows may save.
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (id, hex) = line.split_once(' ').unwrap_or((line, ""));
            let bytes: Option<Vec<u8>> = hex
                .split(':')
                .map(|pair| u8::from_str_radix(pair, 16).ok())
                .collect();
            let fingerprint = bytes.and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
            match (DeviceId::parse(id), fingerprint) {
                (Some(id), Some(fingerprint)) => Ok((id, Fingerprint(fingerprint))),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "not a device and a fingerprint: {:?}",
                        line.chars().take(80).collect::<String>()
                    ),
                )),
            }
        })
        .collect()
}

/// The TLS client configuration of C20 and C21: TLS 1.2 or later, no
/// certificate or host name check, no server name sent.
pub fn tls_connector() -> Result<TlsConnector, native_tls::Error> {
    TlsConnector::builder()
        .min_protocol_version(Some(Protocol::Tlsv12))
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        // The device has no host name to send, and C9 keeps names off the
        // wire.
        .use_sni(false)
        .build()
}

/// The TCP connection under the TLS stream: a read first peeks within the
/// socket's read timeout, so a timeout only ever ends a peek.
#[derive(Debug)]
struct PeekedTcp {
    tcp: TcpStream,
    /// When the write in progress gives up. The socket's timeout counts per
    /// send, and Linux ends a blocked send with the part it got out, so one
    /// TLS record could otherwise wait several timeouts in a row.
    write_until: Option<Instant>,
}

impl PeekedTcp {
    fn new(tcp: TcpStream) -> PeekedTcp {
        PeekedTcp {
            tcp,
            write_until: None,
        }
    }
}

impl Read for PeekedTcp {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.tcp.peek(buf).map_err(peek_error)? == 0 {
            return Ok(0);
        }
        self.tcp.read(buf)
    }
}

/// Windows can end a blocking peek with ERROR_IO_PENDING when its timeout
/// races arriving bytes; the peek took nothing, so it counts as a timeout.
fn peek_error(error: io::Error) -> io::Error {
    const ERROR_IO_PENDING: i32 = 997;
    if cfg!(windows) && error.raw_os_error() == Some(ERROR_IO_PENDING) {
        return io::Error::from(io::ErrorKind::TimedOut);
    }
    error
}

impl Write for PeekedTcp {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(until) = self.write_until {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            self.tcp.set_write_timeout(Some(left))?;
        }
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

/// Where a connect judges the device's answer to the challenge (C129).
#[derive(Clone, Debug)]
pub struct DeviceAuth {
    /// The device's entry in the book.
    pub key: String,
    /// A group is never pinned.
    pub group: bool,
    pub book: Arc<Mutex<DeviceKeyBook>>,
    /// The roots the reply's chain must reach: the embedded ones (C126).
    roots: Vec<Vec<u8>>,
}

impl DeviceAuth {
    pub fn new(key: String, group: bool, book: Arc<Mutex<DeviceKeyBook>>) -> DeviceAuth {
        DeviceAuth {
            key,
            group,
            book,
            roots: auth::ROOTS.iter().map(|root| root.to_vec()).collect(),
        }
    }

    /// Trusts `roots` in place of the embedded ones, for a test's own chain.
    #[doc(hidden)]
    pub fn with_roots(mut self, roots: Vec<Vec<u8>>) -> DeviceAuth {
        self.roots = roots;
        self
    }
}

/// Sends the challenge and waits for the reply, dropping anything else
/// that arrives meanwhile (C119); the outcome of C122.
fn authenticate(
    stream: &mut TlsStream<PeekedTcp>,
    reader: &mut FrameReader,
    peer: Option<&[u8]>,
    auth: Option<&DeviceAuth>,
    selected: &AtomicBool,
) -> Result<Outcome, ConnectError> {
    let entry = auth.and_then(|auth| {
        let book = auth.book.lock().unwrap_or_else(PoisonError::into_inner);
        book.entry(&auth.key)
    });
    let wait = auth_wait(entry);
    let nonce = auth::new_nonce()
        .map_err(|error| ConnectError::Ended(EndReason::Io(format!("no random nonce: {error}"))))?;
    let frame = encode_frame(&auth::challenge(&nonce))
        .map_err(|fault| ConnectError::Ended(EndReason::Fault(fault)))?;
    let io_error = |error: io::Error| ConnectError::Ended(EndReason::Io(error.to_string()));
    stream.write_all(&frame).map_err(io_error)?;
    stream.flush().map_err(io_error)?;
    let deadline = Instant::now() + wait;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        loop {
            match reader.next_message() {
                Ok(Some(message)) if auth::is_reply(&message) => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |since| since.as_secs() as i64);
                    let roots: Vec<&[u8]> = match auth {
                        Some(auth) => auth.roots.iter().map(Vec::as_slice).collect(),
                        None => auth::ROOTS.to_vec(),
                    };
                    return Ok(auth::judge(&message, &nonce, peer, now, &roots));
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(fault) => return Err(ConnectError::Ended(EndReason::Fault(fault))),
            }
        }
        if !selected.load(Ordering::SeqCst) {
            return Err(ConnectError::Ended(EndReason::Stopped));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(Outcome::NoAnswer(wait));
        }
        let slice = READ_SLICE.min(deadline - now).max(Duration::from_millis(1));
        let _ = stream.get_ref().tcp.set_read_timeout(Some(slice));
        match stream.read(&mut buffer) {
            Ok(0) => return Err(ConnectError::Ended(EndReason::ClosedByDevice)),
            Ok(count) => reader.push(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(io_error(error)),
        }
    }
}

/// An open channel to one device.
pub struct CastClient {
    stream: TlsStream<PeekedTcp>,
    reader: FrameReader,
    channel: Channel,
    /// Each event with its sequence number, so a call waits only for
    /// events reported after it started.
    events: VecDeque<(u64, Event)>,
    next_event: u64,
    address: SocketAddr,
    local_address: SocketAddr,
    fingerprint: Option<Fingerprint>,
    /// The judged challenge of this connect, until the session takes it.
    auth: Option<Decision>,
    buffer: Vec<u8>,
    /// Kept apart from the queue, which the host may have drained.
    ended: Option<EndReason>,
    /// Cleared when the user deselects the device; the launch and load
    /// waits give up with it (C80).
    selected: Option<Arc<AtomicBool>>,
    /// The end of a running teardown's budget, which every write keeps to.
    teardown_until: Option<Instant>,
}

impl fmt::Debug for CastClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CastClient")
            .field("address", &self.address)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

impl CastClient {
    /// Connects to a discovered device on its announced addresses and port
    /// (C15); a group the same way, to its leader (C93), and then asks for
    /// its members (C94).
    pub fn connect_device(device: &CastDevice) -> Result<CastClient, ConnectError> {
        let mut client = CastClient::connect(&device.socket_addresses(), RequestIds::random())?;
        if device.kind == DeviceKind::Group {
            client.request_members();
        }
        Ok(client)
    }

    /// Sends the multizone `GET_STATUS` of a group session (C94).
    pub fn request_members(&mut self) {
        self.channel.request_members();
        self.flush();
    }

    /// Tries each address in turn (IPv4 first, C16), at most
    /// [`MAX_CONNECT_ADDRESSES`] of them (C109), and returns once the first
    /// `RECEIVER_STATUS` arrived (C33).
    pub fn connect(addresses: &[SocketAddr], ids: RequestIds) -> Result<CastClient, ConnectError> {
        CastClient::connect_while(addresses, ids, &AtomicBool::new(true), None)
    }

    /// [`CastClient::connect`] that gives up within a read slice once
    /// `selected` is cleared, with [`EndReason::Stopped`], so a quit never
    /// waits out the timeouts of every address (C88). `known` is the
    /// volume a reconnect carries over (C37).
    pub fn connect_while(
        addresses: &[SocketAddr],
        ids: RequestIds,
        selected: &AtomicBool,
        known: Option<&DeviceVolume>,
    ) -> Result<CastClient, ConnectError> {
        CastClient::connect_authed(addresses, ids, selected, known, None)
    }

    /// [`CastClient::connect_while`] that judges the challenge against
    /// `auth`'s entry (C129); without one, as a device with no entry, and
    /// nothing is kept.
    pub fn connect_authed(
        addresses: &[SocketAddr],
        ids: RequestIds,
        selected: &AtomicBool,
        known: Option<&DeviceVolume>,
        auth: Option<&DeviceAuth>,
    ) -> Result<CastClient, ConnectError> {
        let mut ordered: Vec<SocketAddr> = Vec::new();
        for &address in addresses {
            if !ordered.contains(&address) {
                ordered.push(address);
            }
        }
        ordered.sort_by_key(SocketAddr::is_ipv6);
        // Each address may take a TCP and a TLS timeout of its own (C109).
        ordered.truncate(MAX_CONNECT_ADDRESSES);
        // The first error is the one reported: it belongs to the preferred
        // address, usually IPv4.
        let mut first = None;
        for address in ordered {
            if !selected.load(Ordering::SeqCst) {
                return Err(ConnectError::Ended(EndReason::Stopped));
            }
            match CastClient::connect_one(address, ids.clone(), selected, known, auth) {
                Ok(client) => return Ok(client),
                Err(error @ ConnectError::Refused(_)) => return Err(error),
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }
        Err(first.unwrap_or(ConnectError::NoAddress))
    }

    fn connect_one(
        address: SocketAddr,
        ids: RequestIds,
        selected: &AtomicBool,
        known: Option<&DeviceVolume>,
        auth: Option<&DeviceAuth>,
    ) -> Result<CastClient, ConnectError> {
        let tcp = tcp_connect(address, selected)?;
        if !selected.load(Ordering::SeqCst) {
            return Err(ConnectError::Ended(EndReason::Stopped));
        }
        let tcp_error = |error| ConnectError::Tcp { address, error };
        tcp.set_nodelay(true).map_err(tcp_error)?;
        tcp.set_write_timeout(Some(CONNECT_TIMEOUT))
            .map_err(tcp_error)?;
        let local_address = tcp.local_addr().map_err(tcp_error)?;
        let tls_error = |message: String| ConnectError::Tls { address, message };
        let connector = tls_connector().map_err(|error| tls_error(error.to_string()))?;
        // Non-blocking, so a device that stalls the handshake can't keep a
        // deselect waiting longer than one poll step (C88).
        tcp.set_nonblocking(true).map_err(tcp_error)?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut handshake = connector.connect(&address.ip().to_string(), PeekedTcp::new(tcp));
        let stream = loop {
            match handshake {
                Ok(stream) => break stream,
                Err(HandshakeError::Failure(error)) => return Err(tls_error(error.to_string())),
                Err(HandshakeError::WouldBlock(pending)) => {
                    if !selected.load(Ordering::SeqCst) {
                        return Err(ConnectError::Ended(EndReason::Stopped));
                    }
                    if Instant::now() >= deadline {
                        return Err(tls_error("the handshake timed out".to_owned()));
                    }
                    thread::sleep(HANDSHAKE_POLL);
                    handshake = pending.handshake();
                }
            }
        };
        let tcp = &stream.get_ref().tcp;
        tcp.set_nonblocking(false).map_err(tcp_error)?;
        tcp.set_write_timeout(Some(WRITE_TIMEOUT))
            .map_err(tcp_error)?;
        let peer = stream
            .peer_certificate()
            .ok()
            .flatten()
            .and_then(|certificate| certificate.to_der().ok());
        let fingerprint = peer.as_deref().map(Fingerprint::of);
        stream
            .get_ref()
            .tcp
            .set_read_timeout(Some(READ_SLICE))
            .map_err(tcp_error)?;
        let mut stream = stream;
        let mut reader = FrameReader::new();
        let outcome = authenticate(&mut stream, &mut reader, peer.as_deref(), auth, selected)?;
        let decision = match auth {
            Some(auth) => {
                let mut book = auth.book.lock().unwrap_or_else(PoisonError::into_inner);
                book.judge(&auth.key, &outcome, auth.group)
            }
            None => decide(&address.to_string(), None, &outcome, false).0,
        };
        match decision.verdict {
            Verdict::Proceed => {}
            Verdict::ConnectFailed => return Err(ConnectError::Unauthenticated(decision.line)),
            Verdict::Refused => {
                let _ = stream.shutdown();
                return Err(ConnectError::Refused(decision.line));
            }
        }
        let mut client = CastClient {
            stream,
            reader,
            channel: Channel::open(Instant::now(), ids),
            events: VecDeque::new(),
            next_event: 0,
            address,
            local_address,
            fingerprint,
            auth: Some(decision),
            buffer: vec![0; 16 * 1024],
            ended: None,
            selected: None,
            teardown_until: None,
        };
        client.channel.assume_volume(known.cloned());
        loop {
            client.step(READ_SLICE);
            if client.channel.is_ready() {
                return Ok(client);
            }
            if let Some(reason) = client.end_reason() {
                return Err(match reason {
                    EndReason::NoStatus => ConnectError::NoStatus,
                    other => ConnectError::Ended(other),
                });
            }
            if !selected.load(Ordering::SeqCst) {
                return Err(ConnectError::Ended(EndReason::Stopped));
            }
        }
    }

    /// The address that worked, for the host to remember (C16).
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The local address of the connection, which the stream URL of a later
    /// stage uses (C55).
    pub fn local_address(&self) -> SocketAddr {
        self.local_address
    }

    /// The judged challenge of this connect (C132), once.
    pub fn take_auth(&mut self) -> Option<Decision> {
        self.auth.take()
    }

    /// The device certificate's fingerprint, when the TLS stack gave it.
    pub fn fingerprint(&self) -> Option<Fingerprint> {
        self.fingerprint
    }

    pub fn status(&self) -> Option<&ReceiverStatus> {
        self.channel.status()
    }

    pub fn is_ended(&self) -> bool {
        self.channel.is_ended()
    }

    /// Makes [`CastClient::launch`] and [`CastClient::load`] end with
    /// [`EndReason::Stopped`] as soon as `selected` is cleared.
    pub fn set_selected(&mut self, selected: Arc<AtomicBool>) {
        self.selected = Some(selected);
    }

    fn deselected(&self) -> bool {
        self.selected
            .as_ref()
            .is_some_and(|selected| !selected.load(Ordering::SeqCst))
    }

    /// Starts or reuses the Default Media Receiver and waits for it (C40 to
    /// C42).
    /// The result is taken out of the event queue; results of earlier
    /// launches never answer this one.
    pub fn launch(&mut self) -> Result<AppSession, LaunchError> {
        let since = self.next_event;
        self.channel.launch(Instant::now())?;
        loop {
            let found = self.events.iter().position(|(seq, event)| {
                *seq >= since && matches!(event, Event::Launched { .. } | Event::LaunchFailed(_))
            });
            if let Some(index) = found {
                return match self.events.remove(index) {
                    Some((_, Event::Launched { app, .. })) => Ok(app),
                    Some((_, Event::LaunchFailed(error))) => Err(error),
                    _ => unreachable!("position matched a launch result"),
                };
            }
            if let Some(reason) = self.end_reason() {
                return Err(LaunchError::Ended(reason));
            }
            if self.deselected() {
                return Err(LaunchError::Ended(EndReason::Stopped));
            }
            self.step(READ_SLICE);
        }
    }

    /// Sends `LOAD` and waits for its reply; returns the media session
    /// (C46, C49, C50). `PLAYING` is reported later as [`Event::Playing`].
    pub fn load(&mut self, request: &LoadRequest) -> Result<i64, MediaError> {
        let since = self.next_event;
        self.channel.load(Instant::now(), request)?;
        loop {
            let found = self.events.iter().position(|(seq, event)| {
                *seq >= since && matches!(event, Event::Loaded { .. } | Event::LoadFailed(_))
            });
            if let Some(index) = found {
                return match self.events.remove(index) {
                    Some((_, Event::Loaded { media_session_id })) => Ok(media_session_id),
                    Some((_, Event::LoadFailed(error))) => Err(error),
                    _ => unreachable!("position matched a load result"),
                };
            }
            if let Some(reason) = self.end_reason() {
                return Err(MediaError::Ended(reason));
            }
            if self.deselected() {
                return Err(MediaError::Ended(EndReason::Stopped));
            }
            self.step(READ_SLICE);
        }
    }

    /// Picks the app back up after a reconnect (C81); see
    /// [`Channel::resume`].
    pub fn resume(
        &mut self,
        app: &AppSession,
        media_session_id: Option<i64>,
        content_id: &str,
    ) -> bool {
        let resumed = self
            .channel
            .resume(Instant::now(), app, media_session_id, content_id);
        self.flush();
        resumed
    }

    /// The app the channel talks to.
    pub fn app(&self) -> Option<&AppSession> {
        self.channel.app()
    }

    pub fn media_session_id(&self) -> Option<i64> {
        self.channel.media_session_id()
    }

    /// Sets the device volume level under `ask` (C65, C67, C70).
    pub fn set_volume(&mut self, ask: u64, level: f64) -> Result<(), VolumeError> {
        let result = self.channel.ask_volume(Instant::now(), ask, level);
        self.flush();
        result
    }

    pub(crate) fn set_volume_interval(&mut self, interval: Duration) {
        self.channel.set_volume_interval(interval);
    }

    /// The level still waiting for its interval or for its answer (C70),
    /// taken off the channel.
    pub fn take_unanswered_volume(&mut self) -> Option<(u64, f64)> {
        self.channel.take_unanswered_volume()
    }

    /// Mutes or unmutes the device (C65).
    pub fn set_muted(&mut self, muted: bool) -> Result<(), VolumeError> {
        let result = self.channel.set_muted(muted);
        self.flush();
        result
    }

    /// One volume step up or down (C69); returns the level asked for.
    pub fn step_volume(&mut self, up: bool) -> Result<f64, VolumeError> {
        let result = self.channel.step_volume(Instant::now(), up);
        self.flush();
        result
    }

    /// Runs the channel for `duration`: reads, answers, heartbeats.
    pub fn run_for(&mut self, duration: Duration) {
        let until = Instant::now() + duration;
        while !self.channel.is_ended() {
            let now = Instant::now();
            if now >= until {
                break;
            }
            self.step((until - now).min(READ_SLICE));
        }
    }

    /// The end of the channel, once it ended.
    pub fn end_reason(&self) -> Option<EndReason> {
        self.ended.clone()
    }

    /// The next event the channel reported. A host that doesn't poll loses
    /// the oldest droppable reports beyond [`MAX_EVENTS`] (C110).
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front().map(|(_, event)| event)
    }

    /// Tears the session down and closes TLS (C44, C87), within `budget`.
    pub fn stop(mut self, budget: Duration) -> EndReason {
        self.stop_in_place(budget)
    }

    /// Drops the connection without a teardown, before a reconnect (C81).
    pub fn disconnect(&mut self) {
        let _ = self.timed_write(|stream| stream.shutdown());
        let _ = self.stream.get_ref().tcp.shutdown(std::net::Shutdown::Both);
        self.channel.end(EndReason::ClosedByDevice);
    }

    /// [`CastClient::stop`] for an owner that keeps the client.
    pub fn stop_in_place(&mut self, budget: Duration) -> EndReason {
        let until = Instant::now() + budget;
        self.teardown_until = Some(until);
        self.channel.stop(Instant::now());
        while !self.channel.is_ended() && Instant::now() < until {
            self.step(READ_SLICE.min(STOP_STEP_TIMEOUT));
        }
        self.flush();
        let _ = self.timed_write(|stream| stream.shutdown());
        self.end_reason().unwrap_or(EndReason::Stopped)
    }

    fn step(&mut self, wait: Duration) {
        self.flush();
        let _ = self
            .stream
            .get_ref()
            .tcp
            .set_read_timeout(Some(wait.max(Duration::from_millis(1))));
        match self.stream.read(&mut self.buffer) {
            Ok(0) => self.channel.end(EndReason::ClosedByDevice),
            Ok(count) => {
                self.reader.push(&self.buffer[..count]);
                loop {
                    match self.reader.next_message() {
                        Ok(Some(message)) => self.channel.handle(Instant::now(), message),
                        Ok(None) => break,
                        Err(fault) => {
                            self.channel.end(EndReason::Fault(fault));
                            break;
                        }
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => self.channel.end(EndReason::Io(error.to_string())),
        }
        self.channel.tick(Instant::now());
        self.flush();
        while let Some(event) = self.channel.poll_event() {
            if let Event::Ended(reason) = &event {
                self.ended.get_or_insert_with(|| reason.clone());
            }
            queue_event(&mut self.events, (self.next_event, event));
            self.next_event += 1;
        }
    }

    fn flush(&mut self) {
        while let Some(message) = self.channel.poll_outbound() {
            let frame = match encode_frame(&message) {
                Ok(frame) => frame,
                Err(fault) => {
                    self.channel.end(EndReason::Fault(fault));
                    return;
                }
            };
            let result = self.timed_write(|stream| {
                stream.write_all(&frame)?;
                stream.flush()
            });
            if let Err(error) = result {
                self.channel.end(EndReason::Io(error.to_string()));
                return;
            }
        }
    }

    /// Gives one frame or TLS close [`WRITE_TIMEOUT`] in all, or what is
    /// left of a teardown's budget when that is less. Writes the TLS stack
    /// makes on its own during a read keep the per-send timeout only.
    fn timed_write(
        &mut self,
        write: impl FnOnce(&mut TlsStream<PeekedTcp>) -> io::Result<()>,
    ) -> io::Result<()> {
        let now = Instant::now();
        let mut until = now + WRITE_TIMEOUT;
        if let Some(end) = self.teardown_until {
            until = until.min(end.max(now + Duration::from_millis(1)));
        }
        self.stream.get_mut().write_until = Some(until);
        let result = write(&mut self.stream);
        let tcp = self.stream.get_mut();
        tcp.write_until = None;
        let _ = tcp.tcp.set_write_timeout(Some(WRITE_TIMEOUT));
        result
    }
}

/// Queues an event for the owner, keeping the queue to [`MAX_EVENTS`]
/// whatever the kinds in it (C110).
fn queue_event(events: &mut VecDeque<(u64, Event)>, event: (u64, Event)) {
    events.push_back(event);
    if events.len() <= MAX_EVENTS {
        return;
    }
    let routine = events.iter().position(|(_, event)| {
        matches!(
            event,
            Event::Status(_)
                | Event::Dropped(_)
                | Event::MediaStatus(_)
                | Event::Multizone(MultizoneMessage::Other(_))
        )
    });
    let oldest = routine.or_else(|| moot_membership(events)).or_else(|| {
        events.iter().position(|(_, event)| {
            matches!(
                event,
                Event::Rebuffering | Event::Resumed | Event::Multizone(_)
            )
        })
    });
    if let Some(index) = oldest {
        events.remove(index);
    }
}

/// The oldest multizone membership message a later one makes moot: a later
/// `MULTIZONE_STATUS` replaces the whole list, and a later message about the
/// same member replaces what this one said about it.
fn moot_membership(events: &VecDeque<(u64, Event)>) -> Option<usize> {
    let last_status = events
        .iter()
        .rposition(|(_, event)| matches!(event, Event::Multizone(MultizoneMessage::Status(_))));
    if let Some(last) = last_status {
        let before = events.iter().take(last).position(|(_, event)| {
            matches!(
                event,
                Event::Multizone(
                    MultizoneMessage::Status(_)
                        | MultizoneMessage::Upsert(_)
                        | MultizoneMessage::Removed(_)
                )
            )
        });
        if before.is_some() {
            return before;
        }
    }
    // Parsed once: the scan compares every pair.
    let named: Vec<Option<MemberName<'_>>> = events
        .iter()
        .map(|(_, event)| MemberName::of(event))
        .collect();
    named.iter().enumerate().find_map(|(index, member)| {
        let member = member.as_ref()?;
        named[index + 1..]
            .iter()
            .flatten()
            .any(|later| member.same(later))
            .then_some(index)
    })
}

/// The `deviceId` a `DEVICE_ADDED`, `DEVICE_UPDATED` or `DEVICE_REMOVED`
/// names.
struct MemberName<'a> {
    text: &'a str,
    id: Option<DeviceId>,
}

impl<'a> MemberName<'a> {
    fn of(event: &'a Event) -> Option<MemberName<'a>> {
        let text = match event {
            Event::Multizone(MultizoneMessage::Upsert(member)) => &member.device_id,
            Event::Multizone(MultizoneMessage::Removed(device_id)) => device_id,
            _ => return None,
        };
        Some(MemberName {
            text,
            id: DeviceId::parse(text),
        })
    }

    /// Compared as [`GroupMember::is`] does (C95).
    ///
    /// [`GroupMember::is`]: crate::group::GroupMember::is
    fn same(&self, other: &MemberName<'_>) -> bool {
        match (self.id, other.id) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => self.text.eq_ignore_ascii_case(other.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::GroupMember;
    use std::net::{Shutdown, TcpListener};
    use std::thread;

    fn upsert(device_id: &str) -> Event {
        Event::Multizone(MultizoneMessage::Upsert(GroupMember {
            device_id: device_id.to_owned(),
            id: None,
            name: None,
        }))
    }

    fn names(event: &Event, device_id: &str) -> bool {
        matches!(event, Event::Multizone(MultizoneMessage::Upsert(member)) if member.device_id == device_id)
    }

    // covers: C110
    #[test]
    fn the_event_queue_stays_at_its_cap_whatever_the_kind() {
        let mut events = VecDeque::new();
        queue_event(
            &mut events,
            (
                0,
                Event::Loaded {
                    media_session_id: 1,
                },
            ),
        );
        queue_event(&mut events, (1, upsert("kitchen")));
        for n in 0..1000 {
            let member = if n % 2 == 0 { "attic" } else { "hall" };
            queue_event(&mut events, (n + 2, upsert(member)));
        }
        assert!(events.len() <= MAX_EVENTS, "{} queued", events.len());
        assert!(matches!(events[0].1, Event::Loaded { .. }));
        assert!(events.iter().any(|(_, event)| names(event, "kitchen")));
        assert!(events.iter().any(|(_, event)| names(event, "attic")));
        let (last, newest) = events.back().expect("queued");
        assert_eq!(*last, 1001);
        assert!(names(newest, "hall"));

        let mut events = VecDeque::new();
        for n in 0..1000 {
            queue_event(&mut events, (n, upsert(&format!("member {n}"))));
        }
        assert!(events.len() <= MAX_EVENTS, "{} queued", events.len());
        assert_eq!(events.back().map(|(n, _)| *n), Some(999));
    }

    const SLICE: Duration = Duration::from_millis(20);

    fn connected() -> (PeekedTcp, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let tcp = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (device, _) = listener.accept().expect("accept");
        tcp.set_read_timeout(Some(SLICE)).expect("timeout");
        (PeekedTcp::new(tcp), device)
    }

    // A frame whose halves straddle the read slice arrives whole, however
    // often the wait between them times out.
    #[test]
    fn bytes_split_across_read_timeouts_arrive_intact() {
        let (mut tcp, mut device) = connected();
        let frame: Vec<u8> = (0..=255).collect();
        let sent = frame.clone();
        let writer = thread::spawn(move || {
            for pause in [1, 2, 3] {
                device.write_all(&sent[..100]).expect("write");
                thread::sleep(SLICE * pause + Duration::from_millis(5));
                device.write_all(&sent[100..]).expect("write");
            }
            device
        });
        let mut received = Vec::new();
        let mut buffer = [0u8; 64];
        let deadline = Instant::now() + Duration::from_secs(5);
        while received.len() < 3 * frame.len() && Instant::now() < deadline {
            match tcp.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => received.extend_from_slice(&buffer[..count]),
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ),
                    "{error}"
                ),
            }
        }
        let _device = writer.join().expect("writer");
        assert_eq!(received, frame.repeat(3));
    }

    #[cfg(windows)]
    #[test]
    fn a_peek_ending_with_io_pending_is_a_timeout() {
        let error = peek_error(io::Error::from_raw_os_error(997));
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn other_peek_errors_stay_as_they_are() {
        let error = peek_error(io::Error::from(io::ErrorKind::ConnectionReset));
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }

    // After the stop's shutdown the next read ends at once instead of
    // waiting out its timeout.
    #[test]
    fn a_read_after_the_shutdown_ends_at_once() {
        let (mut tcp, _device) = connected();
        tcp.tcp
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        tcp.tcp.shutdown(Shutdown::Both).expect("shutdown");
        let started = Instant::now();
        let mut buffer = [0u8; 16];
        assert!(!matches!(tcp.read(&mut buffer), Ok(count) if count > 0));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    // Linux ends a blocked send with the part it got out, and the retry
    // waits again; the deadline holds across every send until it passes.
    #[test]
    fn a_write_keeps_to_its_deadline_across_partial_sends() {
        let (mut tcp, _device) = connected();
        // macOS runners overshoot a send timeout by about half of it; a retry
        // that waited the whole limit again would still take twice the limit.
        let limit = Duration::from_secs(1);
        tcp.write_until = Some(Instant::now() + limit);
        let started = Instant::now();
        let chunk = vec![0u8; 1 << 16];
        while tcp.write_all(&chunk).is_ok() {
            assert!(started.elapsed() < Duration::from_secs(5), "never blocked");
        }
        let took = started.elapsed();
        assert!(took < limit * 2, "the write took {took:?}");
    }

    // covers: C109
    #[test]
    fn a_connect_tries_at_most_eight_addresses() {
        let listeners: Vec<TcpListener> = (0..40)
            .map(|_| TcpListener::bind("127.0.0.1:0").expect("listener"))
            .collect();
        let mut addresses: Vec<SocketAddr> = listeners
            .iter()
            .map(|listener| listener.local_addr().expect("address"))
            .collect();
        addresses.insert(1, addresses[0]);
        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&done);
        // Each connection is closed unanswered, so its TLS handshake fails
        // at once.
        let device = thread::spawn(move || {
            let mut accepted = vec![0usize; listeners.len()];
            for listener in &listeners {
                listener.set_nonblocking(true).expect("non-blocking");
            }
            while !stop.load(Ordering::SeqCst) {
                for (count, listener) in accepted.iter_mut().zip(&listeners) {
                    if let Ok((tcp, _)) = listener.accept() {
                        *count += 1;
                        drop(tcp);
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            accepted
        });
        let result = CastClient::connect(&addresses, RequestIds::random());
        done.store(true, Ordering::SeqCst);
        let accepted = device.join().expect("device");
        assert!(result.is_err());
        assert_eq!(
            accepted[..MAX_CONNECT_ADDRESSES],
            [1; MAX_CONNECT_ADDRESSES]
        );
        assert!(
            accepted[MAX_CONNECT_ADDRESSES..]
                .iter()
                .all(|&count| count == 0)
        );
    }
}
