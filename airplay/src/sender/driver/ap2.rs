//! The driver's side of the AirPlay 2 path (P2, P169, P170): one session at
//! a time, on an RTSP connection with the session's UDP sockets and an event
//! connection of its own. Nothing here runs while the switch is off: only
//! `start_session` calls [`Driver::start_ap2`], and only with it on.

use super::*;

use crate::ap2::{Ap2Config, Ap2Event, Ap2Output, Ap2Session, Ap2State, Timing};
use crate::error::UnsupportedReason;

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
        // No PTP service runs yet, so the session asks for NTP only and a
        // receiver that refuses it is Unsupported (timing) (P157, P158).
        config.ptp = None;
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

    /// Closes both connections and the UDP sockets of the session.
    pub(super) fn release_ap2(&mut self, key: &DeviceKey) {
        let Some(active) = self.ap2_active(key) else {
            return;
        };
        let io = active.io.take();
        let events = active.events_io.take();
        if let Some(io) = io {
            self.retired.extend(io.close());
        }
        if let Some(events) = events {
            self.retired.extend(events.close());
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
                }
            }
            Ap2Event::Ready(_) => {
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
            // No audio flows yet, so no retransmission request is served.
            IoEvent::Control { .. } => {}
            IoEvent::TimingRequest { at, .. } => active.session.handle_timing_request(at),
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
        IoEvent::Control { .. } | IoEvent::TimingRequest { .. } => {}
    }
}
