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
use std::net::{IpAddr, Shutdown, SocketAddr, SocketAddrV6, TcpStream, UdpSocket};
use std::panic::{self, AssertUnwindSafe};
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
            report_closed(session, attempt, notify.clone()),
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
            report_closed(session, attempt, notify.clone()),
        )?;
        io.threads.push(thread);
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
                io.stop.clone(),
                notify.clone(),
            ),
            report_closed(session, attempt, notify),
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

/// The connection of a pairing (P47): an RTSP reader and writer on their
/// own threads, as in a session, without UDP sockets.
#[derive(Debug)]
pub(crate) struct RtspIo {
    tcp: TcpStream,
    writer: Option<mpsc::Sender<Vec<u8>>>,
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
            tcp,
            writer: Some(writer),
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
            let _ = writer.send(bytes);
        }
    }

    /// Closes the connection and stops the threads, whose handles come back
    /// for the caller to join.
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

impl Drop for RtspIo {
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
    attempt: u32,
    mut stream: TcpStream,
    stop: Arc<AtomicBool>,
    notify: impl Notify,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buf = vec![0u8; READ_BYTES];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    notify(IoEvent::RtspClosed { session, attempt });
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
                    notify(IoEvent::RtspClosed { session, attempt });
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
    attempt: u32,
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
                    notify(IoEvent::RtspClosed { session, attempt });
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const WAIT: Duration = Duration::from_secs(2);

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
    // skipped.
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
        let (io, connection) = SessionIo::open(
            SessionKey(9),
            1,
            tcp,
            SenderClock::new(Instant::now()),
            Arc::new(SpeakerCounters::default()),
            notify,
        )
        .expect("session I/O");
        let timing = SocketAddr::new((connection).local, connection.timing_port);
        let control = SocketAddr::new((connection).local, connection.control_port);
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
        stranger
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        assert!(
            stranger.recv_from(&mut buf).is_err(),
            "no timing reply to the stranger"
        );
        drop(io);
    }
}
