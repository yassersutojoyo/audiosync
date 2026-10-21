//! The driver's side of the AirPlay 2 path (P2, P169, P170): one session at
//! a time, on an RTSP connection with the session's UDP sockets and an event
//! connection of its own, its sealed audio in the shared stream (P155), and
//! the PTP service while it uses PTP timing (P158, P167). Nothing here runs
//! while the switch is off: only `start_session` calls
//! [`Driver::start_ap2`], and only with it on.

use std::net::IpAddr;

use super::*;

use crate::ap2::bodies::uuid_text;
use crate::ap2::session::{FEATURE_PTP, with_port};
use crate::ap2::{Ap2Config, Ap2Event, Ap2Output, Ap2Session, Ap2State, PtpPeer, Timing};
use crate::error::{UnsupportedReason, Warning};
use crate::ptp::{MessageType, PtpService, clock_identity};
use crate::session::MediaDestinations;
use crate::stream::Ap2Audio;
use crate::transport::PtpIo;

/// The bound PTP service (P158).
pub(super) struct PtpRun {
    pub(super) service: PtpService,
    io: PtpIo,
    peer: PtpPeer,
    v6: bool,
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
}

impl Driver {
    fn ap2_active(&mut self, key: &DeviceKey) -> Option<&mut Ap2Active> {
        match &mut self.speakers.get_mut(key)?.phase {
            Phase::Ap2(active) => Some(active),
            _ => None,
        }
    }

    /// Starts an AirPlay 2 session, unless another speaker runs one
    /// (P169): that one, and every AirPlay 1 session, go on untouched.
    pub(super) fn start_ap2(&mut self, key: &DeviceKey, now: Instant) {
        let running = self
            .speakers
            .iter()
            .any(|(other, speaker)| other != key && matches!(speaker.phase, Phase::Ap2(_)));
        if running {
            let error = SessionError::new(ErrorKind::Unsupported(
                UnsupportedReason::OneAirPlay2Session,
            ));
            return self.refuse(key, error, now);
        }
        let Ok(ids) = SessionIds::generate(&mut OsEntropy) else {
            let mut error = SessionError::new(ErrorKind::Unreachable);
            error.detail = Some("the random number source is unavailable");
            return self.refuse(key, error, now);
        };
        // The PTP ports are bound only for a receiver that may take PTP:
        // one that took it before or sets features bit 41 (P157, P158).
        let ptp_family = self.speakers.get(key).and_then(|speaker| {
            let wants = self.ptp_timing.contains(key) || speaker.device.features().has(FEATURE_PTP);
            let first = speaker.device.airplay_socket_addrs().first().copied();
            first.filter(|_| wants).map(|address| address.ip())
        });
        let ptp = ptp_family.and_then(|family| self.ptp_peer(key, family));
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let device = &speaker.device;
        let mut config = Ap2Config::new(device.airplay_socket_addrs(), speaker.options.volume);
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
        config.password = speaker.options.password.clone();
        config.device_id = self.device_id;
        config.ptp = ptp;
        config.timing = if self.ptp_timing.contains(key) {
            Timing::Ptp
        } else {
            Timing::Ntp
        };
        let session_key = SessionKey(self.next_session);
        let events_key = SessionKey(self.next_session + 1);
        self.next_session += 2;
        speaker.phase = Phase::Ap2(Box::new(Ap2Active {
            key: session_key,
            events_key,
            session: Ap2Session::new(config, ids, Box::new(OsEntropy), now),
            attempt: 0,
            events_attempt: 0,
            io: None,
            events_io: None,
            stream_id: None,
        }));
        self.sessions.insert(session_key, key.clone());
        self.sessions.insert(events_key, key.clone());
        self.emit_speaker(key, SpeakerEvent::State(SpeakerState::Connecting));
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
                if let Some(io) = self
                    .ap2_active(key)
                    .and_then(|active| active.events_io.as_ref())
                {
                    io.write(bytes);
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
        if let Some(run) = &mut self.ptp {
            run.service.stop();
        }
    }

    /// The AirPlay 2 session is over: the PTP ports are let go.
    pub(super) fn close_ptp(&mut self) {
        if let Some(run) = self.ptp.take() {
            self.retired.extend(run.io.close());
        }
    }

    /// The PTP service for a session to a receiver in `family`'s address
    /// family, bound now if it isn't (P158). When a port can't be bound the
    /// host is told once and the session goes without PTP.
    fn ptp_peer(&mut self, key: &DeviceKey, family: IpAddr) -> Option<PtpPeer> {
        let v6 = family.to_canonical().is_ipv6();
        if let Some(run) = &self.ptp
            && run.v6 == v6
        {
            return Some(run.peer.clone());
        }
        self.close_ptp();
        let mut random = [0u8; 22];
        OsEntropy.fill(&mut random).ok()?;
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
                Some(peer)
            }
            Err(_) => {
                if !self.ptp_unavailable_told {
                    self.ptp_unavailable_told = true;
                    self.emit_speaker(key, SpeakerEvent::Warning(Warning::PtpUnavailable));
                }
                None
            }
        }
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
                run.service.sync_sent(transmit.sequence_id, Instant::now());
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
            Ap2Event::Info(_) => {}
            Ap2Event::Server(server) => self.emit_speaker(key, SpeakerEvent::Server(server)),
            Ap2Event::Credentials(credentials) => {
                // Kept for the sender's lifetime, so later sessions verify
                // instead of pairing again.
                self.homekit.insert(key.clone(), credentials);
            }
            Ap2Event::TimingChosen(timing) => {
                if timing == Timing::Ptp {
                    self.ptp_timing.insert(key.clone());
                    let remote = self
                        .ap2_active(key)
                        .and_then(|active| active.session.remote());
                    if let (Some(run), Some(remote)) = (&mut self.ptp, remote) {
                        run.service.start(remote.ip(), now);
                    }
                } else {
                    // P158: the service runs only while the session uses PTP.
                    self.close_ptp();
                }
            }
            Ap2Event::Ready(_) => {
                self.ap2_ready(key, now);
                if let Some(speaker) = self.speakers.get_mut(key) {
                    speaker.backoff.on_playing();
                }
                self.emit_speaker(key, SpeakerEvent::State(SpeakerState::Playing));
            }
            Ap2Event::Warning(warning) => self.emit_speaker(key, SpeakerEvent::Warning(warning)),
            Ap2Event::Failed(error) => self.on_failed(key, error, now),
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
