//! Sockets and threads of one session (P3, P6, P20, P31, P82, P83, P91,
//! P141).
//!
//! Everything here is plain `std::net` on `std::thread`, without an async
//! runtime. A connect attempt runs on a thread of its own and reports back
//! ([`connect`]). Once the TCP connection stands, [`SessionIo::open`] binds
//! the session's control and timing sockets on the connection's own local
//! address (so in its address family, P82) with ephemeral ports, and starts
//! four threads:
//!
//! - the timing thread answers every timing request the moment it arrives,
//!   reading the sender clock right after the receive call and right
//!   before the send call (P91), and tells the driver (P131);
//! - the control thread hands retransmission requests to the driver (P86);
//! - the RTSP reader hands everything the receiver writes to the driver,
//!   and reports the connection closing (P141);
//! - the RTSP writer writes the session's requests, so a receiver that
//!   stops reading blocks only its own thread, never the driver or another
//!   speaker (P3, P141).
//!
//! Both UDP threads take datagrams only from the receiver: the address the
//! connection reached (a global IPv6 one by its /64 prefix), from any port
//! ([`from_receiver`]). Others are dropped unread and counted
//! (P86, P91, P144). That screens out stray datagrams (a stale session,
//! another device) from timing replies, resends and the liveness check
//! (P131); it is no security boundary against a forged source address.
//!
//! The threads start before the session learns its ports, so both sockets
//! are open and answering before SETUP names them (P31). Audio, sync and
//! resend datagrams leave from the control socket, timing replies from the
//! timing socket (P83, P91); the driver sends them with
//! [`SessionIo::send`]. Closing ([`SessionIo::close`]) shuts the
//! connection down and tells the threads to stop; each ends within
//! [`POLL_INTERVAL`], except a writer blocked in a send: Windows doesn't wake
//! a blocked send when the socket is shut down, so that one ends at its send
//! timeout, and nobody waits for it.
//!
//! The RTSP reader times out only in a peek, then reads queued bytes: a
//! cancel can't drop bytes a peek never took (Windows `SO_RCVTIMEO`; see core/NOTES.md).
//!
//! The UDP threads receive with only a long backstop timeout
//! ([`UDP_BACKSTOP`], P86): on Windows a receive that times out as a
//! datagram lands can drop that datagram, which would lose a resend or
//! timing request. Closing wakes them instead with an empty datagram each
//! socket sends itself ([`wake`]); the threads drop empty datagrams unread
//! and uncounted. A wake that can't be sent (the local address is gone)
//! leaves the thread to its backstop.
//!
//! A pairing (P47) needs only the TCP connection: [`RtspIo`] runs the same
//! RTSP reader and writer threads, without UDP sockets.
//!
//! A thread whose work panics (a bug) reports what a failure of that work
//! would: a connect thread a failed connect, a session or pairing thread a
//! closed connection. The driver then retries at once, as after a real
//! failure, instead of noticing only at a timeout.
//!
//! Where an ICMP unreachable is reported matters for P141. Windows reports
//! it as a reset on the next call on the socket, which is the control
//! thread's receive, not the send that caused it; the control thread notes
//! it, and the next send from that socket counts as unreachable. Linux and
//! macOS report nothing on an unconnected UDP socket, so there only send
//! errors count.

use std::io::{self, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV6, TcpStream, UdpSocket,
};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::clock::SenderClock;
use crate::rtp::{self, ResendRequest};
use crate::session::{CONNECT_TIMEOUT, Connection, RESPONSE_TIMEOUT, UdpSendResult};
use crate::stream::{LocalSocket, SpeakerCounters};

/// How often a blocked RTSP reader looks at its stop flag: the longest it
/// outlives its session's close.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long a UDP receive waits before its thread looks at the stop flag
/// again: the longest a UDP thread outlives a close whose wake-up couldn't
/// be sent. Long, so a timed-out receive racing a datagram stays rare.
pub(crate) const UDP_BACKSTOP: Duration = Duration::from_secs(2);
/// The longest one UDP send may block before it is given up.
const UDP_SEND_TIMEOUT: Duration = Duration::from_millis(20);
/// The longest one RTSP write may block; the session's own response
/// timeout ends a stuck session sooner.
const RTSP_WRITE_TIMEOUT: Duration = RESPONSE_TIMEOUT;
/// Datagrams larger than this are not AirPlay control or timing packets.
const DATAGRAM_BYTES: usize = 2048;
/// What an RTSP read takes at most.
const READ_BYTES: usize = 16 * 1024;
/// What one RTSP write call takes at most, so the writer looks at its stop
/// flag between pieces of a large request.
const WRITE_BYTES: usize = 16 * 1024;

/// Names one session's I/O, so the driver can tell late messages from a
/// closed session's threads apart from the next session's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SessionKey(pub u64);

/// What the I/O threads report to the driver.
#[derive(Debug)]
pub(crate) enum IoEvent {
    /// Connect attempt `attempt` of the session succeeded.
    Connected {
        session: SessionKey,
        attempt: u32,
        stream: TcpStream,
    },
    /// Connect attempt `attempt` was refused, found no route or ran out of
    /// time (P134).
    ConnectFailed { session: SessionKey, attempt: u32 },
    /// Bytes the receiver wrote on the RTSP connection of connect attempt
    /// `attempt`.
    Rtsp {
        session: SessionKey,
        attempt: u32,
        bytes: Vec<u8>,
    },
    /// The RTSP connection of connect attempt `attempt` closed or failed
    /// (P141). A session that connected again since (P58) drops a late one
    /// from the earlier connection.
    RtspClosed { session: SessionKey, attempt: u32 },
    /// A retransmission request on the control socket (P86).
    Control { session: SessionKey, bytes: Vec<u8> },
    /// A timing request arrived at `at` and was answered (P91, P131).
    TimingRequest { session: SessionKey, at: Instant },
    /// What looks like a Delay_Req reached the PTP event socket at `at`
    /// (P162).
    Ptp {
        bytes: Vec<u8>,
        source: SocketAddr,
        at: Instant,
    },
}

/// How the threads reach the driver: false once it is gone, which ends the
/// thread.
pub(crate) trait Notify: Fn(IoEvent) -> bool + Send + Clone + 'static {}

impl<T: Fn(IoEvent) -> bool + Send + Clone + 'static> Notify for T {}

/// Starts the thread `name` running `body`. A panic in `body` (a bug) is
/// stopped there: the thread then runs `on_panic`, which tells the driver
/// what a failure of the thread's own work would, and ends. The speaker is
/// retried at once instead of after a session timeout.
fn spawn(
    name: &str,
    body: impl FnOnce() + Send + 'static,
    on_panic: impl FnOnce() + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        if panic::catch_unwind(AssertUnwindSafe(body)).is_err() {
            on_panic();
        }
    })
}

/// What a session thread that panicked reports: its connection closed
/// (P141), which the session handles as a lost speaker.
fn report_closed(
    session: SessionKey,
    attempt: u32,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        notify(IoEvent::RtspClosed { session, attempt });
    }
}

/// Starts connect attempt `attempt` to `address` on a thread of its own; the
/// result comes back as [`IoEvent::Connected`] or [`IoEvent::ConnectFailed`]
/// within [`CONNECT_TIMEOUT`] (P20, P134). The thread is not joined: a
/// result nobody waits for any more is dropped, which closes the
/// connection.
pub(crate) fn connect(
    session: SessionKey,
    attempt: u32,
    address: SocketAddr,
    notify: impl Notify,
) -> io::Result<()> {
    let failed = notify.clone();
    spawn(
        "airplay-connect",
        move || {
            let event = match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(stream) => IoEvent::Connected {
                    session,
                    attempt,
                    stream,
                },
                Err(_) => IoEvent::ConnectFailed { session, attempt },
            };
            notify(event);
        },
        move || {
            failed(IoEvent::ConnectFailed { session, attempt });
        },
    )
    .map(drop)
}

/// Whether a send error says the destination can't be reached (P141).
fn unreachable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
    )
}

/// The address a session's UDP sockets bind to: the connection's local
/// address, with an ephemeral port and, for IPv6, the same scope (P82).
fn udp_bind_address(local: SocketAddr) -> SocketAddr {
    match local {
        SocketAddr::V4(address) => SocketAddr::new(IpAddr::V4(*address.ip()), 0),
        SocketAddr::V6(address) => {
            SocketAddr::V6(SocketAddrV6::new(*address.ip(), 0, 0, address.scope_id()))
        }
    }
}

/// A session's control and timing sockets, bound next to its RTSP
/// connection.
struct Udp {
    control: UdpSocket,
    timing: UdpSocket,
    connection: Connection,
}

impl Udp {
    fn bind(tcp: &TcpStream) -> io::Result<Udp> {
        let local = tcp.local_addr()?;
        let bind_at = udp_bind_address(local);
        let control = UdpSocket::bind(bind_at)?;
        let timing = UdpSocket::bind(bind_at)?;
        for socket in [&control, &timing] {
            socket.set_read_timeout(Some(UDP_BACKSTOP))?;
            socket.set_write_timeout(Some(UDP_SEND_TIMEOUT))?;
        }
        let connection = Connection {
            local: local.ip(),
            control_port: control.local_addr()?.port(),
            timing_port: timing.local_addr()?.port(),
        };
        Ok(Udp {
            control,
            timing,
            connection,
        })
    }
}

/// The sockets and threads of one session.
#[derive(Debug)]
pub(crate) struct SessionIo {
    tcp: TcpStream,
    writer: Option<mpsc::Sender<Vec<u8>>>,
    /// Shared with the writer thread, which may have been a pairing's.
    backlog: Arc<Backlog>,
    control: Arc<UdpSocket>,
    timing: Arc<UdpSocket>,
    stop: Arc<AtomicBool>,
    /// Set by the control thread when a receive reports a reset: an ICMP
    /// unreachable for an earlier send (Windows).
    reset: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl SessionIo {
    /// Opens the session's UDP sockets next to the connected `tcp` and
    /// starts its threads. Timing replies are stamped on `clock` and counted
    /// in `counters`. Returns what the session needs to know
    /// ([`Session::handle_connected`](crate::session::Session::handle_connected)).
    pub(crate) fn open(
        session: SessionKey,
        attempt: u32,
        tcp: TcpStream,
        clock: SenderClock,
        counters: Arc<SpeakerCounters>,
        notify: impl Notify,
    ) -> io::Result<(SessionIo, Connection)> {
        // Requests are small and answered one at a time (P20).
        let _ = tcp.set_nodelay(true);
        tcp.set_write_timeout(Some(RTSP_WRITE_TIMEOUT))?;
        tcp.set_read_timeout(Some(POLL_INTERVAL))?;
        let udp = Udp::bind(&tcp)?;
        let reader = tcp.try_clone()?;
        let writer_stream = tcp.try_clone()?;
        let (writer, writes) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let backlog = Arc::new(Backlog::default());
        let (mut io, connection) = SessionIo::with(tcp, writer, backlog, stop, Vec::new(), udp);
        // On a failed spawn, dropping `io` stops the threads already
        // running.
        io.spawn_udp(session, attempt, clock, counters, notify.clone())?;
        let thread = spawn(
            "airplay-rtsp-read",
            read_loop(session, attempt, reader, io.stop.clone(), notify.clone()),
            report_closed(session, attempt, notify.clone()),
        )?;
        io.threads.push(thread);
        let thread = spawn(
            "airplay-rtsp-write",
            write_loop(
                session,
                attempt,
                writer_stream,
                writes,
                io.backlog.clone(),
                io.stop.clone(),
                notify.clone(),
            ),
            report_closed(session, attempt, notify),
        )?;
        io.threads.push(thread);
        Ok((io, connection))
    }

    /// Takes over the connection of a PIN pairing that paired (P153): its
    /// reader and writer go on, still reporting under `session` and
    /// `attempt`, and the session's UDP sockets open next to it. On failure
    /// the connection is closed and its threads come back to be joined.
    pub(crate) fn adopt(
        mut rtsp: RtspIo,
        session: SessionKey,
        attempt: u32,
        clock: SenderClock,
        counters: Arc<SpeakerCounters>,
        notify: impl Notify,
    ) -> Result<(SessionIo, Connection), Vec<JoinHandle<()>>> {
        let udp = match rtsp.tcp.as_ref().map(Udp::bind) {
            Some(Ok(udp)) => udp,
            _ => return Err(rtsp.close()),
        };
        let Some(writer) = rtsp.writer.take() else {
            return Err(rtsp.close());
        };
        let Some(tcp) = rtsp.tcp.take() else {
            return Err(Vec::new());
        };
        let threads = std::mem::take(&mut rtsp.threads);
        let (mut io, connection) = SessionIo::with(
            tcp,
            writer,
            rtsp.backlog.clone(),
            rtsp.stop.clone(),
            threads,
            udp,
        );
        match io.spawn_udp(session, attempt, clock, counters, notify) {
            Ok(()) => Ok((io, connection)),
            Err(_) => Err(io.close()),
        }
    }

    fn with(
        tcp: TcpStream,
        writer: mpsc::Sender<Vec<u8>>,
        backlog: Arc<Backlog>,
        stop: Arc<AtomicBool>,
        threads: Vec<JoinHandle<()>>,
        udp: Udp,
    ) -> (SessionIo, Connection) {
        let io = SessionIo {
            tcp,
            writer: Some(writer),
            backlog,
            control: Arc::new(udp.control),
            timing: Arc::new(udp.timing),
            stop,
            reset: Arc::new(AtomicBool::new(false)),
            threads,
        };
        (io, udp.connection)
    }

    /// Starts the threads of the timing and control sockets, which answer
    /// the receiver at the far end of `self.tcp` only.
    fn spawn_udp(
        &mut self,
        session: SessionKey,
        attempt: u32,
        clock: SenderClock,
        counters: Arc<SpeakerCounters>,
        notify: impl Notify,
    ) -> io::Result<()> {
        let receiver = self.tcp.peer_addr()?.ip().to_canonical();
        let thread = spawn(
            "airplay-timing",
            timing_loop(
                session,
                self.timing.clone(),
                receiver,
                clock,
                counters.clone(),
                self.stop.clone(),
                notify.clone(),
            ),
            report_closed(session, attempt, notify.clone()),
        )?;
        self.threads.push(thread);
        let thread = spawn(
            "airplay-control",
            control_loop(
                session,
                self.control.clone(),
                receiver,
                counters,
                self.stop.clone(),
                self.reset.clone(),
                notify.clone(),
            ),
            report_closed(session, attempt, notify),
        )?;
        self.threads.push(thread);
        Ok(())
    }

    /// Queues bytes for the RTSP connection; the writer thread writes them
    /// in order.
    pub(crate) fn write(&self, bytes: Vec<u8>) {
        if let Some(writer) = &self.writer {
            self.backlog.add(bytes.len());
            let _ = writer.send(bytes);
        }
    }

    /// Sends one datagram from the session's control or timing socket. The
    /// result is `None` when the send failed for a reason other than an
    /// unreachable destination (P141). A send from the control socket after
    /// the control thread saw a reset counts as unreachable, whatever the
    /// call returned: the reset is the late report of an earlier send.
    pub(crate) fn send(
        &self,
        from: LocalSocket,
        to: SocketAddr,
        bytes: &[u8],
    ) -> Option<UdpSendResult> {
        let socket = match from {
            LocalSocket::Control => &self.control,
            LocalSocket::Timing => &self.timing,
        };
        let reported =
            matches!(from, LocalSocket::Control) && self.reset.swap(false, Ordering::AcqRel);
        match socket.send_to(bytes, to) {
            Ok(_) if reported => Some(UdpSendResult::Unreachable),
            Ok(_) => Some(UdpSendResult::Sent),
            Err(error) if unreachable(&error) => Some(UdpSendResult::Unreachable),
            Err(_) => None,
        }
    }

    /// Closes the connection and stops the threads. The handles come back
    /// for the caller to join; the UDP threads end once their wake-up
    /// arrives (within [`UDP_BACKSTOP`] if it can't be sent), the RTSP
    /// reader within [`POLL_INTERVAL`].
    pub(crate) fn close(mut self) -> Vec<JoinHandle<()>> {
        self.stop_all();
        std::mem::take(&mut self.threads)
    }

    fn stop_all(&mut self) {
        if self.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        self.writer = None;
        let _ = self.tcp.shutdown(Shutdown::Both);
        wake(&self.control);
        wake(&self.timing);
    }
}

impl Drop for SessionIo {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// How much a connection may hold queued and not yet written.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BacklogLimit {
    pub(crate) messages: usize,
    pub(crate) bytes: usize,
}

/// The unwritten answers an AirPlay 2 event connection may hold: the
/// receiver sends a few requests a session and reads each answer, so a
/// backlog this deep means it stopped reading.
pub(crate) const EVENT_BACKLOG: BacklogLimit = BacklogLimit {
    messages: 256,
    bytes: 256 * 1024,
};

/// What a bounded write did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Queued {
    Yes,
    /// This write passed the limit: nothing was queued and the connection
    /// is shut down, so its reader reports it closed.
    Overflowed,
    /// An earlier write overflowed; nothing was queued.
    Dropped,
}

/// The messages and bytes handed to a writer thread and not yet written.
#[derive(Debug, Default)]
pub(crate) struct Backlog {
    messages: AtomicUsize,
    bytes: AtomicUsize,
    overflowed: AtomicBool,
}

impl Backlog {
    /// Counts a message of `len` bytes in, unless that passes `limit`.
    /// Only one thread admits; the writer only takes away, so the check
    /// can't be overtaken.
    fn admit(&self, len: usize, limit: BacklogLimit) -> bool {
        let (messages, bytes) = self.queued();
        if messages >= limit.messages || bytes.saturating_add(len) > limit.bytes {
            return false;
        }
        self.add(len);
        true
    }

    fn add(&self, len: usize) {
        self.messages.fetch_add(1, Ordering::AcqRel);
        self.bytes.fetch_add(len, Ordering::AcqRel);
    }

    fn written(&self, len: usize) {
        self.messages.fetch_sub(1, Ordering::AcqRel);
        self.bytes.fetch_sub(len, Ordering::AcqRel);
    }

    fn queued(&self) -> (usize, usize) {
        (
            self.messages.load(Ordering::Acquire),
            self.bytes.load(Ordering::Acquire),
        )
    }
}

/// The connection of a pairing (P47): an RTSP reader and writer on their
/// own threads, as in a session, without UDP sockets.
#[derive(Debug)]
pub(crate) struct RtspIo {
    /// None once a session took the connection over.
    tcp: Option<TcpStream>,
    writer: Option<mpsc::Sender<Vec<u8>>>,
    backlog: Arc<Backlog>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl RtspIo {
    /// Starts the reader and writer threads on the connected `tcp`, the
    /// connection of connect attempt `attempt`.
    pub(crate) fn open(
        session: SessionKey,
        attempt: u32,
        tcp: TcpStream,
        notify: impl Notify,
    ) -> io::Result<RtspIo> {
        let _ = tcp.set_nodelay(true);
        tcp.set_write_timeout(Some(RTSP_WRITE_TIMEOUT))?;
        tcp.set_read_timeout(Some(POLL_INTERVAL))?;
        let reader = tcp.try_clone()?;
        let writer_stream = tcp.try_clone()?;
        let (writer, writes) = mpsc::channel();
        let mut io = RtspIo {
            tcp: Some(tcp),
            writer: Some(writer),
            backlog: Arc::new(Backlog::default()),
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        // On a failed spawn, dropping `io` stops the thread already running.
        let thread = spawn(
            "airplay-pair-read",
            read_loop(session, attempt, reader, io.stop.clone(), notify.clone()),
            report_closed(session, attempt, notify.clone()),
        )?;
        io.threads.push(thread);
        let thread = spawn(
            "airplay-pair-write",
            write_loop(
                session,
                attempt,
                writer_stream,
                writes,
                io.backlog.clone(),
                io.stop.clone(),
                notify.clone(),
            ),
            report_closed(session, attempt, notify),
        )?;
        io.threads.push(thread);
        Ok(io)
    }

    /// Queues bytes for the connection; the writer thread writes them in
    /// order.
    pub(crate) fn write(&self, bytes: Vec<u8>) {
        if let Some(writer) = &self.writer {
            self.backlog.add(bytes.len());
            let _ = writer.send(bytes);
        }
    }

    /// Queues bytes unless the unwritten backlog would pass `limit`, for a
    /// connection whose writes answer the peer, so a peer that reads slowly
    /// can't grow the queue without end.
    pub(crate) fn write_bounded(&self, bytes: Vec<u8>, limit: BacklogLimit) -> Queued {
        let Some(writer) = &self.writer else {
            return Queued::Dropped;
        };
        if self.backlog.overflowed.load(Ordering::Acquire) {
            return Queued::Dropped;
        }
        if !self.backlog.admit(bytes.len(), limit) {
            self.backlog.overflowed.store(true, Ordering::Release);
            // The stop flag stays clear, so the reader reports the close.
            if let Some(tcp) = &self.tcp {
                let _ = tcp.shutdown(Shutdown::Both);
            }
            return Queued::Overflowed;
        }
        let _ = writer.send(bytes);
        Queued::Yes
    }

    /// Closes the connection and stops the threads, whose handles come back
    /// for the caller to join.
    pub(crate) fn close(mut self) -> Vec<JoinHandle<()>> {
        self.stop_all();
        std::mem::take(&mut self.threads)
    }

    fn stop_all(&mut self) {
        let Some(tcp) = &self.tcp else {
            return;
        };
        self.stop.store(true, Ordering::Release);
        self.writer = None;
        let _ = tcp.shutdown(Shutdown::Both);
    }
}

impl Drop for RtspIo {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// The PTP service's sockets on ports 319 and 320 (P158), bound only while
/// the AirPlay 2 switch is on and an AirPlay 2 session may need them. One
/// thread reads the event socket and hands what may be a Delay_Req from a
/// peer to the driver with its arrival time; nothing else is read.
#[derive(Debug)]
pub(crate) struct PtpIo {
    event: Arc<UdpSocket>,
    general: UdpSocket,
    /// The receivers the service serves, as [`PtpIo::set_peers`] last named
    /// them; the thread drops datagrams from anyone else (P162).
    peers: Arc<RwLock<Vec<IpAddr>>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl PtpIo {
    /// Binds both ports on [`crate::ptp::service_bind_ip`]; either bind
    /// failing makes PTP unavailable (P158).
    pub(crate) fn open(
        family: IpAddr,
        ports: crate::ptp::PtpPorts,
        notify: impl Notify,
    ) -> io::Result<PtpIo> {
        let ip = crate::ptp::service_bind_ip(family);
        let event = UdpSocket::bind(SocketAddr::new(ip, ports.event))?;
        let general = UdpSocket::bind(SocketAddr::new(ip, ports.general))?;
        event.set_read_timeout(Some(UDP_BACKSTOP))?;
        for socket in [&event, &general] {
            socket.set_write_timeout(Some(UDP_SEND_TIMEOUT))?;
        }
        let mut io = PtpIo {
            event: Arc::new(event),
            general,
            peers: Arc::new(RwLock::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        let thread = spawn(
            "airplay-ptp",
            ptp_loop(io.event.clone(), io.peers.clone(), io.stop.clone(), notify),
            || {},
        )?;
        io.threads.push(thread);
        Ok(io)
    }

    /// Names the receivers whose datagrams the thread passes on, replacing
    /// the earlier list.
    pub(crate) fn set_peers(&self, peers: impl IntoIterator<Item = IpAddr>) {
        let mut known = self.peers.write().unwrap_or_else(PoisonError::into_inner);
        known.clear();
        known.extend(peers.into_iter().map(|peer| peer.to_canonical()));
    }

    /// Sends one message; a failed send is dropped, since the next Sync
    /// follows within 125 ms.
    pub(crate) fn send(&self, port: crate::ptp::PtpPort, to: SocketAddr, bytes: &[u8]) {
        let socket = match port {
            crate::ptp::PtpPort::Event => &*self.event,
            crate::ptp::PtpPort::General => &self.general,
        };
        let _ = socket.send_to(bytes, to);
    }

    /// Stops the thread, whose handle comes back for the caller to join;
    /// the sockets close when the thread lets go of its own.
    pub(crate) fn close(mut self) -> Vec<JoinHandle<()>> {
        self.stop_all();
        std::mem::take(&mut self.threads)
    }

    fn stop_all(&mut self) {
        if !self.stop.swap(true, Ordering::AcqRel) {
            wake(&self.event);
        }
    }
}

impl Drop for PtpIo {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Sends `socket` an empty datagram from itself, so a receive blocked on it
/// returns and its thread sees the stop flag. A socket bound to every
/// address is reached over loopback. When the send fails (the local
/// address is gone) the thread ends at its [`UDP_BACKSTOP`] instead.
fn wake(socket: &UdpSocket) {
    #[cfg(test)]
    if tests::WAKE_FAILS.with(std::cell::Cell::get) {
        return;
    }
    let Ok(mut address) = socket.local_addr() else {
        return;
    };
    match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => address.set_ip(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() => address.set_ip(Ipv6Addr::LOCALHOST.into()),
        _ => {}
    }
    let _ = socket.send_to(&[], address);
}

fn ptp_loop(
    socket: Arc<UdpSocket>,
    peers: Arc<RwLock<Vec<IpAddr>>>,
    stop: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buf = [0u8; DATAGRAM_BYTES];
        while !stop.load(Ordering::Acquire) {
            #[cfg(test)]
            let _receiving = tests::Receiving::enter(&socket);
            match socket.recv_from(&mut buf) {
                Ok((0, _)) => {}
                Ok((len, source)) => {
                    let at = Instant::now();
                    let from_peer = peers
                        .read()
                        .unwrap_or_else(PoisonError::into_inner)
                        .iter()
                        .any(|&peer| from_receiver(source, peer));
                    if !from_peer || !crate::ptp::may_be_delay_req(&buf[..len]) {
                        continue;
                    }
                    let bytes = buf[..len].to_vec();
                    if !notify(IoEvent::Ptp { bytes, source, at }) {
                        break;
                    }
                }
                Err(error) => idle_after(&error),
            }
        }
    }
}

/// Whether a datagram from `source` came from the session's receiver,
/// whose address (in canonical form) is `receiver`, from any port, since
/// receivers pick their own (P86, P91). An IPv4 or IPv6 link-local address
/// must match exactly. Any other IPv6 address only needs the same /64
/// prefix: a receiver may send from one of its temporary addresses
/// (RFC 8981), which keeps the prefix and changes the rest.
pub(crate) fn from_receiver(source: SocketAddr, receiver: IpAddr) -> bool {
    match (source.ip().to_canonical(), receiver) {
        (IpAddr::V6(source), IpAddr::V6(receiver)) if !receiver.is_unicast_link_local() => {
            source.octets()[..8] == receiver.octets()[..8]
        }
        (source, receiver) => source == receiver,
    }
}

/// Answers timing requests from `receiver` (P90, P91, P92): the receive
/// time is read right after the receive call, the transmit time right
/// before the send call, and nothing slow happens between them. Anything
/// that isn't a request is ignored; a datagram from another address is
/// counted and dropped.
fn timing_loop(
    session: SessionKey,
    socket: Arc<UdpSocket>,
    receiver: IpAddr,
    clock: SenderClock,
    counters: Arc<SpeakerCounters>,
    stop: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buf = [0u8; DATAGRAM_BYTES];
        while !stop.load(Ordering::Acquire) {
            #[cfg(test)]
            let _receiving = tests::Receiving::enter(&socket);
            match socket.recv_from(&mut buf) {
                Ok((0, _)) => {}
                Ok((len, source)) => {
                    let received = Instant::now();
                    if !from_receiver(source, receiver) {
                        counters.count_foreign_datagram();
                        continue;
                    }
                    let transmit = Instant::now();
                    let Some(reply) =
                        rtp::timing_reply(&buf[..len], clock.ntp(received), clock.ntp(transmit))
                    else {
                        continue;
                    };
                    if socket.send_to(&reply, source).is_ok() {
                        counters.count_timing_reply();
                    }
                    if !notify(IoEvent::TimingRequest {
                        session,
                        at: received,
                    }) {
                        break;
                    }
                }
                Err(error) => idle_after(&error),
            }
        }
    }
}

/// Hands retransmission requests from `receiver` to the driver (P86); other
/// datagrams are ignored (P92), and those from another address counted. A
/// reset on a receive is an ICMP unreachable for an earlier send, which the
/// next send reports (P141).
fn control_loop(
    session: SessionKey,
    socket: Arc<UdpSocket>,
    receiver: IpAddr,
    counters: Arc<SpeakerCounters>,
    stop: Arc<AtomicBool>,
    reset: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buf = [0u8; DATAGRAM_BYTES];
        while !stop.load(Ordering::Acquire) {
            #[cfg(test)]
            let _receiving = tests::Receiving::enter(&socket);
            match socket.recv_from(&mut buf) {
                Ok((0, _)) => {}
                Ok((len, source)) => {
                    if !from_receiver(source, receiver) {
                        counters.count_foreign_datagram();
                        continue;
                    }
                    if ResendRequest::parse(&buf[..len]).is_none() {
                        continue;
                    }
                    let bytes = buf[..len].to_vec();
                    if !notify(IoEvent::Control { session, bytes }) {
                        break;
                    }
                }
                Err(error) => {
                    if error.kind() == io::ErrorKind::ConnectionReset {
                        reset.store(true, Ordering::Release);
                    }
                    idle_after(&error);
                }
            }
        }
    }
}

/// After a failed receive: the backstop timeout or an interrupted one just
/// loops back to the stop flag; a reset (Windows reports one on the next
/// receive after an ICMP unreachable) or any other error pauses briefly so
/// a broken socket can't spin.
fn idle_after(error: &io::Error) {
    if !matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    ) {
        thread::sleep(Duration::from_millis(5));
    }
}

/// Reads the RTSP connection until it closes (P141).
fn read_loop(
    session: SessionKey,
    attempt: u32,
    mut stream: TcpStream,
    stop: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buf = vec![0u8; READ_BYTES];
        loop {
            match read_queued(&mut stream, &mut buf) {
                // Our own close shuts the connection down too: that end is
                // not the receiver's and reaches the driver as nothing.
                Ok(0) => {
                    if !stop.load(Ordering::Acquire) {
                        notify(IoEvent::RtspClosed { session, attempt });
                    }
                    break;
                }
                Ok(len) => {
                    let bytes = buf[..len].to_vec();
                    if !notify(IoEvent::Rtsp {
                        session,
                        attempt,
                        bytes,
                    }) {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                }
                Err(_) => {
                    if !stop.load(Ordering::Acquire) {
                        notify(IoEvent::RtspClosed { session, attempt });
                    }
                    break;
                }
            }
        }
    }
}

/// Reads from `stream` only once a peek within its read timeout shows data
/// or the end of the stream, so a timeout never cuts into a read (see the
/// module notes).
fn read_queued(stream: &mut TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    if stream.peek(buf).map_err(peek_error)? == 0 {
        return Ok(0);
    }
    stream.read(buf)
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

/// Writes the session's requests in order until the session closes, in
/// pieces, looking at the stop flag between them.
fn write_loop(
    session: SessionKey,
    attempt: u32,
    mut stream: TcpStream,
    writes: mpsc::Receiver<Vec<u8>>,
    backlog: Arc<Backlog>,
    stop: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        for bytes in writes {
            for piece in bytes.chunks(WRITE_BYTES) {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if stream.write_all(piece).is_err() {
                    notify(IoEvent::RtspClosed { session, attempt });
                    return;
                }
            }
            backlog.written(bytes.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const WAIT: Duration = Duration::from_secs(2);

    thread_local! {
        /// Makes [`wake`] on this thread send nothing, as when the local
        /// address is gone.
        pub(super) static WAKE_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// The local ports of the UDP sockets whose thread is past its stop
    /// check, so it ends only once its receive returns.
    static RECEIVING: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

    /// Marks a UDP thread's socket as receiving for one loop pass.
    pub(super) struct Receiving(Option<u16>);

    impl Receiving {
        pub(super) fn enter(socket: &UdpSocket) -> Self {
            let port = socket.local_addr().ok().map(|address| address.port());
            if let Some(port) = port {
                RECEIVING
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(port);
            }
            Receiving(port)
        }
    }

    impl Drop for Receiving {
        fn drop(&mut self) {
            let Some(port) = self.0 else { return };
            let mut ports = RECEIVING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(at) = ports.iter().position(|&p| p == port) {
                ports.swap_remove(at);
            }
        }
    }

    /// Waits until the threads of the sockets on `ports` are all receiving,
    /// or `WAIT` passes.
    fn all_receiving(ports: &[u16]) -> bool {
        let deadline = Instant::now() + WAIT;
        loop {
            {
                let receiving = RECEIVING
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if ports.iter().all(|port| receiving.contains(port)) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
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

    /// A driver that panics on the events `bug` picks, as a bug in a
    /// thread's work would, and passes every other event on.
    fn panicking_on(bug: fn(&IoEvent) -> bool) -> (impl Notify, mpsc::Receiver<IoEvent>) {
        let (events, received) = mpsc::channel();
        let notify = move |event: IoEvent| {
            if bug(&event) {
                panic!("a bug in a thread's work (test)");
            }
            events.send(event).is_ok()
        };
        (notify, received)
    }

    // A connect thread whose work panics reports a failed connect at once.
    #[test]
    fn a_panicking_connect_thread_reports_a_failed_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let (notify, received) = panicking_on(|event| matches!(event, IoEvent::Connected { .. }));
        connect(SessionKey(3), 7, address, notify).expect("thread");
        match received.recv_timeout(WAIT) {
            Ok(IoEvent::ConnectFailed { session, attempt }) => {
                assert_eq!((session, attempt), (SessionKey(3), 7));
            }
            other => panic!("expected a failed connect, got {other:?}"),
        }
    }

    // A session thread whose work panics reports its connection closed at
    // once, long before a session timeout, while the connection itself
    // stays open.
    #[test]
    fn a_panicking_session_thread_reports_the_connection_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let tcp = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (mut receiver, _) = listener.accept().expect("accept");
        let (notify, received) = panicking_on(|event| matches!(event, IoEvent::Rtsp { .. }));
        let (io, _connection) = SessionIo::open(
            SessionKey(5),
            2,
            tcp,
            SenderClock::new(Instant::now()),
            Arc::new(SpeakerCounters::default()),
            notify,
        )
        .expect("session I/O");
        receiver.write_all(b"RTSP/1.0 200 OK\r\n").expect("write");
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match received.recv_timeout(left) {
                Ok(IoEvent::RtspClosed { session, attempt }) => {
                    assert_eq!((session, attempt), (SessionKey(5), 2));
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("no closed connection: {error:?}"),
            }
        }
        drop(io);
    }

    /// Collects the RTSP bytes `received` reports until `want` bytes came.
    fn rtsp_bytes(received: &mpsc::Receiver<IoEvent>, want: usize) -> Vec<u8> {
        let deadline = Instant::now() + WAIT;
        let mut bytes = Vec::new();
        while bytes.len() < want {
            let left = deadline.saturating_duration_since(Instant::now());
            match received.recv_timeout(left) {
                Ok(IoEvent::Rtsp { bytes: piece, .. }) => bytes.extend_from_slice(&piece),
                Ok(_) => {}
                Err(error) => panic!("only {} of {want} bytes: {error:?}", bytes.len()),
            }
        }
        bytes
    }

    // A response whose halves straddle the reader's poll timeout arrives
    // whole, however often the wait between them times out.
    #[test]
    fn rtsp_bytes_split_across_read_timeouts_arrive_intact() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let tcp = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (mut receiver, _) = listener.accept().expect("accept");
        let (notify, received) = panicking_on(|_| false);
        let io = RtspIo::open(SessionKey(7), 1, tcp, notify).expect("pairing I/O");
        let first = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\n";
        let second = b"Content-Length: 0\r\n\r\n";
        for pause in [1, 2, 3] {
            receiver.write_all(first).expect("write");
            thread::sleep(POLL_INTERVAL * pause + Duration::from_millis(5));
            receiver.write_all(second).expect("write");
            let bytes = rtsp_bytes(&received, first.len() + second.len());
            assert_eq!(bytes, [&first[..], &second[..]].concat());
        }
        drop(io);
    }

    // Closing ends a reader waiting on a quiet connection within its poll
    // timeout, without reporting the close as the receiver's doing.
    #[test]
    fn closing_ends_the_waiting_rtsp_reader() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let tcp = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (_receiver, _) = listener.accept().expect("accept");
        let (notify, received) = panicking_on(|_| false);
        let io = RtspIo::open(SessionKey(8), 1, tcp, notify).expect("pairing I/O");
        thread::sleep(POLL_INTERVAL * 2);
        let started = Instant::now();
        for thread in io.close() {
            thread.join().expect("join");
        }
        assert!(started.elapsed() < WAIT, "took {:?}", started.elapsed());
        assert!(
            !received
                .try_iter()
                .any(|event| matches!(event, IoEvent::RtspClosed { .. }))
        );
    }

    // Answers that never drain are admitted up to the limit and no further,
    // by count for small ones and by bytes for large ones.
    #[test]
    fn a_backlog_admits_up_to_its_limit_only() {
        let small = Backlog::default();
        let admitted = (0..10_000)
            .filter(|_| small.admit(120, EVENT_BACKLOG))
            .count();
        assert_eq!(admitted, EVENT_BACKLOG.messages);
        assert_eq!(
            small.queued(),
            (EVENT_BACKLOG.messages, 120 * EVENT_BACKLOG.messages)
        );
        let large = Backlog::default();
        let admitted = (0..10_000)
            .filter(|_| large.admit(64 * 1024, EVENT_BACKLOG))
            .count();
        assert_eq!(admitted, EVENT_BACKLOG.bytes / (64 * 1024));
        small.written(120);
        assert!(small.admit(120, EVENT_BACKLOG));
    }

    // A receiver that stops reading its answers has its connection shut
    // down once the backlog passes the limit, and the close is reported as
    // the receiver's, so the session takes its usual closed path.
    #[test]
    fn an_unread_backlog_past_its_limit_closes_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let tcp = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (_receiver, _) = listener.accept().expect("accept");
        let (notify, received) = panicking_on(|_| false);
        let io = RtspIo::open(SessionKey(9), 1, tcp, notify).expect("event I/O");
        let limit = BacklogLimit {
            messages: 8,
            bytes: 1024 * 1024,
        };
        let answer = vec![0u8; 64 * 1024];
        let mut refused = Vec::new();
        for _ in 0..10_000 {
            let queued = io.write_bounded(answer.clone(), limit);
            let (messages, bytes) = io.backlog.queued();
            assert!(messages <= limit.messages, "{messages} messages queued");
            assert!(bytes <= limit.bytes, "{bytes} bytes queued");
            if queued != Queued::Yes {
                refused.push(queued);
                if refused.len() == 2 {
                    break;
                }
            }
        }
        assert_eq!(refused, [Queued::Overflowed, Queued::Dropped]);
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match received.recv_timeout(left) {
                Ok(IoEvent::RtspClosed { session, attempt }) => {
                    assert_eq!((session, attempt), (SessionKey(9), 1));
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("no close reported: {error:?}"),
            }
        }
        drop(io);
    }

    /// A timing request with sequence number `seq` (P90).
    fn timing_request(seq: u16) -> [u8; 32] {
        rtp::TimingPacket {
            reply: false,
            seq,
            origin: crate::clock::NtpTime(0),
            receive: crate::clock::NtpTime(0),
            transmit: crate::clock::NtpTime(0x0102_0304_0506_0708),
        }
        .encode()
    }

    /// A retransmission request for `count` packets from `first` (P86).
    fn resend_request(first: u16, count: u16) -> [u8; 8] {
        ResendRequest {
            seq: 1,
            first,
            count,
        }
        .encode()
    }

    // A host other than the receiver gets no timing reply, and neither its
    // timing nor its retransmission requests reach the driver, so it can't
    // keep a session's timing alive (P131) or draw resends; the receiver's
    // own requests, from any port, still do (P86, P91). The other host
    // sends from 127.0.0.2, which Windows and Linux route to loopback;
    // macOS has only 127.0.0.1 unless configured, so there the test is
    // skipped and the next one covers the rule.
    // covers: P86, P91, P144
    #[test]
    fn datagrams_from_another_address_never_reach_the_driver() {
        let Ok(stranger) = UdpSocket::bind("127.0.0.2:0") else {
            eprintln!("skipped: 127.0.0.2 is not a local address here");
            return;
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let tcp = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (_receiver_end, _) = listener.accept().expect("accept");
        let (events, received) = mpsc::channel();
        let notify = move |event: IoEvent| events.send(event).is_ok();
        let counters = Arc::new(SpeakerCounters::default());
        let (io, connection) = SessionIo::open(
            SessionKey(9),
            1,
            tcp,
            SenderClock::new(Instant::now()),
            counters.clone(),
            notify,
        )
        .expect("session I/O");
        let Connection {
            local: sender,
            control_port,
            timing_port,
        } = connection;
        let timing = SocketAddr::new(sender, timing_port);
        let control = SocketAddr::new(sender, control_port);
        let receiver = UdpSocket::bind("127.0.0.1:0").expect("receiver socket");
        receiver.set_read_timeout(Some(WAIT)).expect("timeout");

        // The stranger's datagrams go first, so the sender's threads have
        // dealt with them by the time the receiver's own are answered.
        stranger.send_to(&timing_request(1), timing).expect("send");
        stranger
            .send_to(&resend_request(100, 1), control)
            .expect("send");
        receiver.send_to(&timing_request(2), timing).expect("send");
        receiver
            .send_to(&resend_request(200, 1), control)
            .expect("send");

        let mut buf = [0u8; 64];
        let (len, _) = receiver.recv_from(&mut buf).expect("the receiver's reply");
        let reply = rtp::TimingPacket::parse(&buf[..len]).expect("a timing reply");
        assert_eq!(reply.seq, 2);
        let mut timing_events = 0;
        let mut resends = Vec::new();
        let deadline = Instant::now() + WAIT;
        while resends.is_empty() && Instant::now() < deadline {
            match received.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(IoEvent::TimingRequest { .. }) => timing_events += 1,
                Ok(IoEvent::Control { bytes, .. }) => {
                    let request = ResendRequest::parse(&bytes).expect("a resend request");
                    resends.push(request.first);
                }
                Ok(_) | Err(_) => {}
            }
        }
        // Anything still on its way from the stranger was sent before the
        // receiver's own datagrams and is late by now.
        while let Ok(event) = received.recv_timeout(Duration::from_millis(100)) {
            match event {
                IoEvent::TimingRequest { .. } => timing_events += 1,
                IoEvent::Control { bytes, .. } => {
                    resends.extend(ResendRequest::parse(&bytes).map(|r| r.first));
                }
                _ => {}
            }
        }
        assert_eq!(timing_events, 1, "only the receiver's timing request");
        assert_eq!(resends, [200], "only the receiver's resend request");
        assert_eq!(counters.foreign_datagrams(), 2, "the stranger's, counted");
        assert_eq!(counters.timing_replies(), 1);
        stranger
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        assert!(
            stranger.recv_from(&mut buf).is_err(),
            "no timing reply to the stranger"
        );
        drop(io);
    }

    // Which source addresses count as the receiver: any port; IPv4 and
    // IPv6 link-local exactly, IPv4-mapped IPv6 as IPv4; other IPv6 within
    // the same /64, so a temporary address of the receiver's passes.
    // covers: P86, P91
    #[test]
    fn the_receiver_is_its_address_from_any_port() {
        let from = |source: &str, receiver: &str| {
            let source: SocketAddr = source.parse().expect("source");
            let receiver: IpAddr = receiver.parse().expect("receiver");
            from_receiver(source, receiver)
        };
        assert!(from("192.0.2.7:6002", "192.0.2.7"));
        assert!(from("192.0.2.7:51000", "192.0.2.7"));
        assert!(!from("192.0.2.8:6002", "192.0.2.7"));
        assert!(from("[::ffff:192.0.2.7]:6002", "192.0.2.7"));
        assert!(!from("[::ffff:192.0.2.8]:6002", "192.0.2.7"));
        assert!(from("[fe80::1%3]:6002", "fe80::1"));
        assert!(!from("[fe80::2%3]:6002", "fe80::1"));
        assert!(from("[2001:db8:1:2::10]:6002", "2001:db8:1:2::10"));
        assert!(from(
            "[2001:db8:1:2:a1b2:c3d4:e5f6:789]:6002",
            "2001:db8:1:2::10"
        ));
        assert!(!from("[2001:db8:1:3::10]:6002", "2001:db8:1:2::10"));
        assert!(!from("[fe80::10]:6002", "2001:db8:1:2::10"));
        assert!(!from("192.0.2.7:6002", "2001:db8:1:2::10"));
        assert!(!from("[::1]:6002", "127.0.0.1"));
    }

    // The rule on any system: with the receiver at an address no local
    // socket has (192.0.2.1, reserved for documentation), every datagram
    // from loopback is another host's. Both UDP threads count it, the
    // timing thread doesn't answer it, and nothing reaches the driver.
    // covers: P86, P91, P144
    #[test]
    fn the_udp_threads_count_and_drop_datagrams_from_another_address() {
        let timing = Arc::new(UdpSocket::bind("127.0.0.1:0").expect("timing socket"));
        let control = Arc::new(UdpSocket::bind("127.0.0.1:0").expect("control socket"));
        let receiver = IpAddr::from([192, 0, 2, 1]);
        let counters = Arc::new(SpeakerCounters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (events, received) = mpsc::channel();
        let notify = move |event: IoEvent| events.send(event).is_ok();
        let threads = [
            thread::spawn(timing_loop(
                SessionKey(4),
                timing.clone(),
                receiver,
                SenderClock::new(Instant::now()),
                counters.clone(),
                stop.clone(),
                notify.clone(),
            )),
            thread::spawn(control_loop(
                SessionKey(4),
                control.clone(),
                receiver,
                counters.clone(),
                stop.clone(),
                Arc::new(AtomicBool::new(false)),
                notify,
            )),
        ];
        let stranger = UdpSocket::bind("127.0.0.1:0").expect("stranger");
        let to = |socket: &UdpSocket| socket.local_addr().expect("address");
        stranger
            .send_to(&timing_request(1), to(&timing))
            .expect("send");
        stranger
            .send_to(&resend_request(100, 1), to(&control))
            .expect("send");
        let deadline = Instant::now() + WAIT;
        while counters.foreign_datagrams() < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Release);
        wake(&timing);
        wake(&control);
        for thread in threads {
            thread.join().expect("a UDP thread");
        }
        assert_eq!(counters.foreign_datagrams(), 2, "the wake-ups uncounted");
        assert_eq!(counters.timing_replies(), 0);
        assert!(received.try_recv().is_err(), "nothing reached the driver");
        stranger.set_nonblocking(true).expect("nonblocking");
        assert!(
            stranger.recv_from(&mut [0u8; 64]).is_err(),
            "no timing reply"
        );
    }

    /// Waits until every thread in `threads` has ended, or `WAIT` passes.
    fn all_end(threads: &[JoinHandle<()>]) -> bool {
        all_end_within(threads, WAIT)
    }

    fn all_end_within(threads: &[JoinHandle<()>], wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while threads.iter().any(|thread| !thread.is_finished()) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
        true
    }

    // The UDP sockets receive with only a long backstop timeout, so a
    // request rarely lands as a timed receive gives up (on Windows that
    // drops it): every request of repeated bursts is answered or handed on
    // exactly once. Closing still ends every thread at once, woken by
    // datagrams that aren't counted.
    // covers: P86, P91, P144
    #[test]
    fn no_request_of_a_burst_is_lost_and_closing_wakes_the_udp_threads() {
        const BURSTS: u16 = 20;
        const PER_BURST: u16 = 50;
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let tcp = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (_receiver_end, _) = listener.accept().expect("accept");
        let (events, received) = mpsc::channel();
        let notify = move |event: IoEvent| events.send(event).is_ok();
        let counters = Arc::new(SpeakerCounters::default());
        let (io, connection) = SessionIo::open(
            SessionKey(6),
            1,
            tcp,
            SenderClock::new(Instant::now()),
            counters.clone(),
            notify,
        )
        .expect("session I/O");
        for socket in [&io.control, &io.timing] {
            let timeout = socket.read_timeout().expect("timeout").expect("a backstop");
            assert!(timeout >= Duration::from_secs(1), "{timeout:?}");
        }
        let Connection {
            local: sender,
            control_port,
            timing_port,
        } = connection;
        let timing = SocketAddr::new(sender, timing_port);
        let control = SocketAddr::new(sender, control_port);
        let receiver = UdpSocket::bind("127.0.0.1:0").expect("receiver socket");
        receiver.set_read_timeout(Some(WAIT)).expect("timeout");
        let mut buf = [0u8; 64];
        for burst in 0..BURSTS {
            let seqs = burst * PER_BURST..(burst + 1) * PER_BURST;
            for seq in seqs.clone() {
                receiver
                    .send_to(&timing_request(seq), timing)
                    .expect("send");
                receiver
                    .send_to(&resend_request(seq, 1), control)
                    .expect("send");
            }
            let mut replies = Vec::new();
            for _ in seqs.clone() {
                let (len, _) = receiver.recv_from(&mut buf).expect("a timing reply");
                let reply = rtp::TimingPacket::parse(&buf[..len]).expect("a timing reply");
                replies.push(reply.seq);
            }
            let (mut timing_events, mut resends) = (0, Vec::new());
            while timing_events < PER_BURST || resends.len() < usize::from(PER_BURST) {
                match received.recv_timeout(WAIT).expect("an event") {
                    IoEvent::TimingRequest { .. } => timing_events += 1,
                    IoEvent::Control { bytes, .. } => {
                        let request = ResendRequest::parse(&bytes).expect("a resend request");
                        resends.push(request.first);
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            replies.sort_unstable();
            resends.sort_unstable();
            let sent: Vec<u16> = seqs.collect();
            assert_eq!(replies, sent, "every timing request of burst {burst}");
            assert_eq!(resends, sent, "every resend request of burst {burst}");
        }
        let threads = io.close();
        assert!(all_end(&threads), "closing wakes every thread");
        assert_eq!(counters.timing_replies(), u64::from(BURSTS * PER_BURST));
        assert_eq!(counters.foreign_datagrams(), 0, "the wake-ups uncounted");
        assert!(
            received.try_recv().is_err(),
            "nothing more reached the driver"
        );
    }

    // The PTP event socket receives with the long backstop too, and closing the
    // service, or dropping it, wakes its thread over loopback although the
    // socket is bound to every address.
    // covers: P158
    #[test]
    fn closing_the_ptp_service_wakes_its_thread() {
        let ports = crate::ptp::PtpPorts {
            event: 0,
            general: 0,
        };
        let notify = |_: IoEvent| true;
        let io = PtpIo::open(IpAddr::from([127, 0, 0, 1]), ports, notify).expect("PTP I/O");
        let timeout = io
            .event
            .read_timeout()
            .expect("timeout")
            .expect("a backstop");
        assert!(timeout >= Duration::from_secs(1), "{timeout:?}");
        let threads = io.close();
        assert!(all_end(&threads), "closing wakes the PTP thread");
        let mut io = PtpIo::open(IpAddr::from([127, 0, 0, 1]), ports, notify).expect("PTP I/O");
        let threads = std::mem::take(&mut io.threads);
        drop(io);
        assert!(all_end(&threads), "dropping wakes the PTP thread");
    }

    // Port 319 listens on every address, so a LAN host could otherwise fill
    // the driver's channel with Delay_Req-shaped datagrams.
    // covers: P162
    #[test]
    fn the_ptp_thread_passes_on_datagrams_from_peers_only() {
        let ports = crate::ptp::PtpPorts {
            event: 0,
            general: 0,
        };
        let (events, received) = mpsc::channel();
        let notify = move |event: IoEvent| events.send(event).is_ok();
        let io = PtpIo::open(IpAddr::from([127, 0, 0, 1]), ports, notify).expect("PTP I/O");
        let port = io.event.local_addr().expect("event address").port();
        let to = SocketAddr::from(([127, 0, 0, 1], port));
        let mut delay_req = [0u8; 44];
        delay_req[0] = 0x01;
        delay_req[1] = 0x02;
        assert!(crate::ptp::may_be_delay_req(&delay_req));
        let stranger = UdpSocket::bind("127.0.0.1:0").expect("socket");
        io.set_peers([IpAddr::from([192, 0, 2, 7])]);
        stranger.send_to(&delay_req, to).expect("send");
        assert!(
            received.recv_timeout(Duration::from_millis(300)).is_err(),
            "a datagram from a non-peer is dropped in the thread"
        );
        io.set_peers([IpAddr::from([127, 0, 0, 1])]);
        stranger.send_to(&delay_req, to).expect("send");
        match received.recv_timeout(WAIT) {
            Ok(IoEvent::Ptp { source, .. }) => {
                assert_eq!(source.ip(), IpAddr::from([127, 0, 0, 1]));
            }
            other => panic!("{other:?}"),
        }
        let _ = io.close();
    }

    // A close whose wake-up can't be sent, as when the session's local
    // address went away with the network, still ends the session's UDP
    // threads and the PTP thread within the backstop, not never.
    // covers: P86, P158
    #[test]
    fn udp_threads_end_within_the_backstop_when_the_wake_up_cant_be_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let tcp = TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (_receiver_end, _) = listener.accept().expect("accept");
        let notify = |_: IoEvent| true;
        let (io, connection) = SessionIo::open(
            SessionKey(7),
            1,
            tcp,
            SenderClock::new(Instant::now()),
            Arc::new(SpeakerCounters::default()),
            notify,
        )
        .expect("session I/O");
        let ports = crate::ptp::PtpPorts {
            event: 0,
            general: 0,
        };
        let ptp = PtpIo::open(IpAddr::from([127, 0, 0, 1]), ports, notify).expect("PTP I/O");
        // A thread that sees the stop flag before its first receive ends at
        // once, which proves nothing about the backstop.
        let event_port = ptp.event.local_addr().expect("event address").port();
        assert!(
            all_receiving(&[connection.control_port, connection.timing_port, event_port]),
            "every UDP thread reaches its receive"
        );
        WAKE_FAILS.with(|fails| fails.set(true));
        let mut threads = io.close();
        threads.extend(ptp.close());
        WAKE_FAILS.with(|fails| fails.set(false));
        assert!(
            !all_end_within(&threads, Duration::from_millis(200)),
            "no wake-up was sent, so the UDP threads wait for the backstop"
        );
        let within = 2 * UDP_BACKSTOP;
        assert!(
            all_end_within(&threads, within),
            "every thread ends within the backstop"
        );
    }
}
