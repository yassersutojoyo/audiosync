//! The device-volume hint and its fix through the adapter, the way the
//! windows drive them (spec S192 to S202): what a speaker row shows, and the
//! only writes, which a click makes.

use audiosync_core::policy::device_volume::{DeviceVolume, Hint};
use audiosync_core::volume::DEFAULT_LOCAL_POSITION;

use super::AudioAdapter;
use crate::DeviceVolumeHint;
use crate::audio::backend::AudioBackend;
use crate::audio::mock::{MockBackend, VolumeWrite};
use crate::debug_log::{DebugLog, test_dir};
use crate::settings::{self, SettingsFile};
use crate::ui_model::Backend;
use crate::view::speaker_item;

const SPEAKERS: &str = "speakers";
const HEADPHONES: &str = "headphones";
const PLAYER_PID: u32 = 2000;

/// The mock world: two outputs, both off, and one app.
fn audio() -> MockBackend {
    let mut audio = MockBackend::empty();
    audio.connect_output(SPEAKERS, "Speakers");
    audio.connect_output(HEADPHONES, "Headphones");
    audio.add_app(PLAYER_PID, "player.exe");
    audio
}

fn adapter(audio: MockBackend) -> AudioAdapter<MockBackend> {
    AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default())
}

/// The UI's ID of output `endpoint_id`.
fn key(endpoint_id: &str) -> String {
    format!("lc:{endpoint_id}")
}

fn hint(adapter: &AudioAdapter<MockBackend>, endpoint_id: &str) -> Option<Hint> {
    adapter
        .snapshot()
        .speaker(&key(endpoint_id))
        .expect("listed")
        .device_volume
}

/// What the main window's row of output `endpoint_id` shows of it.
fn row(adapter: &AudioAdapter<MockBackend>, endpoint_id: &str) -> (DeviceVolumeHint, i32) {
    let state = adapter.snapshot();
    let speaker = state.speaker(&key(endpoint_id)).expect("listed");
    let item = speaker_item(&state, speaker, false, false, None);
    (item.device_volume, item.device_volume_percent)
}

fn level(volume: f32) -> VolumeWrite {
    VolumeWrite::Level {
        id: SPEAKERS.to_owned(),
        level: volume,
    }
}

fn unmute() -> VolumeWrite {
    VolumeWrite::Unmute {
        id: SPEAKERS.to_owned(),
    }
}

// S196, S198
#[test]
fn a_switched_on_output_at_13_percent_shows_the_low_hint() {
    let mut audio = audio();
    audio.set_output_level(SPEAKERS, 0.13);
    audio.set_output_level(HEADPHONES, 0.13);
    let mut adapter = adapter(audio);
    assert_eq!(hint(&adapter, SPEAKERS), None, "off: nothing is read");
    assert_eq!(row(&adapter, SPEAKERS).0, DeviceVolumeHint::None);

    adapter.set_speaker_enabled(&key(SPEAKERS), true);
    assert_eq!(hint(&adapter, SPEAKERS), Some(Hint::Low { percent: 13 }));
    assert_eq!(row(&adapter, SPEAKERS), (DeviceVolumeHint::Low, 13));
    assert_eq!(hint(&adapter, HEADPHONES), None, "only outputs that are on");
    assert_eq!(row(&adapter, HEADPHONES).0, DeviceVolumeHint::None);

    // It shows whether or not streaming runs, and AudioSync's own slider
    // stays where it was.
    adapter.set_running(false);
    assert!(!adapter.snapshot().running);
    assert_eq!(hint(&adapter, SPEAKERS), Some(Hint::Low { percent: 13 }));
    let speakers = adapter.snapshot().speaker(&key(SPEAKERS)).cloned().unwrap();
    assert_eq!(speakers.volume, DEFAULT_LOCAL_POSITION);

    // Raised to 25 % in Windows, the hint goes at the next poll.
    adapter.audio_mut().set_output_level(SPEAKERS, 0.25);
    assert!(adapter.poll(), "the row changed");
    assert_eq!(hint(&adapter, SPEAKERS), None);
    assert_eq!(row(&adapter, SPEAKERS).0, DeviceVolumeHint::None);
    // And switched off, the row shows nothing again.
    adapter.audio_mut().set_output_level(SPEAKERS, 0.13);
    adapter.poll();
    adapter.set_speaker_enabled(&key(SPEAKERS), false);
    assert_eq!(hint(&adapter, SPEAKERS), None);
}

// S196, S198
#[test]
fn a_muted_output_shows_the_unmute_fix() {
    let mut audio = audio();
    audio.mute_output(SPEAKERS, true);
    let mut adapter = adapter(audio);
    adapter.set_speaker_enabled(&key(SPEAKERS), true);
    assert_eq!(hint(&adapter, SPEAKERS), Some(Hint::Muted));
    assert_eq!(row(&adapter, SPEAKERS).0, DeviceVolumeHint::Muted);

    adapter.audio_mut().set_output_level(SPEAKERS, 0.13);
    assert!(adapter.poll());
    assert_eq!(
        hint(&adapter, SPEAKERS),
        Some(Hint::MutedAndLow { percent: 13 })
    );
    assert_eq!(row(&adapter, SPEAKERS), (DeviceVolumeHint::MutedLow, 13));

    // Unmuted in Windows, it is only low.
    adapter.audio_mut().mute_output(SPEAKERS, false);
    assert!(adapter.poll());
    assert_eq!(row(&adapter, SPEAKERS), (DeviceVolumeHint::Low, 13));
}

// S197
#[test]
fn the_fix_sets_75_percent_and_unmutes_once_each() {
    let mut audio = audio();
    audio.mute_output(SPEAKERS, true);
    audio.set_output_level(SPEAKERS, 0.13);
    let record = audio.volume_record();
    let mut adapter = adapter(audio);
    adapter.set_speaker_enabled(&key(SPEAKERS), true);
    // Windows' notification comes some time after the writes.
    adapter.audio_mut().hold_volume_notifications(true);
    adapter.fix_device_volume(&key(SPEAKERS));
    assert_eq!(record.writes(), [level(0.75), unmute()]);
    // The row follows that notification, not the values written.
    assert_eq!(row(&adapter, SPEAKERS), (DeviceVolumeHint::MutedLow, 13));
    assert!(!adapter.poll(), "nothing reported yet");
    adapter.audio_mut().hold_volume_notifications(false);
    assert!(adapter.poll(), "the notification came");
    assert_eq!(hint(&adapter, SPEAKERS), None);
    // Nothing is left to fix, so a second click writes nothing.
    adapter.fix_device_volume(&key(SPEAKERS));
    assert_eq!(record.writes().len(), 2);

    // Muted at 90 %: only unmuted, and left at 90 %.
    adapter.audio_mut().set_output_level(SPEAKERS, 0.9);
    adapter.audio_mut().mute_output(SPEAKERS, true);
    adapter.poll();
    assert_eq!(row(&adapter, SPEAKERS).0, DeviceVolumeHint::Muted);
    adapter.fix_device_volume(&key(SPEAKERS));
    assert_eq!(record.writes()[2..], [unmute()]);
    adapter.poll();
    let speakers = adapter
        .audio_mut()
        .outputs()
        .into_iter()
        .find(|output| output.id == SPEAKERS)
        .unwrap();
    assert_eq!(
        speakers.device_volume,
        Some(DeviceVolume {
            volume: Some(f64::from(0.9_f32)),
            muted: Some(false)
        })
    );
    assert_eq!(speakers.volume, DEFAULT_LOCAL_POSITION, "AudioSync's own");

    // A write the device ignores leaves the hint and the fix as they were,
    // and the next click tries again.
    adapter.audio_mut().set_output_level(SPEAKERS, 0.1);
    adapter.poll();
    adapter.audio_mut().ignore_volume_writes(true);
    adapter.fix_device_volume(&key(SPEAKERS));
    adapter.poll();
    assert_eq!(row(&adapter, SPEAKERS), (DeviceVolumeHint::Low, 10));
    adapter.fix_device_volume(&key(SPEAKERS));
    assert_eq!(record.writes()[3..], [level(0.75), level(0.75)]);

    // An output that is off and an AirPlay speaker have nothing to fix.
    adapter.fix_device_volume(&key(HEADPHONES));
    adapter.fix_device_volume("ap:02005E005301");
    adapter.fix_device_volume("nonsense");
    assert_eq!(record.writes().len(), 5);
}

// S192, S197
#[test]
fn nothing_is_written_without_a_click() {
    let dir = test_dir("adapter-device-volume-writes");
    let path = dir.join(settings::FILE_NAME);
    let launch = |audio: MockBackend| {
        let file = SettingsFile::at(path.clone());
        AudioAdapter::with_settings(audio, || "09.41".to_owned(), DebugLog::default(), file)
    };
    // A first run switches both outputs on, then quits.
    let mut adapter = launch(audio());
    adapter.set_speaker_enabled(&key(SPEAKERS), true);
    adapter.set_speaker_enabled(&key(HEADPHONES), true);
    adapter.save_settings();
    drop(adapter);

    // The next launch finds the speakers muted at 13 % and the headphones
    // at 5 %: shown, never written.
    let mut audio = audio();
    audio.mute_output(SPEAKERS, true);
    audio.set_output_level(SPEAKERS, 0.13);
    audio.set_output_level(HEADPHONES, 0.05);
    let record = audio.volume_record();
    let mut adapter = launch(audio);
    assert_eq!(
        hint(&adapter, SPEAKERS),
        Some(Hint::MutedAndLow { percent: 13 })
    );
    assert_eq!(hint(&adapter, HEADPHONES), Some(Hint::Low { percent: 5 }));
    // A start, a source change while streaming, a stop and a start again.
    adapter.set_running(true);
    assert!(adapter.snapshot().running);
    adapter.select_source(&format!("app:{PLAYER_PID}:player.exe"));
    adapter.set_running(false);
    adapter.set_running(true);
    // Unplugged and plugged in again, switched off and on again.
    adapter.audio_mut().disconnect_output(HEADPHONES);
    adapter.poll();
    adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
    adapter.poll();
    adapter.set_speaker_enabled(&key(SPEAKERS), false);
    adapter.set_speaker_enabled(&key(SPEAKERS), true);
    adapter.poll();
    assert_eq!(hint(&adapter, HEADPHONES), Some(Hint::Low { percent: 5 }));
    // AudioSync quits.
    adapter.save_settings();
    drop(adapter);
    assert_eq!(record.writes(), []);
    assert!(
        record.watched().is_empty(),
        "quitting removes every listener"
    );
    let _ = std::fs::remove_dir_all(dir);
}
