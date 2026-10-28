//! The channel over a real TLS connection (C19 to C21), driven on the
//! caller's thread with blocking sockets and short read timeouts.
//!
//! A read times out only in a peek, then reads queued bytes ([`PeekedTcp`]):
//! a cancel can't drop bytes a peek never took (Windows `SO_RCVTIMEO`).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use native_tls::{HandshakeError, Protocol, TlsConnector, TlsStream};
use sha2::{Digest, Sha256};

use crate::channel::{Channel, Event, STOP_STEP_TIMEOUT};
use crate::discovery::{CastDevice, DeviceId, DeviceKind};
use crate::error::{ConnectError, EndReason, LaunchError};
use crate::framing::{FrameReader, encode_frame};
use crate::ids::RequestIds;
use crate::media::{LoadRequest, MediaError, VolumeError};
use crate::status::{AppSession, ReceiverStatus};

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
/// status and drop reports go first; launch results and the end are kept.
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

/// The fingerprints seen per device, for diagnostics only (C21).
#[derive(Debug, Default)]
pub struct FingerprintBook {
    seen: HashMap<DeviceId, Fingerprint>,
}

impl FingerprintBook {
    pub fn new() -> FingerprintBook {
        FingerprintBook::default()
    }

    pub fn record(&mut self, id: DeviceId, fingerprint: Fingerprint) -> FingerprintChange {
        match self.seen.insert(id, fingerprint) {
            None => FingerprintChange::First,
            Some(before) if before == fingerprint => FingerprintChange::Same,
            Some(before) => FingerprintChange::Changed { before },
        }
    }
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

    /// Tries each address in turn (IPv4 first, C16) and returns once the
    /// first `RECEIVER_STATUS` arrived (C33).
    pub fn connect(addresses: &[SocketAddr], ids: RequestIds) -> Result<CastClient, ConnectError> {
        CastClient::connect_while(addresses, ids, &AtomicBool::new(true))
    }

    /// [`CastClient::connect`] that gives up within a read slice once
    /// `selected` is cleared, with [`EndReason::Stopped`], so a quit never
    /// waits out the timeouts of every address (C88).
    pub fn connect_while(
        addresses: &[SocketAddr],
        ids: RequestIds,
        selected: &AtomicBool,
    ) -> Result<CastClient, ConnectError> {
        let mut ordered = addresses.to_vec();
        ordered.sort_by_key(SocketAddr::is_ipv6);
        // The first error is the one reported: it belongs to the preferred
        // address, usually IPv4.
        let mut first = None;
        for address in ordered {
            if !selected.load(Ordering::SeqCst) {
                return Err(ConnectError::Ended(EndReason::Stopped));
            }
            match CastClient::connect_one(address, ids.clone(), selected) {
                Ok(client) => return Ok(client),
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
        let fingerprint = stream
            .peer_certificate()
            .ok()
            .flatten()
            .and_then(|certificate| certificate.to_der().ok())
            .map(|der| Fingerprint::of(&der));
        stream
            .get_ref()
            .tcp
            .set_read_timeout(Some(READ_SLICE))
            .map_err(tcp_error)?;
        let mut client = CastClient {
            stream,
            reader: FrameReader::new(),
            channel: Channel::open(Instant::now(), ids),
            events: VecDeque::new(),
            next_event: 0,
            address,
            local_address,
            fingerprint,
            buffer: vec![0; 16 * 1024],
            ended: None,
            selected: None,
            teardown_until: None,
        };
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

    /// Sets the device volume level (C65, C67, C70).
    pub fn set_volume(&mut self, level: f64) -> Result<(), VolumeError> {
        let result = self.channel.set_volume(Instant::now(), level);
        self.flush();
        result
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
    /// the oldest status and drop reports beyond [`MAX_EVENTS`].
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
            self.events.push_back((self.next_event, event));
            self.next_event += 1;
            if self.events.len() > MAX_EVENTS {
                let routine = self.events.iter().position(|(_, event)| {
                    matches!(
                        event,
                        Event::Status(_) | Event::Dropped(_) | Event::MediaStatus(_)
                    )
                });
                if let Some(index) = routine {
                    self.events.remove(index);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Shutdown, TcpListener};
    use std::thread;

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
