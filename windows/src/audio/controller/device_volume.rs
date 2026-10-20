//! An output device's own volume and mute (spec S192 to S202): which outputs
//! are followed, their readings, the log lines and the fix.
//!
//! Windows keeps a volume and a mute for every endpoint and applies them
//! after AudioSync's gain, so an output left at 13 % in Windows sounds very
//! quiet whatever AudioSync's slider says. The controller follows the own
//! volume of every output that is on and connected, and of no other: it
//! reads it once when the output is switched on or its device comes back,
//! and again whenever Windows reports a change (the driver's notification
//! only marks the endpoint; the read happens here, on the thread that polls).
//! It lets go of an output switched off, unplugged, or at quit. While
//! Windows audio hangs, an output whose listener, read or fix doesn't answer
//! in time shows no device volume and is tried again some seconds later. The
//! core (`audiosync_core::policy::device_volume`) decides what a reading
//! means and what the fix writes; the only write is that fix, when the user
//! clicks it, and the row follows what Windows reports afterwards, never the
//! values written. None of this touches an audio thread, the renderers or
//! AudioSync's own volumes.

use std::collections::BTreeMap;

use audiosync_core::policy::device_volume::{DeviceVolume, decision_change};

use super::{Controller, Driver, STALL_RETRY_POLLS, label};
use crate::audio::backend::{BackendError, BackendEvent};
use crate::audio::routing::EndpointVolume;

/// The outputs whose own volume is followed, by endpoint ID, each with its
/// last reading.
#[derive(Debug, Default)]
pub(super) struct Followed {
    readings: BTreeMap<String, DeviceVolume>,
    /// The outputs to follow whose listener, read or fix didn't answer in
    /// time ([`BackendError::NotResponding`]: Windows audio hangs), each with
    /// the polls left before the next try ([`STALL_RETRY_POLLS`]). They show
    /// no device volume meanwhile and aren't read, since a read would wait
    /// for Windows audio too.
    stalled: BTreeMap<String, u32>,
}

impl Followed {
    /// The last reading of output `id`, or `None` while it isn't followed.
    pub(super) fn reading(&self, id: &str) -> Option<DeviceVolume> {
        self.readings.get(id).copied()
    }

    /// One poll passed: each stalled output's next try comes a poll closer.
    pub(super) fn tick(&mut self) {
        for wait in self.stalled.values_mut() {
            *wait = wait.saturating_sub(1);
        }
    }
}

/// A reading as the core decides on it: both parts, or neither when Windows
/// can't activate or read the endpoint's volume control (S194). Callers deal
/// with a read that didn't answer in time first ([`Controller::stalls`]).
fn device_volume(volume: Result<EndpointVolume, BackendError>) -> DeviceVolume {
    volume.map_or(DeviceVolume::UNKNOWN, |volume| DeviceVolume {
        volume: Some(f64::from(volume.level)),
        muted: Some(volume.muted),
    })
}

/// A reading as the log shows it: "13%, muted", or "unknown".
fn describe(reading: DeviceVolume) -> String {
    if reading == DeviceVolume::UNKNOWN {
        return "unknown".to_owned();
    }
    let volume = reading.percent().map_or_else(
        || "volume unknown".to_owned(),
        |percent| format!("{percent}%"),
    );
    let mute = match reading.muted {
        Some(true) => "muted",
        Some(false) => "not muted",
        None => "mute unknown",
    };
    format!("{volume}, {mute}")
}

/// How a write of the fix went, for the log.
fn outcome(result: &Result<(), BackendError>) -> String {
    match result {
        Ok(()) => "done".to_owned(),
        Err(error) => format!("failed: {error}"),
    }
}

impl<D: Driver> Controller<D> {
    /// Follows the own volume of every output that is on and connected, and
    /// of no other (S192, S195): an output newly on, or whose device came
    /// back, is registered with Windows and read once, with a line in the
    /// log (S201); one switched off or gone is let go. Called whenever the
    /// outputs that are on, or the connected ones, may have changed.
    pub(super) fn follow_device_volumes(&mut self) {
        let wanted: Vec<String> = self
            .present
            .iter()
            .filter(|endpoint| self.policy.settings(&endpoint.id).enabled)
            .map(|endpoint| endpoint.id.clone())
            .collect();
        let dropped: Vec<String> = self
            .device_volumes
            .readings
            .keys()
            .filter(|id| !wanted.contains(id))
            .cloned()
            .collect();
        for id in dropped {
            self.driver.unwatch_endpoint_volume(&id);
            self.device_volumes.readings.remove(&id);
        }
        self.device_volumes
            .stalled
            .retain(|id, _| wanted.contains(id));
        for id in wanted {
            if self.device_volumes.readings.contains_key(&id)
                || self
                    .device_volumes
                    .stalled
                    .get(&id)
                    .is_some_and(|wait| *wait > 0)
            {
                continue;
            }
            let retried = self.device_volumes.stalled.contains_key(&id);
            let watched = self.driver.watch_endpoint_volume(&id);
            // A listener that didn't register in time leaves the device
            // unread: the read would wait for Windows audio too.
            let read = if watched == Err(BackendError::NotResponding) {
                Err(BackendError::NotResponding)
            } else {
                self.driver.endpoint_volume(&id)
            };
            if self.stalls(&id, &read) {
                continue;
            }
            let reading = device_volume(read);
            self.log.log(|| {
                // Windows can set both parts whenever it can read them.
                let settable = if reading == DeviceVolume::UNKNOWN {
                    "not settable"
                } else {
                    "settable"
                };
                let followed = match &watched {
                    Ok(()) => String::new(),
                    Err(error) => format!("; its changes aren't followed: {error}"),
                };
                format!(
                    "output {} device volume: {}; volume and mute {settable}{followed}",
                    label(&self.names, &id),
                    describe(reading)
                )
            });
            // Back after a stall, outside any change the user made: the row
            // shows it once told.
            if retried && reading.hint().is_some() {
                self.pending
                    .push(BackendEvent::DeviceVolumeChanged { id: id.clone() });
            }
            self.device_volumes.readings.insert(id, reading);
        }
    }

    /// Reads again each followed output that Windows reported a change for
    /// (S195, S202): both parts, whatever the notification said. A new
    /// reading that changes what the row shows is reported as
    /// [`BackendEvent::DeviceVolumeChanged`]; one whose decision changed
    /// gets a line in the log, others none (S201).
    pub(super) fn read_device_volume_changes(&mut self) {
        for id in self.driver.endpoint_volume_changes() {
            self.read_device_volume_again(id);
        }
    }

    /// Whether `result`, of a call on output `id`'s own volume, is Windows
    /// audio not answering in time: then the output stalls
    /// ([`Controller::stall_device_volume`]). Any other result ends a stall.
    fn stalls<T>(&mut self, id: &str, result: &Result<T, BackendError>) -> bool {
        if !matches!(result, Err(BackendError::NotResponding)) {
            self.device_volumes.stalled.remove(id);
            return false;
        }
        self.stall_device_volume(id);
        true
    }

    /// Windows audio didn't answer in time for output `id`'s own volume: its
    /// listener goes, its row shows no device volume (a hint it showed goes,
    /// [`BackendEvent::DeviceVolumeChanged`]) and it isn't read, and it is
    /// tried again [`STALL_RETRY_POLLS`] polls later. The log says so once
    /// until a try works.
    fn stall_device_volume(&mut self, id: &str) {
        self.driver.unwatch_endpoint_volume(id);
        if let Some(before) = self.device_volumes.readings.remove(id)
            && before.hint().is_some()
        {
            self.pending
                .push(BackendEvent::DeviceVolumeChanged { id: id.to_owned() });
        }
        let first = self
            .device_volumes
            .stalled
            .insert(id.to_owned(), STALL_RETRY_POLLS)
            .is_none();
        if first {
            self.log.log(|| {
                format!(
                    "output {} device volume: Windows audio didn't answer in time; none shown \
                     for now, tried again in about ten seconds",
                    label(&self.names, id)
                )
            });
        }
    }

    /// Registers every followed output's listener again and reads it afresh,
    /// after Windows reported a change of the outputs (S202): a device
    /// unplugged and plugged in again between two listings is listed as
    /// before, but its old listener went with it. One whose listener or read
    /// doesn't answer in time shows no device volume until a later try works.
    pub(super) fn renew_device_volumes(&mut self) {
        let followed: Vec<String> = self.device_volumes.readings.keys().cloned().collect();
        for id in followed {
            self.driver.unwatch_endpoint_volume(&id);
            let watched = self.driver.watch_endpoint_volume(&id);
            if self.stalls(&id, &watched) {
                continue;
            }
            if let Err(error) = watched {
                self.log.log(|| {
                    format!(
                        "output {} device volume: its changes aren't followed: {error}",
                        label(&self.names, &id)
                    )
                });
            }
            self.read_device_volume_again(id);
        }
    }

    /// Reads followed output `id` again, both parts: logs a change of its
    /// decision (S201), and reports a change of what its row shows. A read
    /// that doesn't answer in time stalls the output instead.
    fn read_device_volume_again(&mut self, id: String) {
        let Some(before) = self.device_volumes.reading(&id) else {
            return;
        };
        let read = self.driver.endpoint_volume(&id);
        if self.stalls(&id, &read) {
            return;
        }
        let reading = device_volume(read);
        if let Some((from, to)) = decision_change(Some(before.decision()), reading.decision()) {
            self.log.log(|| {
                format!(
                    "output {} device volume: {from} → {to} ({})",
                    label(&self.names, &id),
                    describe(reading)
                )
            });
        }
        self.device_volumes.readings.insert(id.clone(), reading);
        if reading.hint() != before.hint() {
            self.pending.push(BackendEvent::DeviceVolumeChanged { id });
        }
    }

    /// The fix the user clicked under output `id` (S197): reads the device
    /// again, since its volume may have changed after the row last did (a
    /// notification not polled yet, or a listener that couldn't register),
    /// and the fix must never lower it; writes what the core decides for
    /// that reading, once each, and logs what it wrote and how each write
    /// went (S201). The row keeps its reading until Windows reports the
    /// change, so a write the device ignores leaves the hint and the fix
    /// showing. An output that isn't followed, or has nothing to fix,
    /// writes nothing. While Windows audio doesn't answer in time, nothing
    /// more is written, and the output stalls: what the device holds isn't
    /// known until a later read.
    pub(super) fn fix_followed_device_volume(&mut self, id: &str) {
        if self.device_volumes.reading(id).is_none() {
            return;
        }
        let read = self.driver.endpoint_volume(id);
        if let Err(error @ BackendError::NotResponding) = &read {
            self.log.log(|| {
                format!(
                    "output {} device volume fix: nothing written: {error}",
                    label(&self.names, id)
                )
            });
            self.stall_device_volume(id);
            return;
        }
        let fix = device_volume(read).fix();
        if fix.is_empty() {
            return;
        }
        let level = fix
            .volume
            .map(|volume| (volume, self.driver.set_endpoint_level(id, volume as f32)));
        let unmuted = fix.unmute.then(|| self.driver.unmute_endpoint(id));
        self.log.log(|| {
            let mut written = Vec::new();
            if let Some((volume, result)) = &level {
                written.push(format!(
                    "volume set to {}%: {}",
                    (volume * 100.0).round(),
                    outcome(result)
                ));
            }
            if let Some(result) = &unmuted {
                written.push(format!("unmuted: {}", outcome(result)));
            }
            format!(
                "output {} device volume fix: {}",
                label(&self.names, id),
                written.join("; ")
            )
        });
        let hung = |result: Option<&Result<(), BackendError>>| {
            matches!(result, Some(Err(BackendError::NotResponding)))
        };
        if hung(level.as_ref().map(|(_, result)| result)) || hung(unmuted.as_ref()) {
            self.stall_device_volume(id);
        }
    }

    /// Lets go of every output's own volume: AudioSync quits.
    pub(super) fn stop_following_device_volumes(&mut self) {
        for id in std::mem::take(&mut self.device_volumes.readings).into_keys() {
            self.driver.unwatch_endpoint_volume(&id);
        }
        self.device_volumes.stalled.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use audiosync_core::policy::device_volume::{DeviceVolume, Hint};

    use super::super::STALL_RETRY_POLLS;
    use crate::audio::backend::{AudioBackend, BackendError, BackendEvent, SourceId};
    use crate::audio::mock::{MockBackend, VolumeWrite};
    use crate::debug_log::{DebugLog, FILE_NAME, TEST_FLUSH, read_messages, test_dir};

    const SPEAKERS: &str = "speakers";
    const HEADPHONES: &str = "headphones";

    /// A stopped mock with two outputs, both on and listed once, as at a
    /// launch.
    fn backend() -> MockBackend {
        let mut backend = MockBackend::empty();
        backend.connect_output(SPEAKERS, "Speakers");
        backend.connect_output(HEADPHONES, "Headphones");
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        backend.set_output_enabled(HEADPHONES, true).unwrap();
        backend.list_outputs().unwrap();
        backend
    }

    /// What the backend shows of output `id`'s device volume.
    fn device_volume(backend: &MockBackend, id: &str) -> Option<DeviceVolume> {
        backend
            .outputs()
            .into_iter()
            .find(|output| output.id == id)
            .and_then(|output| output.device_volume)
    }

    fn hint(backend: &MockBackend, id: &str) -> Option<Hint> {
        device_volume(backend, id).and_then(DeviceVolume::hint)
    }

    fn at(volume: f64, muted: bool) -> Option<DeviceVolume> {
        Some(DeviceVolume {
            volume: Some(volume),
            muted: Some(muted),
        })
    }

    /// The outputs whose device volume changed, by the events.
    fn changed(events: &[BackendEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                BackendEvent::DeviceVolumeChanged { id } => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }

    // S195, S202
    #[test]
    fn an_endpoint_volume_notification_updates_the_row() {
        let mut backend = backend();
        let record = backend.volume_record();
        assert_eq!(
            record.watched(),
            [HEADPHONES, SPEAKERS],
            "one listener for each output that is on"
        );
        assert_eq!(device_volume(&backend, SPEAKERS), at(1.0, false));
        backend.start(&SourceId::WholeSystem).unwrap();
        let gain = backend.playing_params(SPEAKERS).unwrap().gain;
        let own_volume = backend.policy().settings(SPEAKERS).volume;
        backend.poll();

        // Windows turns the speakers down twice before the next poll, which
        // reads them again and says so once.
        backend.set_output_level(SPEAKERS, 0.5);
        backend.set_output_level(SPEAKERS, 0.125);
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));
        assert!(changed(&backend.poll()).is_empty(), "nothing new");
        // A notification that changes nothing the row shows says nothing.
        backend.notify_volume(SPEAKERS);
        assert!(changed(&backend.poll()).is_empty());
        backend.mute_output(SPEAKERS, true);
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(
            hint(&backend, SPEAKERS),
            Some(Hint::MutedAndLow { percent: 13 })
        );
        // The device volume is never AudioSync's own (S192).
        assert_eq!(backend.playing_params(SPEAKERS).unwrap().gain, gain);
        assert_eq!(backend.policy().settings(SPEAKERS).volume, own_volume);

        // Switched off, the output has no listener and shows nothing;
        // switched on again, it is read afresh.
        backend.set_output_enabled(SPEAKERS, false).unwrap();
        assert_eq!(record.watched(), [HEADPHONES]);
        assert_eq!(device_volume(&backend, SPEAKERS), None);
        backend.mute_output(SPEAKERS, false);
        assert!(
            changed(&backend.poll()).is_empty(),
            "not followed while off"
        );
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        assert_eq!(record.watched(), [HEADPHONES, SPEAKERS]);
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));

        // A device that goes away drops its listener; back, it is read
        // afresh.
        backend.disconnect_output(HEADPHONES);
        backend.poll();
        assert_eq!(record.watched(), [SPEAKERS]);
        backend.set_output_level(HEADPHONES, 0.25);
        backend.connect_output(HEADPHONES, "Headphones");
        backend.poll();
        assert_eq!(record.watched(), [HEADPHONES, SPEAKERS]);
        assert_eq!(device_volume(&backend, HEADPHONES), at(0.25, false));

        // Quitting removes every listener.
        drop(backend);
        assert!(record.watched().is_empty());
    }

    // S194, S196
    #[test]
    fn an_endpoint_whose_volume_cannot_be_read_shows_nothing() {
        let mut backend = MockBackend::empty();
        backend.connect_output(SPEAKERS, "Speakers");
        backend.hide_output_volume(SPEAKERS);
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        backend.list_outputs().unwrap();
        let record = backend.volume_record();
        assert_eq!(
            device_volume(&backend, SPEAKERS),
            Some(DeviceVolume::UNKNOWN),
            "both parts unknown"
        );
        assert_eq!(hint(&backend, SPEAKERS), None);
        backend.fix_device_volume(SPEAKERS);
        assert!(record.writes().is_empty(), "nothing to fix");
        // Readable again, the next notification shows it.
        backend.set_output_level(SPEAKERS, 0.125);
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));
        // An output switched on that isn't connected is read once it is.
        backend.set_output_enabled("usb", true).unwrap();
        assert_eq!(record.watched(), [SPEAKERS]);
        assert_eq!(device_volume(&backend, "usb"), None);
    }

    // S197
    #[test]
    fn the_fix_reads_the_device_again_and_never_lowers_a_volume_raised_meanwhile() {
        let mut backend = backend();
        let record = backend.volume_record();
        backend.set_output_level(SPEAKERS, 0.125);
        backend.poll();
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));
        // Raised to 90 % in Windows, and clicked before the next poll.
        backend.set_output_level(SPEAKERS, 0.9);
        backend.fix_device_volume(SPEAKERS);
        assert!(record.writes().is_empty(), "never lowered to 75 %");
        // That read doesn't update the row; Windows' notification does.
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));
        backend.poll();
        assert_eq!(hint(&backend, SPEAKERS), None);
        // Muted meanwhile at a fine volume: only unmuted.
        backend.set_output_level(SPEAKERS, 0.125);
        backend.poll();
        backend.set_output_level(SPEAKERS, 0.5);
        backend.mute_output(SPEAKERS, true);
        backend.fix_device_volume(SPEAKERS);
        assert_eq!(
            record.writes(),
            [VolumeWrite::Unmute {
                id: SPEAKERS.to_owned()
            }]
        );
    }

    // S195, S202
    #[test]
    fn an_output_replugged_between_two_listings_is_followed_again() {
        let mut backend = backend();
        let record = backend.volume_record();
        backend.poll();
        // Unplugged and plugged in again before the next listing: the list
        // looks the same, but the old listener went with the device.
        backend.disconnect_output(SPEAKERS);
        backend.set_output_level(SPEAKERS, 0.25);
        backend.connect_output(SPEAKERS, "Speakers");
        backend.poll();
        assert_eq!(record.watched(), [HEADPHONES, SPEAKERS]);
        assert_eq!(device_volume(&backend, SPEAKERS), at(0.25, false));
        backend.set_output_level(SPEAKERS, 0.125);
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));
    }

    // S195, S202
    #[test]
    fn an_output_whose_volume_listener_doesnt_answer_shows_no_device_volume_until_a_later_try() {
        let dir = test_dir("controller-device-volume-stalled");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut backend = MockBackend::empty();
        backend.set_debug_log(log.clone());
        backend.connect_output(SPEAKERS, "Speakers");
        backend.connect_output(HEADPHONES, "Headphones");
        backend.list_outputs().unwrap();
        let record = backend.volume_record();
        // Windows audio hangs: the listener isn't there in time, and the
        // device isn't read, since that would wait for Windows audio too.
        backend.stall_device_volumes(true);
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        assert!(record.watched().is_empty());
        assert!(record.reads().is_empty(), "{:?}", record.reads());
        assert_eq!(device_volume(&backend, SPEAKERS), None, "nothing for now");
        // Not tried at every poll: once Windows audio answers again, the
        // next try comes some seconds after the last.
        poll_times(&mut backend, STALL_RETRY_POLLS);
        backend.stall_device_volumes(false);
        poll_times(&mut backend, STALL_RETRY_POLLS - 1);
        assert!(record.watched().is_empty());
        backend.poll();
        assert_eq!(record.watched(), [SPEAKERS]);
        assert_eq!(device_volume(&backend, SPEAKERS), at(1.0, false));

        // Windows reports a change of the outputs while it hangs again: the
        // listener can't be renewed, so the row shows nothing for now.
        backend.set_output_level(SPEAKERS, 0.125);
        backend.poll();
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));
        backend.stall_device_volumes(true);
        let reads = record.reads().len();
        backend.connect_output("usb", "USB DAC");
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(device_volume(&backend, SPEAKERS), None);
        assert_eq!(record.reads().len(), reads, "not read");
        backend.stall_device_volumes(false);
        poll_times(&mut backend, STALL_RETRY_POLLS);
        assert_eq!(record.watched(), [SPEAKERS]);
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));

        // Said once each time it stops answering, not at every try.
        let stalled: Vec<String> = logged(&log, &dir)
            .into_iter()
            .filter(|message| message.contains("didn't answer"))
            .collect();
        assert_eq!(
            stalled,
            [
                "output \"Speakers\" (speakers) device volume: Windows audio didn't answer in \
                 time; none shown for now, tried again in about ten seconds",
                "output \"Speakers\" (speakers) device volume: Windows audio didn't answer in \
                 time; none shown for now, tried again in about ten seconds",
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The log lines about device volumes that didn't answer in time.
    fn stall_lines(log: &DebugLog, dir: &Path) -> Vec<String> {
        logged(log, dir)
            .into_iter()
            .filter(|message| message.contains("didn't answer"))
            .collect()
    }

    const STALLED: &str = "output \"Speakers\" (speakers) device volume: Windows audio didn't \
                           answer in time; none shown for now, tried again in about ten seconds";

    // S195, S202
    #[test]
    fn a_device_volume_read_that_doesnt_answer_shows_none_until_a_later_try() {
        let dir = test_dir("controller-device-volume-read-stalled");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut backend = MockBackend::empty();
        backend.set_debug_log(log.clone());
        backend.connect_output(SPEAKERS, "Speakers");
        backend.list_outputs().unwrap();
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        let record = backend.volume_record();
        backend.set_output_level(SPEAKERS, 0.125);
        backend.poll();
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));

        // Windows reports a change and then hangs in the read: the row shows
        // no device volume for now rather than one that may be stale, and
        // the device isn't read at every poll.
        backend.stall_device_volumes(true);
        backend.set_output_level(SPEAKERS, 0.5);
        let reads = record.reads().len();
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(device_volume(&backend, SPEAKERS), None);
        assert!(record.watched().is_empty(), "its listener goes too");
        poll_times(&mut backend, STALL_RETRY_POLLS - 1);
        assert_eq!(record.reads().len(), reads + 1, "read once");
        // Windows audio answers again: the next try shows it, and says so.
        backend.stall_device_volumes(false);
        backend.set_output_level(SPEAKERS, 0.125);
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(record.watched(), [SPEAKERS]);
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));
        assert_eq!(stall_lines(&log, &dir), [STALLED]);
        let _ = std::fs::remove_dir_all(dir);
    }

    // S197
    #[test]
    fn a_fix_that_windows_audio_doesnt_answer_shows_no_device_volume_until_a_later_try() {
        let dir = test_dir("controller-device-volume-fix-stalled");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut backend = MockBackend::empty();
        backend.set_debug_log(log.clone());
        backend.connect_output(SPEAKERS, "Speakers");
        backend.list_outputs().unwrap();
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        let record = backend.volume_record();
        backend.set_output_level(SPEAKERS, 0.125);
        backend.poll();

        // Windows audio hangs when the user clicks the fix: the read before
        // the writes doesn't answer, so nothing is written, and the hint and
        // its fix go until Windows audio answers again.
        backend.stall_device_volumes(true);
        backend.fix_device_volume(SPEAKERS);
        assert!(record.writes().is_empty(), "{:?}", record.writes());
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(device_volume(&backend, SPEAKERS), None);
        backend.stall_device_volumes(false);
        poll_times(&mut backend, STALL_RETRY_POLLS);
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));

        // A write that doesn't answer in time leaves the device as it may
        // be: no device volume either until the next try reads it.
        backend.refuse_volume_writes(Some(BackendError::NotResponding));
        backend.fix_device_volume(SPEAKERS);
        assert_eq!(
            record.writes(),
            [VolumeWrite::Level {
                id: SPEAKERS.to_owned(),
                level: 0.75
            }]
        );
        assert_eq!(changed(&backend.poll()), [SPEAKERS]);
        assert_eq!(device_volume(&backend, SPEAKERS), None);
        backend.refuse_volume_writes(None);
        poll_times(&mut backend, STALL_RETRY_POLLS);
        assert_eq!(hint(&backend, SPEAKERS), Some(Hint::Low { percent: 13 }));

        let lines: Vec<String> = logged(&log, &dir)
            .into_iter()
            .filter(|message| message.contains("fix") || message.contains("didn't answer"))
            .collect();
        assert_eq!(
            lines,
            [
                "output \"Speakers\" (speakers) device volume fix: nothing written: Windows \
                 audio didn't answer in time.",
                STALLED,
                "output \"Speakers\" (speakers) device volume fix: volume set to 75%: failed: \
                 Windows audio didn't answer in time.",
                STALLED,
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // A level write that doesn't answer leaves Windows audio hung: the
    // unmute after it isn't tried, and the log says so rather than
    // reporting a failure that never happened.
    #[test]
    fn a_fix_whose_level_write_doesnt_answer_tries_no_unmute() {
        let dir = test_dir("controller-device-volume-fix-no-unmute");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut backend = MockBackend::empty();
        backend.set_debug_log(log.clone());
        backend.connect_output(SPEAKERS, "Speakers");
        backend.list_outputs().unwrap();
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        let record = backend.volume_record();
        backend.set_output_level(SPEAKERS, 0.125);
        backend.mute_output(SPEAKERS, true);
        backend.poll();

        backend.refuse_volume_writes(Some(BackendError::NotResponding));
        backend.fix_device_volume(SPEAKERS);
        assert_eq!(
            record.writes(),
            [VolumeWrite::Level {
                id: SPEAKERS.to_owned(),
                level: 0.75
            }]
        );
        let lines: Vec<String> = logged(&log, &dir)
            .into_iter()
            .filter(|message| message.contains("fix"))
            .collect();
        assert_eq!(
            lines,
            ["output \"Speakers\" (speakers) device volume fix: volume set to 75%: failed: \
              Windows audio didn't answer in time.; unmute not tried"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn poll_times(backend: &mut MockBackend, polls: u32) {
        for _ in 0..polls {
            backend.poll();
        }
    }

    fn logged(log: &DebugLog, dir: &Path) -> Vec<String> {
        assert!(log.flush(TEST_FLUSH), "the log is written");
        read_messages(&dir.join(FILE_NAME))
    }

    // S201
    #[test]
    fn a_volume_change_logs_only_when_its_decision_changes() {
        let dir = test_dir("controller-device-volume");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut backend = MockBackend::empty();
        backend.set_debug_log(log.clone());
        backend.connect_output(SPEAKERS, "Speakers");
        backend.connect_output(HEADPHONES, "Headphones");
        backend.set_output_level(SPEAKERS, 0.8);
        backend.hide_output_volume(HEADPHONES);
        backend.list_outputs().unwrap();
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        backend.set_output_enabled(HEADPHONES, true).unwrap();
        // Dragged from 80 % to 30 % (no line), on to 20 % (one) and 15 %
        // (none again).
        for level in [0.5, 0.3, 0.2, 0.15] {
            backend.set_output_level(SPEAKERS, level);
            backend.poll();
        }
        backend.fix_device_volume(SPEAKERS);
        backend.poll();
        backend.mute_output(SPEAKERS, true);
        backend.poll();
        backend.refuse_volume_writes(Some(BackendError::System {
            call: "IAudioEndpointVolume::SetMute",
            code: 0x8007_0005_u32 as i32,
        }));
        backend.fix_device_volume(SPEAKERS);
        backend.poll();
        let messages = logged(&log, &dir);
        let lines: Vec<&str> = messages
            .iter()
            .map(String::as_str)
            .filter(|message| message.contains("device volume"))
            .collect();
        assert_eq!(
            lines,
            [
                "output \"Speakers\" (speakers) device volume: 80%, not muted; \
                 volume and mute settable",
                "output \"Headphones\" (headphones) device volume: unknown; \
                 volume and mute not settable",
                "output \"Speakers\" (speakers) device volume: fine → low (20%, not muted)",
                "output \"Speakers\" (speakers) device volume fix: volume set to 75%: done",
                "output \"Speakers\" (speakers) device volume: low → fine (75%, not muted)",
                "output \"Speakers\" (speakers) device volume: fine → muted (75%, muted)",
                "output \"Speakers\" (speakers) device volume fix: unmuted: failed: \
                 IAudioEndpointVolume::SetMute failed (0x80070005).",
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
