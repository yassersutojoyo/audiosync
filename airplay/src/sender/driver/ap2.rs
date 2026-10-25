//! The driver's side of the AirPlay 2 path (P2, P169, P170): one session at
//! a time, on an RTSP connection with the session's UDP sockets and an event
//! connection of its own, its sealed audio in the shared stream (P155), and
//! the PTP service while it uses PTP timing (P158, P167). Nothing here runs
//! while the switch is off: only `start_session` calls
//! [`Driver::start_ap2`], and only with it on.

use std::net::{IpAddr, SocketAddr};

use super::*;

use crate::ap2::bodies::uuid_text;
use crate::ap2::session::{FEATURE_PTP, with_port};
use crate::ap2::{Ap2Config, Ap2Event, Ap2Output, Ap2Session, Ap2State, PtpPeer, Timing};
use crate::error::{UnsupportedReason, Warning};
use crate::ptp::{MessageType, PtpService, clock_identity};
use crate::session::{Connection, MediaDestinations};
use crate::stream::Ap2Audio;
use crate::transport::{PtpIo, Queued};

/// How often a session waiting for the old PTP service's ports looks again.
const PTP_PORTS_POLL: Duration = Duration::from_millis(10);

/// The connection of an AirPlay 2 PIN pairing that paired, framed and
/// waiting for the speaker's session (P153).
pub(super) struct Held {
    key: SessionKey,
    attempt: u32,
    session: Ap2Session,
    io: RtspIo,
    pub(super) until: Instant,
}

/// What a session to start gets from the PTP service (P158).
enum PtpBind {
    Peer(PtpPeer),
    /// No PTP for this session; it asks for NTP only.
    Without,
    /// The old service's threads still hold its ports.
    Later,
}

/// The bound PTP service (P158).
pub(super) struct PtpRun {
    pub(super) service: PtpService,
    io: PtpIo,
    peer: PtpPeer,
    v6: bool,
}

impl PtpRun {
    /// Hands the service's receivers to the socket thread, which drops
    /// datagrams from anyone else (P162).
    fn share_peers(&self) {
        self.io
            .set_peers(self.service.peers().iter().map(SocketAddr::ip));
    }
}

/// A held pairing connection taken over by the speaker's session.
struct Claimed {
    key: SessionKey,
    attempt: u32,
    session: Ap2Session,
    io: SessionIo,
    connection: Connection,
}

/// An AirPlay 2 session that is starting, ready or closing.
pub(super) struct Ap2Active {
    pub(super) key: SessionKey,
    /// The event connection's I/O reports under its own key (P70).
    pub(super) events_key: SessionKey,
    pub(super) session: Ap2Session,
    pub(super) attempt: u32,
    pub(super) events_attempt: u32,
    pub(super) io: Option<SessionIo>,
    pub(super) events_io: Option<RtspIo>,
    /// The speaker in the stream once the stream is set up and flushed.
    pub(super) stream_id: Option<stream::SpeakerId>,
    /// The session started with stored HomeKit credentials, so a
    /// PairingRequired from it means the receiver no longer takes them (P154).
    pub(super) verifying: bool,
}

impl Driver {
    fn ap2_active(&mut self, key: &DeviceKey) -> Option<&mut Ap2Active> {
        match &mut self.speakers.get_mut(key)?.phase {
            Phase::Ap2(active) => Some(active),
            _ => None,
        }
    }

    /// Starts an AirPlay 2 session, unless another speaker runs one or waits
    /// to retry its own (P169): that one, and every AirPlay 1 session, go on
    /// untouched.
    pub(super) fn start_ap2(&mut self, key: &DeviceKey, now: Instant) {
        let running = self.speakers.iter().any(|(other, speaker)| {
            other != key && speaker.holds_ap2() && !self.same_target(key, other)
        });
        if running {
            let error = SessionError::new(ErrorKind::Unsupported(
                UnsupportedReason::OneAirPlay2Session,
            ));
            self.refuse(key, error, now);
            self.ap2_refusals += 1;
            if let Some(speaker) = self.speakers.get_mut(key) {
                speaker.ap2_waiting = true;
                speaker.ap2_refusal = self.ap2_refusals;
            }
            return;
        }
        let Ok(ids) = self.session_ids() else {
            let mut error = SessionError::new(ErrorKind::Unreachable);
            error.detail = Some("the random number source is unavailable");
            return self.refuse(key, error, now);
        };
        // The PTP ports are bound only for a receiver that may take PTP:
        // one that took it before or sets features bit 41 (P157, P158).
        let restart = self.ptp_restart.remove(key);
        let wants = restart != Some(None)
            && self.speakers.get(key).is_some_and(|speaker| {
                self.ptp_timing.contains(key) || speaker.device.features().has(FEATURE_PTP)
            });
        let mut addresses = self
            .speakers
            .get(key)
            .map(|speaker| speaker.device.airplay_socket_addrs())
            .unwrap_or_default();
        // The session goes on over the connection its PIN pairing left open.
        let held_remote = self.held.get(key).and_then(|held| held.session.remote());
        if let Some(remote) = held_remote {
            addresses.sort_by_key(|address| *address != remote);
        }
        if matches!(restart, Some(Some(_))) {
            self.ptp_retrying.insert(key.clone());
        } else {
            self.ptp_retrying.remove(key);
        }
        // A family the speaker no longer has an address in (a network
        // change) is forgotten, so its sessions don't go without a clock.
        if let Some(&moved) = self.ptp_moved.get(key)
            && !addresses
                .iter()
                .any(|address| address.ip().to_canonical().is_ipv6() == moved)
        {
            self.ptp_moved.remove(key);
        }
        let mut ptp = None;
        let mut ptp_v6 = None;
        if wants && let Some(v6) = restart.flatten().or_else(|| self.ptp_v6(key)) {
            let in_family = |address: &SocketAddr| address.ip().to_canonical().is_ipv6() == v6;
            match addresses.iter().copied().find(in_family) {
                Some(address) => {
                    // The session goes over the service's family first (P158).
                    addresses.sort_by_key(|address| !in_family(address));
                    match self.ptp_peer(key, address.ip()) {
                        PtpBind::Peer(peer) => {
                            ptp = Some(peer);
                            ptp_v6 = Some(v6);
                        }
                        PtpBind::Without => {}
                        PtpBind::Later => return self.start_ap2_later(key, restart, now),
                    }
                }
                None => {
                    if self.ptp_family_told.insert(key.clone()) {
                        self.emit_speaker(key, SpeakerEvent::Warning(Warning::PtpOutsideFamily));
                    }
                }
            }
        }
        let members = self.other_member_addresses(key);
        let claimed = self.claim_held(key, ptp_v6);
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let device = &speaker.device;
        let mut config = Ap2Config::new(addresses, speaker.options.volume);
        config.trace = speaker.options.trace;
        config.read_volume = speaker.reads_volume;
        config.txt_flags = device
            .airplay_txt
            .as_ref()
            .and_then(|txt| txt.flags)
            .map(|flags| u64::from(flags.0));
        config.features = device.features();
        config.credentials = speaker
            .options
            .homekit_credentials
            .clone()
            .or_else(|| self.homekit.get(key).cloned());
        let verifying = config.credentials.is_some();
        config.password = speaker.options.password.clone();
        config.device_id = self.device_id;
        config.ptp = ptp;
        config.members = members;
        config.timing = if self.ptp_timing.contains(key) {
            Timing::Ptp
        } else {
            Timing::Ntp
        };
        let events_key = SessionKey(self.next_session);
        self.next_session += 1;
        let mut pairing_connection = None;
        let (session_key, session, attempt, io, verifying) = match claimed {
            Some(Claimed {
                key: session_key,
                attempt,
                mut session,
                io,
                connection,
            }) => {
                pairing_connection = Some(connection);
                session.proceed(config, connection, now);
                (session_key, session, attempt, Some(io), false)
            }
            None => {
                let session_key = SessionKey(self.next_session);
                self.next_session += 1;
                let session = Ap2Session::new(config, ids, Box::new(OsEntropy), now);
                (session_key, session, 0, None, verifying)
            }
        };
        let remote = session.ids().active_remote;
        speaker.phase = Phase::Ap2(Box::new(Ap2Active {
            key: session_key,
            events_key,
            session,
            attempt,
            events_attempt: 0,
            io,
            events_io: None,
            stream_id: None,
            verifying,
        }));
        self.sessions.insert(session_key, key.clone());
        self.sessions.insert(events_key, key.clone());
        self.remote_open(key, remote, now);
        // A session that goes on over its PIN pairing's connection attaches
        // that connection's address (P190).
        if let (Some(Connection { local, .. }), Some(peer)) = (pairing_connection, held_remote) {
            self.remote_attach(key, remote, SocketAddr::new(local, 0), peer.ip());
        }
        self.emit_speaker(key, SpeakerEvent::State(SpeakerState::Connecting));
    }

    /// Takes the connection a PIN pairing with the device left open and
    /// opens the session's UDP sockets next to it, unless it goes to an
    /// address outside the family of the PTP service in `ptp_v6` (P158).
    fn claim_held(&mut self, key: &DeviceKey, ptp_v6: Option<bool>) -> Option<Claimed> {
        let remote = self.held.get(key)?.session.remote()?;
        let counters = match self.speakers.get(key) {
            Some(speaker) if ptp_v6.is_none_or(|v6| remote.ip().to_canonical().is_ipv6() == v6) => {
                speaker.counters.clone()
            }
            _ => {
                self.drop_held(key);
                return None;
            }
        };
        let Held {
            key: session_key,
            attempt,
            session,
            io,
            ..
        } = self.held.remove(key)?;
        self.pairing_sessions.remove(&session_key);
        let notify = self.notify();
        let clock = self.stream.clock();
        match SessionIo::adopt(io, session_key, attempt, clock, counters, notify) {
            Ok((io, connection)) => Some(Claimed {
                key: session_key,
                attempt,
                session,
                io,
                connection,
            }),
            Err(threads) => {
                self.retired.extend(threads);
                None
            }
        }
    }

    /// The PIN pairing the host asked for with a device on the AirPlay 2
    /// path (P61, P153), on a connection the speaker's session takes over.
    pub(super) fn pin_pairing(
        &self,
        device: &Device,
        password: Option<Secret>,
        ids: SessionIds,
        now: Instant,
    ) -> PinPairing {
        let mut config = Ap2Config::new(device.airplay_socket_addrs(), Volume::Muted);
        config.txt_flags = device
            .airplay_txt
            .as_ref()
            .and_then(|txt| txt.flags)
            .map(|flags| u64::from(flags.0));
        config.features = device.features();
        config.password = password;
        config.device_id = self.device_id;
        PinPairing::new(config, ids, Box::new(OsEntropy), now)
    }

    /// Keeps the open connection of a PIN pairing that paired, still
    /// reporting under `session_key`, for the speaker's session.
    pub(super) fn hold(
        &mut self,
        key: &DeviceKey,
        session_key: SessionKey,
        attempt: u32,
        session: Ap2Session,
        io: RtspIo,
        now: Instant,
    ) {
        self.drop_held(key);
        let held = Held {
            key: session_key,
            attempt,
            session,
            io,
            until: now + self.held_for,
        };
        self.held.insert(key.clone(), held);
    }

    /// Closes a held pairing connection: unclaimed in time, given up by the
    /// host, or ended by the receiver. The speaker's next session verifies.
    pub(super) fn drop_held(&mut self, key: &DeviceKey) {
        if let Some(held) = self.held.remove(key) {
            self.pairing_sessions.remove(&held.key);
            self.retired.extend(held.io.close());
        }
    }

    /// The receiver closed a held connection, or wrote on it unasked.
    pub(super) fn held_io_event(&mut self, key: &DeviceKey, event: &IoEvent) {
        let ours = self.held.get(key).is_some_and(|held| match event {
            IoEvent::Rtsp { attempt, .. } | IoEvent::RtspClosed { attempt, .. } => {
                *attempt == held.attempt
            }
            _ => false,
        });
        if ours {
            self.drop_held(key);
        }
    }

    /// Keeps the credentials a PIN pairing gave, so a session started before
    /// the host hands them back verifies with them (P152).
    pub(super) fn keep_homekit(&mut self, key: &DeviceKey, credentials: HomeKitCredentials) {
        if let Some(speaker) = self.speakers.get_mut(key) {
            speaker.options.homekit_credentials = Some(credentials.clone());
        }
        self.homekit.insert(key.clone(), credentials);
    }

    /// Drops the HomeKit credentials kept for a device, so its next session
    /// pairs again.
    pub(super) fn forget_homekit(&mut self, key: &DeviceKey) {
        if let Some(speaker) = self.speakers.get_mut(key) {
            speaker.options.homekit_credentials = None;
        }
        self.homekit.remove(key);
    }

    /// Drops HomeKit credentials the receiver turned down (P154), and tells
    /// the host to delete its stored copy.
    fn reject_homekit(&mut self, key: &DeviceKey) {
        self.forget_homekit(key);
        self.emit_speaker(key, SpeakerEvent::HomeKitRejected);
    }

    pub(super) fn next_ap2_output(&mut self, key: &DeviceKey) -> Option<Ap2Output> {
        self.ap2_active(key)?.session.poll_output()
    }

    pub(super) fn apply_ap2_output(&mut self, key: &DeviceKey, output: Ap2Output, now: Instant) {
        let notify = self.notify();
        match output {
            Ap2Output::Connect(address) => {
                let Some(active) = self.ap2_active(key) else {
                    return;
                };
                active.attempt += 1;
                if transport::connect(active.key, active.attempt, address, notify).is_err() {
                    active.session.handle_connect_failed(now);
                }
            }
            Ap2Output::Send(bytes) => {
                if let Some(io) = self.ap2_active(key).and_then(|active| active.io.as_ref()) {
                    io.write(bytes);
                }
            }
            Ap2Output::ConnectEvents(address) => {
                let Some(active) = self.ap2_active(key) else {
                    return;
                };
                active.events_attempt += 1;
                let attempt = active.events_attempt;
                if transport::connect(active.events_key, attempt, address, notify).is_err() {
                    active.session.handle_events_connect_failed(now);
                }
            }
            Ap2Output::SendEvents(bytes) => {
                let Some(io) = self
                    .ap2_active(key)
                    .and_then(|active| active.events_io.as_ref())
                else {
                    return;
                };
                if io.write_bounded(bytes, transport::EVENT_BACKLOG) == Queued::Overflowed {
                    let line = format!(
                        "warning: the receiver left more than {} event answers or {} bytes unread; \
                         closing the event connection",
                        transport::EVENT_BACKLOG.messages,
                        transport::EVENT_BACKLOG.bytes
                    );
                    self.emit_speaker(key, SpeakerEvent::Trace(line));
                }
            }
            Ap2Output::Close => self.release_ap2(key),
            Ap2Output::Event(event) => self.ap2_event(key, event, now),
        }
    }

    /// Closes both connections and the UDP sockets of the session, takes
    /// its speaker out of the stream and stops the PTP traffic (P167).
    pub(super) fn release_ap2(&mut self, key: &DeviceKey) {
        let Some(active) = self.ap2_active(key) else {
            return;
        };
        let io = active.io.take();
        let events = active.events_io.take();
        let stream_id = active.stream_id.take();
        let remote = active.session.remote();
        if let Some(io) = io {
            self.retired.extend(io.close());
        }
        if let Some(events) = events {
            self.retired.extend(events.close());
        }
        if let Some(id) = stream_id {
            self.stream.remove_speaker(id);
            self.stream_ids.remove(&id);
        }
        if let (Some(run), Some(remote)) = (&mut self.ptp, remote) {
            run.service.stop_peer(remote.ip());
            run.share_peers();
        }
    }

    /// The AirPlay 2 session is over: the PTP ports are let go.
    pub(super) fn close_ptp(&mut self) {
        if let Some(run) = self.ptp.take() {
            self.ptp_closing.retain(|thread| !thread.is_finished());
            self.ptp_closing.extend(run.io.close());
        }
    }

    /// Whether another speaker's AirPlay 2 session may use the PTP
    /// service: only members of one target share the path (P169, P179).
    fn ptp_shared(&self, key: &DeviceKey) -> bool {
        self.speakers
            .iter()
            .any(|(other, speaker)| other != key && matches!(speaker.phase, Phase::Ap2(_)))
    }

    /// The address family, IPv6 when true, of the PTP service for a
    /// session to `key`: the running service's while another member uses
    /// it, the family it last moved to for this speaker, IPv4 for a target with
    /// a member reachable over IPv4, and otherwise the family of the
    /// receiver's first address (P158, P179).
    fn ptp_v6(&self, key: &DeviceKey) -> Option<bool> {
        if let Some(run) = &self.ptp
            && self.ptp_shared(key)
        {
            return Some(run.v6);
        }
        if let Some(&moved) = self.ptp_moved.get(key) {
            return Some(moved);
        }
        let has_v4 = |device: &&Device| {
            device
                .airplay_socket_addrs()
                .iter()
                .any(|address| !address.ip().to_canonical().is_ipv6())
        };
        if self.members_in_view(key).iter().any(has_v4) {
            return Some(false);
        }
        let speaker = self.speakers.get(key)?;
        let first = speaker.device.airplay_socket_addrs().first()?.ip();
        Some(first.to_canonical().is_ipv6())
    }

    /// The PTP service for a session to a receiver in `family`'s address
    /// family, bound now if it isn't (P158). When a port can't be bound the
    /// host is told once and the session goes without PTP; while the old
    /// service's threads still run, the bind waits for a later turn.
    fn ptp_peer(&mut self, key: &DeviceKey, family: IpAddr) -> PtpBind {
        let v6 = family.to_canonical().is_ipv6();
        if let Some(run) = &self.ptp {
            if run.v6 == v6 {
                return PtpBind::Peer(run.peer.clone());
            }
            if self.ptp_shared(key) {
                return PtpBind::Without;
            }
        }
        self.close_ptp();
        // An IPv6 wildcard socket takes the IPv4 port too on most systems, so
        // the old service's ports must be free first; joining here would stall every stream.
        self.ptp_closing.retain(|thread| !thread.is_finished());
        if !self.ptp_closing.is_empty() {
            return PtpBind::Later;
        }
        let mut random = [0u8; 22];
        if OsEntropy.fill(&mut random).is_err() {
            return PtpBind::Without;
        }
        let mut identity_random = [0u8; 6];
        identity_random.copy_from_slice(&random[..6]);
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&random[6..]);
        let identity = clock_identity(identity_random);
        match PtpIo::open(family.to_canonical(), self.ptp_ports, self.notify()) {
            Ok(io) => {
                let peer = PtpPeer {
                    clock_id: u64::from_be_bytes(identity),
                    id: uuid_text(uuid),
                };
                let service =
                    PtpService::new(self.stream.clock(), identity, self.ptp_receiver_ports);
                self.ptp = Some(PtpRun {
                    service,
                    io,
                    peer: peer.clone(),
                    v6,
                });
                if std::mem::take(&mut self.ptp_unavailable_told) {
                    self.emit_speaker(key, SpeakerEvent::PtpOpened);
                }
                PtpBind::Peer(peer)
            }
            Err(_) => {
                if !self.ptp_unavailable_told {
                    self.ptp_unavailable_told = true;
                    self.emit_speaker(key, SpeakerEvent::Warning(Warning::PtpUnavailable));
                }
                PtpBind::Without
            }
        }
    }

    /// The session waits for the old PTP service's ports: the speaker keeps
    /// the AirPlay 2 place and starts again on a later turn (P169).
    fn start_ap2_later(&mut self, key: &DeviceKey, restart: Option<Option<bool>>, now: Instant) {
        if let Some(restart) = restart {
            self.ptp_restart.insert(key.clone(), restart);
        }
        if let Some(speaker) = self.speakers.get_mut(key) {
            speaker.ap2_retry = true;
            speaker.phase = Phase::Waiting(now + PTP_PORTS_POLL);
        }
    }

    /// Closes, without a TEARDOWN, a session that took PTP over an address
    /// outside the service's family and starts the speaker again: with the
    /// service moved to that family when no other session uses it and the
    /// session isn't already the restart after a move, else on NTP with the
    /// warning told once (P158, P179).
    fn restart_outside_family(&mut self, key: &DeviceKey, landed_v6: bool, now: Instant) {
        let family = if self.ptp_shared(key) || self.ptp_retrying.contains(key) {
            if self.ptp_family_told.insert(key.clone()) {
                self.emit_speaker(key, SpeakerEvent::Warning(Warning::PtpOutsideFamily));
            }
            None
        } else {
            self.ptp_moved.insert(key.clone(), landed_v6);
            Some(landed_v6)
        };
        if self.is_target_member(key) {
            self.note_teardown(key.clone(), now);
        }
        self.ptp_restart.insert(key.clone(), family);
        self.end_session(key, Phase::Idle);
        self.start_session(key, now);
    }

    /// Sends what the PTP service queued; a Sync's Follow_Up carries the
    /// time read right after the Sync left (P161). True when anything went.
    pub(super) fn send_ptp(&mut self) -> bool {
        let Some(run) = &mut self.ptp else {
            return false;
        };
        let mut sent = false;
        while let Some(transmit) = run.service.poll_transmit() {
            sent = true;
            run.io.send(transmit.port, transmit.to, &transmit.bytes);
            if transmit.kind == MessageType::Sync {
                run.service
                    .sync_sent_to(transmit.sequence_id, transmit.to.ip(), Instant::now());
            }
        }
        sent
    }

    /// The stream is set up and flushed (P42): the speaker joins the stream
    /// with its sealed packets (P155), and time-announce packets under PTP
    /// (P166).
    fn ap2_ready(&mut self, key: &DeviceKey, now: Instant) {
        let clock_identity = self.ptp.as_ref().map(|run| run.service.identity());
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let Phase::Ap2(active) = &mut speaker.phase else {
            return;
        };
        let (Some(remote), Some(ports)) = (active.session.remote(), active.session.stream()) else {
            return;
        };
        let ids = active.session.ids();
        let ptp = active.session.timing() == Timing::Ptp;
        let mut config = SpeakerConfig::new(
            Codec::Alac,
            MediaDestinations {
                audio: with_port(remote, ports.data_port),
                control: with_port(remote, ports.control_port),
            },
            ids.ssrc,
            ids.first_seq,
            ids.first_rtptime,
        );
        config.delay = speaker.options.delay;
        config.reported_latency = active.session.reported_latency();
        config.airplay2 = Some(Ap2Audio {
            key: active.session.stream_key(),
            clock_identity: clock_identity.filter(|_| ptp),
        });
        let id = self
            .stream
            .add_speaker_with_counters(config, speaker.counters.clone(), now);
        active.stream_id = Some(id);
        self.stream_ids.insert(id, key.clone());
    }

    /// Sends one of the stream's datagrams from the session's control
    /// socket, once the stream is ready.
    pub(super) fn ap2_transmit(&mut self, key: &DeviceKey, transmit: Transmit, now: Instant) {
        let Some(active) = self.ap2_active(key) else {
            return;
        };
        if active.session.state() != Ap2State::Ready {
            return;
        }
        let Some(io) = &active.io else {
            return;
        };
        if let Some(result) = io.send(transmit.from, transmit.to, &transmit.bytes) {
            active.session.handle_udp_send(result, now);
        }
    }

    /// The stream restarted a speaker: FLUSH on its session (P100, P143).
    pub(super) fn ap2_flush(&mut self, key: &DeviceKey, seq: u16, rtptime: u32, now: Instant) {
        if let Some(active) = self.ap2_active(key) {
            active.session.flush(seq, rtptime, now);
        }
    }

    fn ap2_event(&mut self, key: &DeviceKey, event: Ap2Event, now: Instant) {
        match event {
            // Only a PinPairing's session reports these.
            Ap2Event::Info(_)
            | Ap2Event::PinRequested
            | Ap2Event::WrongPin
            | Ap2Event::Verified => {}
            Ap2Event::Server(server) => self.emit_speaker(key, SpeakerEvent::Server(server)),
            Ap2Event::Credentials(credentials) => {
                // Kept for the sender's lifetime, so later sessions verify
                // instead of pairing again, and handed to the host to store.
                if let Some(speaker) = self.speakers.get_mut(key) {
                    speaker.options.homekit_credentials = Some(credentials.clone());
                }
                self.homekit.insert(key.clone(), credentials.clone());
                self.emit_speaker(key, SpeakerEvent::HomeKitPaired(credentials));
            }
            Ap2Event::TimingChosen(timing) => {
                if timing == Timing::Ptp {
                    self.ptp_timing.insert(key.clone());
                    let remote = self
                        .ap2_active(key)
                        .and_then(|active| active.session.remote());
                    match (&mut self.ptp, remote) {
                        (Some(run), Some(remote))
                            if remote.ip().to_canonical().is_ipv6() == run.v6 =>
                        {
                            run.service.start(remote, now);
                            run.share_peers();
                        }
                        // It fell back to an address outside the service's
                        // family, which no clock serves (P158, P179).
                        (Some(_), Some(remote)) => {
                            let landed_v6 = remote.ip().to_canonical().is_ipv6();
                            self.restart_outside_family(key, landed_v6, now);
                        }
                        _ => {}
                    }
                } else if !self.ptp_shared(key)
                    && self
                        .ptp
                        .as_ref()
                        .is_some_and(|run| run.service.peers().is_empty())
                {
                    // P158: the service runs only while a session uses PTP.
                    self.close_ptp();
                }
            }
            Ap2Event::Ready(_) => {
                self.ap2_ready(key, now);
                if let Some(speaker) = self.speakers.get_mut(key) {
                    speaker.backoff.on_playing();
                }
                self.emit_speaker(key, SpeakerEvent::State(SpeakerState::Playing));
                // Ready follows the stream SETUP answer and FLUSH (P123).
                self.send_now_playing(key, now);
            }
            Ap2Event::Warning(warning) => self.emit_speaker(key, SpeakerEvent::Warning(warning)),
            Ap2Event::Trace(line) => self.emit_speaker(key, SpeakerEvent::Trace(line)),
            Ap2Event::VolumeReported(db) => {
                self.emit_speaker(key, SpeakerEvent::VolumeReported(db))
            }
            Ap2Event::Failed(error) => {
                let verifying = self.ap2_active(key).is_some_and(|active| active.verifying);
                let member = self
                    .speakers
                    .get(key)
                    .is_some_and(|speaker| speaker.target.is_some());
                let settling = member && self.settling(key, error.kind, now);
                if verifying && error.kind == ErrorKind::PairingRequired && !settling {
                    self.reject_homekit(key);
                }
                self.on_failed(key, error, now);
            }
            Ap2Event::Closed => self.on_closed(key, now),
        }
    }

    /// What the I/O threads of an AirPlay 2 session report.
    pub(super) fn ap2_io_event(&mut self, key: &DeviceKey, event: IoEvent, now: Instant) {
        let notify = self.notify();
        let clock = self.stream.clock();
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let counters = speaker.counters.clone();
        let Phase::Ap2(active) = &mut speaker.phase else {
            return;
        };
        let session = match &event {
            IoEvent::Connected { session, .. }
            | IoEvent::ConnectFailed { session, .. }
            | IoEvent::Rtsp { session, .. }
            | IoEvent::RtspClosed { session, .. }
            | IoEvent::Control { session, .. }
            | IoEvent::TimingRequest { session, .. } => *session,
            IoEvent::Ptp { .. } => return,
        };
        if session == active.events_key {
            return events_io_event(active, event, notify, now);
        }
        if session != active.key {
            return;
        }
        match event {
            IoEvent::Connected {
                attempt, stream, ..
            } => {
                if attempt != active.attempt || active.session.state() != Ap2State::Connecting {
                    return;
                }
                match SessionIo::open(session, attempt, stream, clock, counters, notify) {
                    Ok((io, connection)) => {
                        active.io = Some(io);
                        active.session.handle_connected(connection, now);
                    }
                    Err(_) => active.session.handle_connect_failed(now),
                }
            }
            IoEvent::ConnectFailed { attempt, .. } => {
                if attempt == active.attempt {
                    active.session.handle_connect_failed(now);
                }
            }
            IoEvent::Rtsp { attempt, bytes, .. } => {
                if attempt == active.attempt {
                    active.session.handle_input(&bytes, now);
                }
            }
            IoEvent::RtspClosed { attempt, .. } => {
                if attempt == active.attempt {
                    active.session.handle_connection_closed(now);
                }
            }
            IoEvent::Control { bytes, .. } => {
                if let Some(id) = active.stream_id {
                    self.stream.handle_control_packet(id, &bytes, now);
                }
            }
            IoEvent::TimingRequest { at, .. } => active.session.handle_timing_request(at),
            IoEvent::Ptp { .. } => {}
        }
    }
}

fn events_io_event(
    active: &mut Ap2Active,
    event: IoEvent,
    notify: impl transport::Notify,
    now: Instant,
) {
    match event {
        IoEvent::Connected {
            attempt, stream, ..
        } => {
            if attempt != active.events_attempt || active.events_io.is_some() {
                return;
            }
            match RtspIo::open(active.events_key, attempt, stream, notify) {
                Ok(io) => {
                    active.events_io = Some(io);
                    active.session.handle_events_connected(now);
                }
                Err(_) => active.session.handle_events_connect_failed(now),
            }
        }
        IoEvent::ConnectFailed { attempt, .. } => {
            if attempt == active.events_attempt {
                active.session.handle_events_connect_failed(now);
            }
        }
        IoEvent::Rtsp { attempt, bytes, .. } => {
            if attempt == active.events_attempt {
                active.session.handle_events_input(&bytes, now);
            }
        }
        IoEvent::RtspClosed { attempt, .. } => {
            if attempt == active.events_attempt {
                active.session.handle_events_closed(now);
            }
        }
        IoEvent::Control { .. } | IoEvent::TimingRequest { .. } | IoEvent::Ptp { .. } => {}
    }
}
