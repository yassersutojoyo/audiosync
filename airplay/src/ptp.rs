//! The PTP service of the AirPlay 2 path (P158 to P163, P167), sans-I/O:
//! the sender is the unicast grandmaster of its one PTP receiver, or of
//! every member of its one target (P179), sending each Announce once a
//! second and a two-step Sync with its Follow_Up eight times a second, and
//! answering their Delay_Req. Every
//! timestamp is the sender clock read as PTP time (P163). Message layouts
//! follow IEEE 1588-2008 version 2.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use crate::clock::{PtpTime, SenderClock};

/// The event port: Sync and Delay_Req (P158).
pub const EVENT_PORT: u16 = 319;
/// The general port: Announce, Follow_Up and Delay_Resp (P158).
pub const GENERAL_PORT: u16 = 320;
/// Announce once a second (P160).
pub const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);
/// Sync eight times a second (P161).
pub const SYNC_INTERVAL: Duration = Duration::from_millis(125);

/// Bytes of the common message header (IEEE 1588-2008 §13.3).
pub const HEADER_BYTES: usize = 34;
/// Bytes of a timestamp: 48-bit seconds, 32-bit nanoseconds.
pub const TIMESTAMP_BYTES: usize = 10;
pub const SYNC_BYTES: usize = HEADER_BYTES + TIMESTAMP_BYTES;
/// Header, preciseOriginTimestamp, then the 802.1AS TLV (4 + 28 bytes) and
/// Apple's TLV (4 + 14 bytes) (P161).
pub const FOLLOW_UP_BYTES: usize = HEADER_BYTES + TIMESTAMP_BYTES + 32 + 18;
/// Header, the Announce fields (30 bytes) and the path trace TLV (P160).
pub const ANNOUNCE_BYTES: usize = HEADER_BYTES + 30 + 12;
/// Header, receiveTimestamp and requestingPortIdentity (P162).
pub const DELAY_RESP_BYTES: usize = HEADER_BYTES + TIMESTAMP_BYTES + 10;
/// The shortest Delay_Req: header and originTimestamp.
pub const DELAY_REQ_BYTES: usize = HEADER_BYTES + TIMESTAMP_BYTES;

/// transportSpecific 1 in the upper four bits of byte 0 (P159).
const TRANSPORT_SPECIFIC: u8 = 0x10;
const VERSION: u8 = 2;
/// Unicast and PTP timescale (P159).
const FLAGS: u16 = 0x0408;
/// The same with two-step, on Sync and Delay_Resp (P159).
const FLAGS_TWO_STEP: u16 = 0x0608;
/// The port number of the sender's port identity (P159).
const PORT_NUMBER: u16 = 1;
/// logMessageInterval −3: eight a second (P161, P162).
const LOG_INTERVAL_SYNC: u8 = 0xFD;

/// The PTP message types the service sends or reads (IEEE 1588-2008
/// §13.3.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Sync = 0x0,
    DelayReq = 0x1,
    FollowUp = 0x8,
    DelayResp = 0x9,
    Announce = 0xB,
}

/// The controlField of each type (IEEE 1588-2008 Table 23).
fn control_field(kind: MessageType) -> u8 {
    match kind {
        MessageType::Sync => 0,
        MessageType::DelayReq => 1,
        MessageType::FollowUp => 2,
        MessageType::DelayResp => 3,
        MessageType::Announce => 5,
    }
}

/// The sender's clock identity: 0xFF 0xFF, then 6 random bytes (P159).
pub fn clock_identity(random: [u8; 6]) -> [u8; 8] {
    let mut identity = [0xFF; 8];
    identity[2..].copy_from_slice(&random);
    identity
}

/// Which of the service's sockets a message leaves from, and which of the
/// receiver's ports it goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtpPort {
    Event,
    General,
}

/// A PTP message to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtpTransmit {
    pub kind: MessageType,
    pub port: PtpPort,
    pub to: SocketAddr,
    /// The sequenceId, so the driver can name a Sync it sent.
    pub sequence_id: u16,
    pub bytes: Vec<u8>,
}

/// The receiver's ports: 319 and 320 (P158), or others in a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtpPorts {
    pub event: u16,
    pub general: u16,
}

impl Default for PtpPorts {
    fn default() -> PtpPorts {
        PtpPorts {
            event: EVENT_PORT,
            general: GENERAL_PORT,
        }
    }
}

/// Set by CI only: debug builds (the tests) then bind every address as a
/// release does. Unset, they keep every socket on loopback, since Windows
/// Firewall asks about each new test program that binds another address.
pub const LAN_TESTS: &str = "AUDIOSYNC_LAN_TESTS";

/// Whether this build keeps its sockets on loopback: a debug build without
/// [`LAN_TESTS`]. Release builds ignore it and always bind every address.
#[doc(hidden)]
pub fn loopback_only() -> bool {
    cfg!(debug_assertions) && std::env::var_os(LAN_TESTS).is_none_or(|value| value.is_empty())
}

/// The address the PTP service binds for receivers of `family`'s kind: every
/// address of that kind (P158), or its loopback in a [`loopback_only`] build.
#[doc(hidden)]
pub fn service_bind_ip(family: IpAddr) -> IpAddr {
    bind_ip(family, loopback_only())
}

fn bind_ip(family: IpAddr, loopback: bool) -> IpAddr {
    match (family, loopback) {
        (IpAddr::V4(_), false) => Ipv4Addr::UNSPECIFIED.into(),
        (IpAddr::V6(_), false) => Ipv6Addr::UNSPECIFIED.into(),
        (IpAddr::V4(_), true) => Ipv4Addr::LOCALHOST.into(),
        (IpAddr::V6(_), true) => Ipv6Addr::LOCALHOST.into(),
    }
}

/// One PTP service for the sender's one AirPlay 2 session, or the sessions
/// of one target's members (P158, P169, P179).
#[derive(Debug)]
pub struct PtpService {
    clock: SenderClock,
    identity: [u8; 8],
    remote_ports: PtpPorts,
    /// The receivers it serves while their PTP sessions run (P167, P179),
    /// with the scope of a link-local address (P17); the port is unused.
    peers: Vec<SocketAddr>,
    next_announce: Option<Instant>,
    next_sync: Option<Instant>,
    announce_seq: u16,
    sync_seq: u16,
    transmits: VecDeque<PtpTransmit>,
}

impl PtpService {
    pub fn new(clock: SenderClock, identity: [u8; 8], remote_ports: PtpPorts) -> PtpService {
        PtpService {
            clock,
            identity,
            remote_ports,
            peers: Vec::new(),
            next_announce: None,
            next_sync: None,
            announce_seq: 0,
            sync_seq: 0,
            transmits: VecDeque::new(),
        }
    }

    pub fn identity(&self) -> [u8; 8] {
        self.identity
    }

    /// The first receiver served now, if any.
    pub fn peer(&self) -> Option<IpAddr> {
        self.peers.first().map(SocketAddr::ip)
    }

    /// Every receiver served now.
    pub fn peers(&self) -> &[SocketAddr] {
        &self.peers
    }

    /// A PTP session SETUP to `peer` was answered 200 (P167): the first
    /// Announce goes at once, and Sync every 125 ms from now on. Another
    /// member of the same target joins the schedule already running
    /// (P179).
    pub fn start(&mut self, peer: SocketAddr, now: Instant) {
        let peer = match peer.ip().to_canonical() {
            IpAddr::V6(_) => {
                let mut scoped = peer;
                scoped.set_port(0);
                scoped
            }
            ip => SocketAddr::new(ip, 0),
        };
        if self.peers.iter().any(|known| known.ip() == peer.ip()) {
            return;
        }
        self.peers.push(peer);
        if self.peers.len() == 1 {
            self.next_announce = Some(now);
            self.next_sync = Some(now);
            self.handle_timeout(now);
        }
    }

    /// Every session ended (P167): nothing more is sent.
    pub fn stop(&mut self) {
        self.peers.clear();
        self.next_announce = None;
        self.next_sync = None;
        self.transmits.clear();
    }

    /// One receiver's session ended; the others are still served.
    pub fn stop_peer(&mut self, peer: IpAddr) {
        let peer = peer.to_canonical();
        self.peers.retain(|known| known.ip() != peer);
        self.transmits.retain(|transmit| transmit.to.ip() != peer);
        if self.peers.is_empty() {
            self.stop();
        }
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        match (self.next_announce, self.next_sync) {
            (Some(a), Some(s)) => Some(a.min(s)),
            (a, s) => a.or(s),
        }
    }

    pub fn poll_transmit(&mut self) -> Option<PtpTransmit> {
        self.transmits.pop_front()
    }

    /// Queues the Announce and Sync messages due at `now`. A late turn
    /// sends one of each, not a burst, and the schedule goes on from now.
    pub fn handle_timeout(&mut self, now: Instant) {
        if self.peers.is_empty() {
            return;
        }
        if let Some(due) = self.next_announce
            && due <= now
        {
            let bytes = self.announce();
            for index in 0..self.peers.len() {
                let peer = self.peers[index];
                self.queue(
                    MessageType::Announce,
                    peer,
                    self.announce_seq,
                    bytes.clone(),
                );
            }
            self.announce_seq = self.announce_seq.wrapping_add(1);
            self.next_announce = Some(next_after(due, ANNOUNCE_INTERVAL, now));
        }
        if let Some(due) = self.next_sync
            && due <= now
        {
            let bytes = self.sync();
            for index in 0..self.peers.len() {
                let peer = self.peers[index];
                self.queue(MessageType::Sync, peer, self.sync_seq, bytes.clone());
            }
            self.sync_seq = self.sync_seq.wrapping_add(1);
            self.next_sync = Some(next_after(due, SYNC_INTERVAL, now));
        }
    }

    /// The Sync with `sequence_id` left at `sent`: its Follow_Up carries
    /// that time (P161). For the first receiver served.
    pub fn sync_sent(&mut self, sequence_id: u16, sent: Instant) {
        if let Some(peer) = self.peer() {
            self.sync_sent_to(sequence_id, peer, sent);
        }
    }

    /// The Sync with `sequence_id` left for `peer` at `sent`: that
    /// receiver's Follow_Up carries the time.
    pub fn sync_sent_to(&mut self, sequence_id: u16, peer: IpAddr, sent: Instant) {
        let peer = peer.to_canonical();
        let Some(&peer) = self.peers.iter().find(|known| known.ip() == peer) else {
            return;
        };
        let bytes = self.follow_up(sequence_id, self.clock.ptp(sent));
        self.queue(MessageType::FollowUp, peer, sequence_id, bytes);
    }

    /// A message that reached one of the service's sockets at `received`.
    /// Only a Delay_Req from the receiver's address (P86) is answered
    /// (P162); everything else changes nothing.
    pub fn handle_message(&mut self, bytes: &[u8], source: SocketAddr, received: Instant) {
        if !self
            .peers
            .iter()
            .any(|peer| crate::transport::from_receiver(source, peer.ip()))
        {
            return;
        }
        let Some(request) = DelayReq::parse(bytes) else {
            return;
        };
        let mut out = self.header(
            MessageType::DelayResp,
            DELAY_RESP_BYTES,
            FLAGS_TWO_STEP,
            request.sequence_id,
            LOG_INTERVAL_SYNC,
        );
        put_timestamp(&mut out, self.clock.ptp(received));
        out.extend_from_slice(&request.source_port_identity);
        // `source` keeps the scope of a link-local receiver (P17).
        let mut to = source;
        to.set_port(self.remote_ports.general);
        self.transmits.push_back(PtpTransmit {
            kind: MessageType::DelayResp,
            port: PtpPort::General,
            to,
            sequence_id: request.sequence_id,
            bytes: out,
        });
    }

    fn queue(&mut self, kind: MessageType, peer: SocketAddr, sequence_id: u16, bytes: Vec<u8>) {
        let port = match kind {
            MessageType::Sync | MessageType::DelayReq => PtpPort::Event,
            _ => PtpPort::General,
        };
        let remote = match port {
            PtpPort::Event => self.remote_ports.event,
            PtpPort::General => self.remote_ports.general,
        };
        let mut to = peer;
        to.set_port(remote);
        self.transmits.push_back(PtpTransmit {
            kind,
            port,
            to,
            sequence_id,
            bytes,
        });
    }

    /// The common header (P159), with an empty body to follow.
    fn header(
        &self,
        kind: MessageType,
        length: usize,
        flags: u16,
        sequence_id: u16,
        log_interval: u8,
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(length);
        out.push(TRANSPORT_SPECIFIC | kind as u8);
        out.push(VERSION);
        out.extend_from_slice(&(length as u16).to_be_bytes());
        out.push(0); // domainNumber
        out.push(0);
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&[0; 8]); // correctionField
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&self.identity);
        out.extend_from_slice(&PORT_NUMBER.to_be_bytes());
        out.extend_from_slice(&sequence_id.to_be_bytes());
        out.push(control_field(kind));
        out.push(log_interval);
        out
    }

    /// Announce (P160).
    fn announce(&self) -> Vec<u8> {
        let mut out = self.header(
            MessageType::Announce,
            ANNOUNCE_BYTES,
            FLAGS,
            self.announce_seq,
            0,
        );
        out.extend_from_slice(&[0; TIMESTAMP_BYTES]); // originTimestamp
        out.extend_from_slice(&0i16.to_be_bytes()); // currentUtcOffset
        out.push(0);
        out.push(128); // grandmasterPriority1
        out.extend_from_slice(&[6, 0x21, 0x43, 0x6A]); // grandmasterClockQuality
        out.push(128); // grandmasterPriority2
        out.extend_from_slice(&self.identity);
        out.extend_from_slice(&0u16.to_be_bytes()); // stepsRemoved
        out.push(0x20); // timeSource
        out.extend_from_slice(&0x0008u16.to_be_bytes()); // PATH_TRACE
        out.extend_from_slice(&8u16.to_be_bytes());
        out.extend_from_slice(&self.identity);
        out
    }

    /// A two-step Sync with originTimestamp 0 (P161).
    fn sync(&self) -> Vec<u8> {
        let mut out = self.header(
            MessageType::Sync,
            SYNC_BYTES,
            FLAGS_TWO_STEP,
            self.sync_seq,
            LOG_INTERVAL_SYNC,
        );
        out.extend_from_slice(&[0; TIMESTAMP_BYTES]);
        out
    }

    /// The Follow_Up of Sync `sequence_id` (P161).
    fn follow_up(&self, sequence_id: u16, origin: PtpTime) -> Vec<u8> {
        let mut out = self.header(
            MessageType::FollowUp,
            FOLLOW_UP_BYTES,
            FLAGS,
            sequence_id,
            LOG_INTERVAL_SYNC,
        );
        put_timestamp(&mut out, origin);
        // The IEEE 802.1AS Follow_Up information TLV, all values zero.
        out.extend_from_slice(&3u16.to_be_bytes());
        out.extend_from_slice(&28u16.to_be_bytes());
        out.extend_from_slice(&[0x00, 0x80, 0xC2, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&[0; 22]);
        // Apple's organization extension with the clock identity.
        out.extend_from_slice(&3u16.to_be_bytes());
        out.extend_from_slice(&14u16.to_be_bytes());
        out.extend_from_slice(&[0x00, 0x0D, 0x93, 0x00, 0x00, 0x04]);
        out.extend_from_slice(&self.identity);
        out
    }
}

/// The next time on a schedule of `interval` from `due` that is after
/// `now`.
fn next_after(due: Instant, interval: Duration, now: Instant) -> Instant {
    let next = due + interval;
    if next > now { next } else { now + interval }
}

fn put_timestamp(out: &mut Vec<u8>, time: PtpTime) {
    out.extend_from_slice(&time.seconds.to_be_bytes()[2..]);
    out.extend_from_slice(&time.nanos.to_be_bytes());
}

/// The fields of a Delay_Req the answer needs.
struct DelayReq {
    sequence_id: u16,
    source_port_identity: [u8; 10],
}

impl DelayReq {
    fn parse(bytes: &[u8]) -> Option<DelayReq> {
        if bytes.len() < DELAY_REQ_BYTES
            || bytes[0] & 0x0F != MessageType::DelayReq as u8
            || bytes[1] & 0x0F != VERSION
        {
            return None;
        }
        let mut source_port_identity = [0u8; 10];
        source_port_identity.copy_from_slice(&bytes[20..30]);
        Some(DelayReq {
            sequence_id: u16::from_be_bytes([bytes[30], bytes[31]]),
            source_port_identity,
        })
    }
}

/// Whether a datagram could be a Delay_Req, the one message the service
/// reads; the socket threads drop everything else unread.
pub fn may_be_delay_req(bytes: &[u8]) -> bool {
    DelayReq::parse(bytes).is_some()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::bind_ip;

    #[test]
    fn the_service_binds_every_address_unless_tests_keep_to_loopback() {
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7));
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(bind_ip(v4, false), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(bind_ip(v6, false), IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(bind_ip(v4, true), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(bind_ip(v6, true), IpAddr::V6(Ipv6Addr::LOCALHOST));
    }
}
