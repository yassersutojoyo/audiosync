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
//! Where an ICMP unreachable is reported matters for P141. Windows reports
//! it as a reset on the next call on the socket, which is the control
//! thread's receive, not the send that caused it; the control thread notes
//! it, and the next send from that socket counts as unreachable. Linux and
//! macOS report nothing on an unconnected UDP socket, so there only send
//! errors count.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, SocketAddrV6, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::clock::SenderClock;
use crate::rtp::{self, ResendRequest};
use crate::session::{CONNECT_TIMEOUT, Connection, RESPONSE_TIMEOUT, UdpSendResult};
use crate::stream::{LocalSocket, SpeakerCounters};

/// How often a blocked reader looks at its stop flag: the longest a thread
/// outlives its session's close.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);
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
    /// Bytes the receiver wrote on the RTSP connection.
    Rtsp { session: SessionKey, bytes: Vec<u8> },
    /// The RTSP connection closed or failed (P141).
    RtspClosed { session: SessionKey },
    /// A retransmission request on the control socket (P86).
    Control { session: SessionKey, bytes: Vec<u8> },
    /// A timing request arrived at `at` and was answered (P91, P131).
    TimingRequest { session: SessionKey, at: Instant },
}

/// How the threads reach the driver: false once it is gone, which ends the
/// thread.
pub(crate) trait Notify: Fn(IoEvent) -> bool + Send + Clone + 'static {}

impl<T: Fn(IoEvent) -> bool + Send + Clone + 'static> Notify for T {}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_owned()).spawn(body)
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
    spawn("airplay-connect", move || {
        let event = match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => IoEvent::Connected {
                session,
                attempt,
                stream,
            },
            Err(_) => IoEvent::ConnectFailed { session, attempt },
        };
        notify(event);
    })
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

/// The sockets and threads of one session.
#[derive(Debug)]
pub(crate) struct SessionIo {
    tcp: TcpStream,
    writer: Option<mpsc::Sender<Vec<u8>>>,
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
        tcp: TcpStream,
        clock: SenderClock,
        counters: Arc<SpeakerCounters>,
        notify: impl Notify,
    ) -> io::Result<(SessionIo, Connection)> {
        // Requests are small and answered one at a time (P20).
        let _ = tcp.set_nodelay(true);
        tcp.set_write_timeout(Some(RTSP_WRITE_TIMEOUT))?;
        tcp.set_read_timeout(Some(POLL_INTERVAL))?;
        let local = tcp.local_addr()?;
        let bind_at = udp_bind_address(local);
        let control = UdpSocket::bind(bind_at)?;
        let timing = UdpSocket::bind(bind_at)?;
        for socket in [&control, &timing] {
            socket.set_read_timeout(Some(POLL_INTERVAL))?;
            socket.set_write_timeout(Some(UDP_SEND_TIMEOUT))?;
        }
        let connection = Connection {
            local: local.ip(),
            control_port: control.local_addr()?.port(),
            timing_port: timing.local_addr()?.port(),
        };
        let reader = tcp.try_clone()?;
        let writer_stream = tcp.try_clone()?;
        let (writer, writes) = mpsc::channel();
        let mut io = SessionIo {
            tcp,
            writer: Some(writer),
            control: Arc::new(control),
            timing: Arc::new(timing),
            stop: Arc::new(AtomicBool::new(false)),
            reset: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        // On a failed spawn, dropping `io` stops the threads already
        // running.
        let thread = spawn(
            "airplay-timing",
            timing_loop(
                session,
                io.timing.clone(),
                clock,
                counters,
                io.stop.clone(),
                notify.clone(),
            ),
        )?;
        io.threads.push(thread);
        let thread = spawn(
            "airplay-control",
            control_loop(
                session,
                io.control.clone(),
                io.stop.clone(),
                io.reset.clone(),
                notify.clone(),
            ),
        )?;
        io.threads.push(thread);
        let thread = spawn(
            "airplay-rtsp-read",
            read_loop(session, reader, io.stop.clone(), notify.clone()),
        )?;
        io.threads.push(thread);
        let thread = spawn(
            "airplay-rtsp-write",
            write_loop(session, writer_stream, writes, io.stop.clone(), notify),
        )?;
        io.threads.push(thread);
        Ok((io, connection))
    }

    /// Queues bytes for the RTSP connection; the writer thread writes them
    /// in order.
    pub(crate) fn write(&self, bytes: Vec<u8>) {
        if let Some(writer) = &self.writer {
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
    /// for the caller to join; every thread ends within [`POLL_INTERVAL`].
    pub(crate) fn close(mut self) -> Vec<JoinHandle<()>> {
        self.stop_all();
        std::mem::take(&mut self.threads)
    }

    fn stop_all(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.writer = None;
        let _ = self.tcp.shutdown(Shutdown::Both);
    }
}

impl Drop for SessionIo {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Answers timing requests (P90, P91, P92): the receive time is read right
/// after the receive call, the transmit time right before the send call,
/// and nothing slow happens between them. Anything that isn't a request is
/// ignored.
fn timing_loop(
    session: SessionKey,
    socket: Arc<UdpSocket>,
    clock: SenderClock,
    counters: Arc<SpeakerCounters>,
    stop: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buf = [0u8; DATAGRAM_BYTES];
        while !stop.load(Ordering::Acquire) {
            match socket.recv_from(&mut buf) {
                Ok((len, source)) => {
                    let received = Instant::now();
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

/// Hands retransmission requests to the driver (P86); other datagrams are
/// ignored (P92). A reset on a receive is an ICMP unreachable for an earlier
/// send, which the next send reports (P141).
fn control_loop(
    session: SessionKey,
    socket: Arc<UdpSocket>,
    stop: Arc<AtomicBool>,
    reset: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buf = [0u8; DATAGRAM_BYTES];
        while !stop.load(Ordering::Acquire) {
            match socket.recv_from(&mut buf) {
                Ok((len, _)) => {
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

/// After a failed receive: a timeout just loops back to the stop flag; a
/// reset (Windows reports one on the next receive after an ICMP
/// unreachable) or any other error pauses briefly so a broken socket can't
/// spin.
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
    mut stream: TcpStream,
    stop: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buf = vec![0u8; READ_BYTES];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    notify(IoEvent::RtspClosed { session });
                    break;
                }
                Ok(len) => {
                    let bytes = buf[..len].to_vec();
                    if !notify(IoEvent::Rtsp { session, bytes }) {
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
                    notify(IoEvent::RtspClosed { session });
                    break;
                }
            }
        }
    }
}

/// Writes the session's requests in order until the session closes, in
/// pieces, looking at the stop flag between them.
fn write_loop(
    session: SessionKey,
    mut stream: TcpStream,
    writes: mpsc::Receiver<Vec<u8>>,
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
                    notify(IoEvent::RtspClosed { session });
                    return;
                }
            }
        }
    }
}
