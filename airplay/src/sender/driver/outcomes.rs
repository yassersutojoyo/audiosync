//! The AirPlay 1 outcome that moved a device to the AirPlay 2 path, kept
//! per device until its `_raop._tcp` service goes or AirPlay 1 plays
//! (P218), and the pairings that follow from it (P219, P220).

use super::*;

use crate::rtsp::Method;

/// What a legacy pairing needs to go on as the AirPlay 2 PIN pairing when
/// its OPTIONS shows that AirPlay 1 can't serve the device (P220).
pub(super) struct Switch {
    pub(super) device: Device,
    pub(super) password: Option<Secret>,
}

impl Driver {
    /// The outcome a speaker's next session reads: a target member only
    /// its own start's (P177), any other speaker its device's (P218).
    pub(super) fn airplay1_outcome(&self, key: &DeviceKey, speaker: &Speaker) -> Option<ErrorKind> {
        if speaker.target.is_some() {
            speaker.airplay1_outcome
        } else {
            self.airplay1_outcomes.get(key).copied()
        }
    }

    pub(super) fn remember_outcome(&mut self, key: &DeviceKey, kind: ErrorKind, reason: &str) {
        if !self.airplay2 || self.airplay1_outcomes.insert(key.clone(), kind) == Some(kind) {
            return;
        }
        self.emit(SenderEvent::PathLog {
            key: key.clone(),
            line: format!("AirPlay 1 outcome {kind:?} remembered ({reason}): AirPlay 2 path"),
        });
    }

    pub(super) fn forget_outcome(&mut self, key: &DeviceKey, reason: &str) {
        if let Some(kind) = self.airplay1_outcomes.remove(key) {
            self.emit(SenderEvent::PathLog {
                key: key.clone(),
                line: format!("AirPlay 1 outcome {kind:?} forgotten ({reason})"),
            });
        }
    }

    /// The legacy pairing a host started ended at its OPTIONS in
    /// Unsupported or Refused: with a [`Switch`] it goes on as the
    /// AirPlay 2 PIN pairing on a new connection, the host seeing one
    /// pairing (P220). Only OPTIONS gives these outcomes in a pairing, so
    /// no `/pair-pin-start` went out. Returns true when the pairing went
    /// on to the AirPlay 2 path.
    pub(super) fn switch_pairing(
        &mut self,
        key: &DeviceKey,
        error: &SessionError,
        now: Instant,
    ) -> bool {
        let at_options = matches!(error.kind, ErrorKind::Unsupported(_) | ErrorKind::Refused)
            && error.method == Some(Method::Options);
        if !at_options {
            return false;
        }
        let Some(switch) = self.pairings.get_mut(key).and_then(|run| run.switch.take()) else {
            return false;
        };
        let Ok(ids) = self.session_ids() else {
            return false;
        };
        self.remember_outcome(key, error.kind, "the pairing's OPTIONS");
        let pairing = self.pin_pairing(&switch.device, switch.password, ids, now);
        match self.pairings.get_mut(key) {
            Some(run) => {
                run.pairing = Job::HomeKit(Box::new(pairing));
                true
            }
            None => false,
        }
    }
}
