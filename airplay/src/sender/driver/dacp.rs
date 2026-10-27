//! The driver's side of remote control from the receiver (P186 to P195):
//! session identifiers with the run's DACP ID, the command server's table
//! kept to the running sessions, and the commands handed to the host.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr, TcpStream};

use super::*;

use crate::dacp::DacpAdvertiser;
use crate::error::Warning;
use crate::ids::EntropyUnavailable;
use crate::sender::remote::{Entry, RemoteInput};

/// The Active-Remote of a phase's running session.
fn active_remote(phase: &Phase) -> Option<u32> {
    match phase {
        Phase::Active(active) => Some(active.session.ids().active_remote),
        Phase::Ap2(active) => Some(active.session.ids().active_remote),
        Phase::Waiting(_)
        | Phase::Queued { .. }
        | Phase::Unannounced
        | Phase::Failed
        | Phase::Idle => None,
    }
}

impl Driver {
    /// Fresh identifiers for a session or a pairing, carrying the run's
    /// DACP ID and an Active-Remote not issued before in the run (P187).
    pub(super) fn session_ids(&mut self) -> Result<SessionIds, EntropyUnavailable> {
        let mut entropy = OsEntropy;
        let mut ids = SessionIds::generate(&mut entropy)?;
        self.dacp.fill_ids(&mut ids, &mut entropy)?;
        Ok(ids)
    }

    /// A session of `key` with this Active-Remote starts, which starts the
    /// command server (P189); commands that name it come from the
    /// addresses discovery reported for the speaker (P190).
    pub(super) fn remote_open(&mut self, key: &DeviceKey, remote: u32, now: Instant) {
        let Some(speaker) = self.speakers.get(key) else {
            return;
        };
        let device = &speaker.device;
        let mut peers: Vec<IpAddr> = Vec::new();
        for endpoint in device.raop.iter().chain(device.airplay.iter()) {
            for address in &endpoint.addresses {
                let ip = address.ip.to_canonical();
                if !peers.contains(&ip) {
                    peers.push(ip);
                }
            }
        }
        let entry = Entry {
            key: key.clone(),
            name: device.name.clone(),
            peers,
            target: speaker.target.clone(),
        };
        self.dacp.open(remote, entry, now);
    }

    /// The RTSP connection of a running session connected: the server
    /// listens on its local address before its first request (P188, P190).
    pub(super) fn remote_connected(
        &mut self,
        key: &DeviceKey,
        session: SessionKey,
        attempt: u32,
        stream: &TcpStream,
    ) {
        let remote = match self.speakers.get(key).map(|speaker| &speaker.phase) {
            Some(Phase::Active(active)) if active.key == session && active.attempt == attempt => {
                active.session.ids().active_remote
            }
            Some(Phase::Ap2(active)) if active.key == session && active.attempt == attempt => {
                active.session.ids().active_remote
            }
            _ => return,
        };
        let (Ok(local), Ok(peer)) = (stream.local_addr(), stream.peer_addr()) else {
            return;
        };
        self.remote_attach(key, remote, local, peer.ip());
    }

    /// Attaches a session's connection from `local` to `peer` to the
    /// server; a port it can't bind there, or a failed registration, is one
    /// warning and the session plays on (P188, P190).
    pub(super) fn remote_attach(
        &mut self,
        key: &DeviceKey,
        remote: u32,
        local: SocketAddr,
        peer: IpAddr,
    ) {
        let attached = self.dacp.connected(remote, local, peer);
        if attached.unavailable {
            self.emit_speaker(
                key,
                SpeakerEvent::Warning(Warning::RemoteControlUnavailable),
            );
        }
        if attached.not_advertised.is_some() {
            self.emit_speaker(
                key,
                SpeakerEvent::Warning(Warning::RemoteControlNotAdvertised),
            );
        }
    }

    /// Keeps the server's table to the running sessions and runs its
    /// timers; called every turn, it looks at the table only when a
    /// session started or ended (P189).
    pub(super) fn remote_turn(&mut self, now: Instant) {
        let live = self.dacp.live();
        let mut running = 0;
        let mut known = true;
        for speaker in self.speakers.values() {
            if let Some(remote) = active_remote(&speaker.phase) {
                running += 1;
                known &= live.contains(&remote);
            }
        }
        if !known || running != live.len() {
            let running: BTreeSet<u32> = self
                .speakers
                .values()
                .filter_map(|speaker| active_remote(&speaker.phase))
                .collect();
            let closed = self.dacp.retain(&running, now);
            self.retired.extend(closed);
        }
        if self.dacp.poll_timeout().is_some_and(|at| at <= now) {
            let closed = self.dacp.handle_timeout(now);
            self.retired.extend(closed);
        }
    }

    /// What a connection thread handed over: a command goes to the host
    /// naming its speaker, and its target for a member, unless its session
    /// ended since the answer (P193).
    pub(super) fn remote_input(&mut self, input: RemoteInput) {
        match input {
            RemoteInput::Log(line) => self.emit(SenderEvent::RemoteLog(line)),
            RemoteInput::Command {
                remote,
                command,
                who,
                what,
            } => {
                let found = self
                    .speakers
                    .iter()
                    .find(|(_, speaker)| active_remote(&speaker.phase) == Some(remote))
                    .map(|(key, speaker)| (key.clone(), speaker.target.clone()));
                match found {
                    Some((key, target)) => self.emit(SenderEvent::Remote {
                        key,
                        target,
                        command,
                    }),
                    None => self.emit(SenderEvent::RemoteLog(format!(
                        "DACP {who}: {what} -> 204, dropped: its session ended"
                    ))),
                }
            }
        }
    }

    /// The host's advertiser for the `_dacp._tcp` record (P188).
    pub(super) fn set_dacp_advertiser(&mut self, advertiser: Box<dyn DacpAdvertiser + Send>) {
        if self.dacp.set_advertiser(advertiser).is_err() {
            let first = self
                .speakers
                .iter()
                .find(|(_, speaker)| active_remote(&speaker.phase).is_some())
                .map(|(key, _)| key.clone());
            if let Some(key) = first {
                self.emit_speaker(
                    &key,
                    SpeakerEvent::Warning(Warning::RemoteControlNotAdvertised),
                );
            }
        }
    }

    /// Stops the server for good when the sender shuts down.
    pub(super) fn stop_remote(&mut self) {
        let closed = self.dacp.stop();
        self.retired.extend(closed);
    }
}
