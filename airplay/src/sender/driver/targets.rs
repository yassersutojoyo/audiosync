//! The driver's side of targets (P177 to P181): a session per member in
//! view, all on one path, one AirPlay 2 place for the whole target, one
//! delay and one volume, and members that come back or settle after a
//! TEARDOWN.

use super::*;

use crate::discovery::ProtocolPath;
use crate::error::UnsupportedReason;

/// How long after the sender's own TEARDOWN to a member a refusal counts
/// as a loss (P181).
pub(super) const SETTLE_AFTER_TEARDOWN: Duration = Duration::from_secs(10);

/// A target the host added.
pub(super) struct TargetRun {
    pub(super) target: Target,
    /// Every member's options and counters, also for members out of view,
    /// which join when discovery sees them again (P181).
    members: BTreeMap<DeviceKey, (SpeakerOptions, Arc<SpeakerCounters>)>,
    /// Every member plays over AirPlay 2 (P177).
    airplay2: bool,
    /// Its control device played by itself when it was added: every member
    /// was refused (P177).
    refused: bool,
    /// The members last reported missing (P181).
    missing: Vec<DeviceKey>,
}

impl Driver {
    pub(super) fn add_target(
        &mut self,
        target: Target,
        members: Vec<(DeviceKey, SpeakerOptions, Arc<SpeakerCounters>)>,
        now: Instant,
    ) {
        if self.stopping.is_some() {
            return;
        }
        let id = target.id.clone();
        if let Some(run) = self.driven.get_mut(&id) {
            for (key, options, counters) in &members {
                run.members
                    .insert(key.clone(), (options.clone(), counters.clone()));
            }
            for (key, options, counters) in members {
                let device = self
                    .speakers
                    .get(&key)
                    .map(|speaker| speaker.device.clone())
                    .or_else(|| self.directory.device(&key).cloned());
                if let Some(device) = device {
                    self.insert_member(&id, device, options, counters, now);
                }
            }
            return;
        }
        let in_view: Vec<Device> = target
            .members
            .iter()
            .filter_map(|member| {
                let device = self.directory.device(&member.key).cloned();
                device.or_else(|| member.device.as_deref().cloned())
            })
            .collect();
        let refused = target
            .control
            .as_ref()
            .and_then(|control| self.speakers.get(control))
            .is_some_and(|speaker| !speaker.removing);
        // The members' options carry the host's stored credentials before
        // any member is a speaker of this run.
        let paired = |key: &DeviceKey| {
            self.homekit_paired(key)
                || members.iter().any(|(member, options, _)| {
                    member == key && options.homekit_credentials.is_some()
                })
        };
        let airplay2 = self.airplay2
            && in_view.iter().any(|device| {
                device.choose_path_preferring(
                    true,
                    self.airplay2_preferred,
                    None,
                    paired(&device.key),
                ) == Ok(ProtocolPath::AirPlay2)
            });
        self.driven.insert(
            id.clone(),
            TargetRun {
                target,
                members: members
                    .into_iter()
                    .map(|(key, options, counters)| (key, (options, counters)))
                    .collect(),
                airplay2,
                refused,
                missing: Vec::new(),
            },
        );
        for device in in_view {
            let Some((options, counters)) = self
                .driven
                .get(&id)
                .and_then(|run| run.members.get(&device.key))
                .cloned()
            else {
                continue;
            };
            if refused {
                let key = device.key.clone();
                if self.insert_speaker(&id, device, options, counters) {
                    let error = ErrorKind::Unsupported(UnsupportedReason::PlaysThroughSpeakers);
                    self.refuse(&key, SessionError::new(error), now);
                }
            } else {
                self.insert_member(&id, device, options, counters, now);
            }
        }
    }

    /// Makes the device a speaker of the target; false, changing nothing,
    /// for a speaker the host added on its own, which stays the host's.
    fn insert_speaker(
        &mut self,
        id: &TargetId,
        device: Device,
        options: SpeakerOptions,
        counters: Arc<SpeakerCounters>,
    ) -> bool {
        let key = device.key.clone();
        if let Some(speaker) = self.speakers.get_mut(&key) {
            if speaker.target.as_ref() != Some(id) && !speaker.removing {
                return false;
            }
            speaker.target = Some(id.clone());
            return true;
        }
        self.speakers.insert(
            key,
            Speaker {
                device,
                options,
                counters,
                backoff: Backoff::new(),
                phase: Phase::Idle,
                removing: false,
                airplay1_outcome: None,
                ap2_waiting: false,
                ap2_refusal: 0,
                ap2_retry: false,
                reads_volume: false,
                target: Some(id.clone()),
            },
        );
        true
    }

    /// Adds one member as a speaker of the target and starts its session.
    fn insert_member(
        &mut self,
        id: &TargetId,
        device: Device,
        options: SpeakerOptions,
        counters: Arc<SpeakerCounters>,
        now: Instant,
    ) {
        let key = device.key.clone();
        if self.speakers.contains_key(&key) {
            if self.insert_speaker(id, device.clone(), options.clone(), counters.clone()) {
                self.add_speaker(device, options, counters, now);
            }
            return;
        }
        self.insert_speaker(id, device, options, counters);
        self.start_session(&key, now);
    }

    pub(super) fn remove_target(&mut self, id: &TargetId, now: Instant) {
        if self.driven.remove(id).is_none() {
            return;
        }
        let keys: Vec<DeviceKey> = self
            .speakers
            .iter()
            .filter(|(_, speaker)| speaker.target.as_ref() == Some(id))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.remove_speaker(&key, now);
        }
    }

    /// The members of a target that are speakers now.
    pub(super) fn target_members(&self, id: &TargetId) -> Vec<DeviceKey> {
        self.speakers
            .iter()
            .filter(|(_, speaker)| speaker.target.as_ref() == Some(id) && !speaker.removing)
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// One delay for every member, also those that join later (P179).
    pub(super) fn set_target_delay(&mut self, id: &TargetId, delay: Duration, now: Instant) {
        if let Some(run) = self.driven.get_mut(id) {
            for (options, _) in run.members.values_mut() {
                options.delay = delay;
            }
        }
        for key in self.target_members(id) {
            self.command(Command::SetDelay(key, delay), now);
        }
    }

    /// One volume for every member, also those that join later (P180).
    pub(super) fn set_target_volume(&mut self, id: &TargetId, volume: Volume, now: Instant) {
        if matches!(volume, Volume::Db(db) if db.is_nan()) {
            return;
        }
        if let Some(run) = self.driven.get_mut(id) {
            for (options, _) in run.members.values_mut() {
                options.volume = volume;
            }
        }
        for key in self.target_members(id) {
            self.set_volume(&key, volume, now);
        }
    }

    /// Asks the leader, or the first member that plays (P180); on the
    /// AirPlay 2 path only that member's session reads (P184).
    pub(super) fn read_target_volume(&mut self, id: &TargetId, now: Instant) {
        let Some(run) = self.driven.get(id) else {
            return;
        };
        let plays = |key: &DeviceKey| {
            self.speakers
                .get(key)
                .is_some_and(|speaker| match &speaker.phase {
                    Phase::Active(active) => active.stream_id.is_some(),
                    Phase::Ap2(active) => active.stream_id.is_some(),
                    _ => false,
                })
        };
        let leader = run.target.leader.clone().filter(|key| plays(key));
        let source = leader.or_else(|| {
            run.target
                .members
                .iter()
                .map(|member| member.key.clone())
                .find(|key| plays(key))
        });
        if let Some(key) = source {
            self.read_volume(&key, now);
        }
    }

    /// The speaker's target plays over AirPlay 2 (P177).
    pub(super) fn target_on_airplay2(&self, speaker: &Speaker) -> bool {
        speaker
            .target
            .as_ref()
            .and_then(|id| self.driven.get(id))
            .is_some_and(|run| run.airplay2)
    }

    /// Two speakers are members of one target, so they share one AirPlay 2
    /// place (P177).
    pub(super) fn same_target(&self, a: &DeviceKey, b: &DeviceKey) -> bool {
        let target = |key: &DeviceKey| self.speakers.get(key).and_then(|s| s.target.clone());
        target(a).is_some_and(|id| target(b) == Some(id))
    }

    /// A member's AirPlay 1 attempt ended in Unsupported or Refused: the
    /// whole target moves to AirPlay 2 for the rest of this start, and the
    /// members already on AirPlay 1 start over (P177).
    pub(super) fn move_target_to_airplay2(
        &mut self,
        id: &TargetId,
        from: &DeviceKey,
        now: Instant,
    ) {
        let Some(run) = self.driven.get_mut(id) else {
            return;
        };
        if run.airplay2 {
            return;
        }
        run.airplay2 = true;
        let keys: Vec<DeviceKey> = self
            .speakers
            .iter()
            .filter(|(key, speaker)| {
                *key != from
                    && speaker.target.as_ref() == Some(id)
                    && !speaker.removing
                    && speaker.device.verdict.airplay2
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            let Some(speaker) = self.speakers.get_mut(&key) else {
                continue;
            };
            match &mut speaker.phase {
                Phase::Active(active) => {
                    active.session.teardown(now);
                    self.note_teardown(key, now);
                }
                Phase::Waiting(_) => speaker.phase = Phase::Waiting(now),
                _ => {}
            }
        }
    }

    /// Within 10 s of the sender's own TEARDOWN to the member, a refused
    /// connection or a pairing request is a loss, retried once (P181).
    pub(super) fn settling(&self, key: &DeviceKey, kind: ErrorKind, now: Instant) -> bool {
        matches!(
            kind,
            ErrorKind::Unreachable | ErrorKind::Refused | ErrorKind::PairingRequired
        ) && self
            .teardowns
            .get(key)
            .is_some_and(|at| now.saturating_duration_since(*at) < SETTLE_AFTER_TEARDOWN)
    }

    /// Whether the device is the control device of a target the host plays
    /// (P177).
    pub(super) fn controls_driven_target(&self, key: &DeviceKey) -> bool {
        self.driven
            .values()
            .any(|run| !run.refused && run.target.control.as_ref() == Some(key))
    }

    /// The members of a target waiting for the AirPlay 2 place start with
    /// the one that got it (P169, P177).
    pub(super) fn wake_target_waiters(&mut self, id: &TargetId, now: Instant) {
        let keys: Vec<DeviceKey> = self
            .speakers
            .iter_mut()
            .filter(|(_, speaker)| {
                speaker.target.as_ref() == Some(id)
                    && speaker.ap2_waiting
                    && !speaker.removing
                    && matches!(speaker.phase, Phase::Failed)
            })
            .map(|(key, speaker)| {
                speaker.ap2_waiting = false;
                speaker.phase = Phase::Waiting(now);
                key.clone()
            })
            .collect();
        for key in keys {
            self.emit_speaker(
                &key,
                SpeakerEvent::State(SpeakerState::Retrying { at: now }),
            );
        }
    }

    /// The target of every device that is a member of one (P176).
    pub(super) fn target_refs(&self) -> BTreeMap<DeviceKey, TargetId> {
        self.targets
            .iter()
            .flat_map(|target| {
                target
                    .members
                    .iter()
                    .map(|member| (member.key.clone(), target.id.clone()))
            })
            .collect()
    }

    /// A member of a target the host plays came into view: it joins (P181).
    pub(super) fn rejoin_member(&mut self, key: &DeviceKey, now: Instant) {
        if self.speakers.contains_key(key) || self.stopping.is_some() {
            return;
        }
        let Some((id, (options, counters))) = self.driven.iter().find_map(|(id, run)| {
            let member = run.members.get(key).filter(|_| !run.refused)?;
            Some((id.clone(), member.clone()))
        }) else {
            return;
        };
        let Some(device) = self.directory.device(key).cloned() else {
            return;
        };
        self.insert_member(&id, device, options, counters, now);
    }

    /// Whether the speaker plays as a member of a target the host added.
    pub(super) fn is_target_member(&self, key: &DeviceKey) -> bool {
        self.speakers
            .get(key)
            .is_some_and(|speaker| speaker.target.is_some() && !speaker.removing)
    }

    /// The members in view of the target `key` is a member of: as
    /// discovery lists them now, or as the host gave them for a target
    /// discovery doesn't list.
    pub(super) fn members_in_view(&self, key: &DeviceKey) -> Vec<&Device> {
        let Some(id) = self.speakers.get(key).and_then(|s| s.target.as_ref()) else {
            return Vec::new();
        };
        let Some(run) = self.driven.get(id) else {
            return Vec::new();
        };
        let target = self.targets.get(id).unwrap_or(&run.target);
        target
            .members
            .iter()
            .filter_map(|member| member.device.as_deref())
            .collect()
    }

    /// Addresses of the other members in view, for a member's SETPEERS
    /// (P179).
    pub(super) fn other_member_addresses(&self, key: &DeviceKey) -> Vec<std::net::IpAddr> {
        self.members_in_view(key)
            .into_iter()
            .filter(|device| device.key != *key)
            .filter_map(|device| {
                device
                    .airplay_socket_addrs()
                    .first()
                    .map(|address| address.ip())
            })
            .collect()
    }

    /// Notes the sender's own TEARDOWN to a member; older notes are past
    /// their window and go (P181).
    pub(super) fn note_teardown(&mut self, key: DeviceKey, now: Instant) {
        self.teardowns
            .retain(|_, at| now.saturating_duration_since(*at) < SETTLE_AFTER_TEARDOWN);
        self.teardowns.insert(key, now);
    }

    /// Reports the members of each target that play nothing now, when that
    /// changed (P181).
    pub(super) fn report_missing_members(&mut self) {
        if self.driven.is_empty() {
            return;
        }
        let missing = |speakers: &BTreeMap<DeviceKey, Speaker>, key: &DeviceKey| {
            speakers.get(key).is_none_or(|speaker| {
                matches!(
                    speaker.phase,
                    Phase::Waiting(_) | Phase::Unannounced | Phase::Failed
                )
            })
        };
        let mut changed = Vec::new();
        for (id, run) in &self.driven {
            if run.refused {
                continue;
            }
            let now_missing = run
                .members
                .keys()
                .filter(|key| missing(&self.speakers, key));
            if !now_missing.clone().eq(run.missing.iter()) {
                changed.push((id.clone(), now_missing.cloned().collect::<Vec<_>>()));
            }
        }
        for (id, missing) in changed {
            if let Some(run) = self.driven.get_mut(&id) {
                run.missing = missing.clone();
            }
            self.emit(SenderEvent::TargetMissing { id, missing });
        }
    }
}
