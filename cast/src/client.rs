//! The channel over a real TLS connection (C19 to C21), driven on the
//! caller's thread with blocking sockets and short read timeouts.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
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
/// The longest a read waits before timers run again.
const READ_SLICE: Duration = Duration::from_millis(50);
/// Events kept for a host that doesn't poll: beyond this, the oldest
/// status and drop reports go first; launch results and the end are kept.
pub const MAX_EVENTS: usize = 64;
/// The longest teardown at host quit (C88).
pub const QUIT_BUDGET: Duration = Duration::from_secs(3);

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

/// An open channel to one device.
pub struct CastClient {
    stream: TlsStream<TcpStream>,
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
    /// (C3, C15).
    pub fn connect_device(device: &CastDevice) -> Result<CastClient, ConnectError> {
        if device.kind == DeviceKind::Group {
            return Err(ConnectError::Group);
        }
        CastClient::connect(&device.socket_addresses(), RequestIds::random())
    }

    /// Tries each address in turn (IPv4 first, C16) and returns once the
    /// first `RECEIVER_STATUS` arrived (C33).
    pub fn connect(addresses: &[SocketAddr], ids: RequestIds) -> Result<CastClient, ConnectError> {
        let mut ordered = addresses.to_vec();
        ordered.sort_by_key(SocketAddr::is_ipv6);
        // The first error is the one reported: it belongs to the preferred
        // address, usually IPv4.
        let mut first = None;
        for address in ordered {
            match CastClient::connect_one(address, ids.clone()) {
                Ok(client) => return Ok(client),
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }
        Err(first.unwrap_or(ConnectError::NoAddress))
    }

    fn connect_one(address: SocketAddr, ids: RequestIds) -> Result<CastClient, ConnectError> {
        let tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(|error| ConnectError::Tcp { address, error })?;
        let tcp_error = |error| ConnectError::Tcp { address, error };
        tcp.set_nodelay(true).map_err(tcp_error)?;
        tcp.set_read_timeout(Some(CONNECT_TIMEOUT))
            .map_err(tcp_error)?;
        tcp.set_write_timeout(Some(CONNECT_TIMEOUT))
            .map_err(tcp_error)?;
        let local_address = tcp.local_addr().map_err(tcp_error)?;
        let tls_error = |message: String| ConnectError::Tls { address, message };
        let connector = tls_connector().map_err(|error| tls_error(error.to_string()))?;
        let stream =
            connector
                .connect(&address.ip().to_string(), tcp)
                .map_err(|error| match error {
                    HandshakeError::Failure(error) => tls_error(error.to_string()),
                    HandshakeError::WouldBlock(_) => {
                        tls_error("the handshake timed out".to_owned())
                    }
                })?;
        let fingerprint = stream
            .peer_certificate()
            .ok()
            .flatten()
            .and_then(|certificate| certificate.to_der().ok())
            .map(|der| Fingerprint::of(&der));
        stream
            .get_ref()
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
        let _ = self.stream.shutdown();
        let _ = self.stream.get_ref().shutdown(std::net::Shutdown::Both);
        self.channel.end(EndReason::ClosedByDevice);
    }

    /// [`CastClient::stop`] for an owner that keeps the client.
    pub fn stop_in_place(&mut self, budget: Duration) -> EndReason {
        let until = Instant::now() + budget;
        self.channel.stop(Instant::now());
        while !self.channel.is_ended() && Instant::now() < until {
            self.step(READ_SLICE.min(STOP_STEP_TIMEOUT));
        }
        self.flush();
        let _ = self.stream.shutdown();
        self.end_reason().unwrap_or(EndReason::Stopped)
    }

    fn step(&mut self, wait: Duration) {
        self.flush();
        let _ = self
            .stream
            .get_ref()
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
            let result = match encode_frame(&message) {
                Ok(frame) => self.stream.write_all(&frame),
                Err(fault) => {
                    self.channel.end(EndReason::Fault(fault));
                    return;
                }
            };
            if let Err(error) = result.and_then(|()| self.stream.flush()) {
                self.channel.end(EndReason::Io(error.to_string()));
                return;
            }
        }
    }
}
