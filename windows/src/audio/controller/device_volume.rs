//! An output device's own volume and mute (spec S192 to S202): which outputs
//! are followed, their readings, the log lines and the fix.

#[cfg(test)]
mod tests {
    use std::path::Path;

    use audiosync_core::policy::device_volume::{DeviceVolume, Hint};

    use crate::audio::backend::{AudioBackend, BackendError, BackendEvent, SourceId};
    use crate::audio::mock::MockBackend;
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
