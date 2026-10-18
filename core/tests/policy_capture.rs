//! Capture sources, the microphone permission, the capture check, capture
//! rate changes and app quits (S12, S15, S17 to S19, S157, S164).

use audiosync_core::policy::capture::{
    CHECK_EVERY, CaptureCheck, CaptureVerdict, FIRST_CHECK_AFTER, PLAYING_GRACE, PlayerReading,
    RateDecision, capture_rate_decision, capture_verdict, playing_for,
};
use audiosync_core::policy::session::{Incident, StartOrigin, reaction};
use audiosync_core::policy::source::{
    AudioSource, MicrophonePermission, MicrophoneStep, after_microphone_request, microphone_step,
};

fn app(bundle_id: &str) -> AudioSource {
    AudioSource::App {
        bundle_id: bundle_id.to_owned(),
    }
}

fn reading(bundle_id: &str, playing_since: Option<f64>) -> PlayerReading {
    PlayerReading {
        bundle_id: bundle_id.to_owned(),
        playing_since,
    }
}

// covers: S12
#[test]
fn sources_round_trip_through_their_stored_form() {
    let input = AudioSource::Input {
        uid: "BuiltInMicrophoneDevice".to_owned(),
    };
    for source in [
        AudioSource::System,
        app("com.spotify.client"),
        input.clone(),
    ] {
        assert_eq!(AudioSource::from_stored(Some(&source.stored())), source);
    }
    assert_eq!(AudioSource::System.stored(), "");
    assert_eq!(input.stored(), "input:BuiltInMicrophoneDevice");
    assert_eq!(app("com.apple.Music").stored(), "com.apple.Music");
}

// covers: S12
#[test]
fn a_missing_value_is_the_system_and_a_legacy_bundle_id_an_app() {
    assert_eq!(AudioSource::from_stored(None), AudioSource::System);
    assert_eq!(
        AudioSource::from_stored(Some("com.spotify.client")),
        app("com.spotify.client")
    );
    // UIDs keep everything after the prefix, colons included.
    assert_eq!(
        AudioSource::from_stored(Some("input:AppleUSB:20:1")),
        AudioSource::Input {
            uid: "AppleUSB:20:1".to_owned()
        }
    );
}

// covers: S12
#[test]
fn only_an_app_source_has_a_bundle_id() {
    assert_eq!(
        app("com.spotify.client").bundle_id(),
        Some("com.spotify.client")
    );
    assert_eq!(AudioSource::System.bundle_id(), None);
    let input = AudioSource::Input {
        uid: "mic".to_owned(),
    };
    assert_eq!(input.bundle_id(), None);
}

// covers: S15
#[test]
fn an_input_source_needs_the_microphone_permission() {
    let input = AudioSource::Input {
        uid: "mic".to_owned(),
    };
    assert_eq!(
        microphone_step(&input, MicrophonePermission::Authorized),
        MicrophoneStep::Start
    );
    assert_eq!(
        microphone_step(&input, MicrophonePermission::NotDetermined),
        MicrophoneStep::Request
    );
    assert_eq!(
        microphone_step(&input, MicrophonePermission::Denied),
        MicrophoneStep::Fail
    );
    assert_eq!(
        microphone_step(&input, MicrophonePermission::Restricted),
        MicrophoneStep::Fail
    );
    assert_eq!(after_microphone_request(true), MicrophoneStep::Start);
    assert_eq!(after_microphone_request(false), MicrophoneStep::Fail);
    for permission in [
        MicrophonePermission::Denied,
        MicrophonePermission::NotDetermined,
    ] {
        assert_eq!(
            microphone_step(&AudioSource::System, permission),
            MicrophoneStep::Start
        );
        assert_eq!(
            microphone_step(&app("com.apple.Music"), permission),
            MicrophoneStep::Start
        );
    }
}

// covers: S18
#[test]
fn capture_frames_mean_flowing() {
    assert_eq!(capture_verdict(1, None), CaptureVerdict::Flowing);
    assert_eq!(capture_verdict(48_000, Some(30.0)), CaptureVerdict::Flowing);
}

// covers: S17, S18
#[test]
fn capture_no_frames_from_an_unknown_source_is_waiting() {
    assert_eq!(capture_verdict(0, None), CaptureVerdict::Waiting);
    // Every session starts again from zero frames: a new check waits again.
    let mut check = CaptureCheck::started(100.0);
    assert_eq!(check.check(103.0, 0, None), Some(CaptureVerdict::Waiting));
}

// covers: S18
#[test]
fn capture_playing_only_briefly_is_still_waiting_and_the_grace_period_means_not_working() {
    assert_eq!(PLAYING_GRACE, 6.0);
    assert_eq!(capture_verdict(0, Some(5.9)), CaptureVerdict::Waiting);
    assert_eq!(capture_verdict(0, Some(6.0)), CaptureVerdict::NotWorking);
}

// covers: S18, S19
#[test]
fn a_paused_or_unknown_source_never_blames_the_permission() {
    let paused = [reading("com.spotify.client", None)];
    assert_eq!(playing_for(&AudioSource::System, &paused, 100.0), None);
    assert_eq!(
        capture_verdict(0, playing_for(&AudioSource::System, &paused, 100.0)),
        CaptureVerdict::Waiting
    );
    assert_eq!(
        capture_verdict(0, playing_for(&AudioSource::System, &[], 100.0)),
        CaptureVerdict::Waiting
    );
}

// covers: S19
#[test]
fn known_playing_counts_any_player_for_system_only_that_app_for_an_app_never_for_an_input() {
    let readings = [
        reading("com.spotify.client", Some(80.0)),
        reading("com.apple.Music", Some(95.0)),
    ];
    assert_eq!(
        playing_for(&AudioSource::System, &readings, 100.0),
        Some(20.0)
    );
    assert_eq!(
        playing_for(&app("com.apple.Music"), &readings, 100.0),
        Some(5.0)
    );
    assert_eq!(
        playing_for(&app("com.example.Other"), &readings, 100.0),
        None
    );
    let input = AudioSource::Input {
        uid: "mic".to_owned(),
    };
    assert_eq!(playing_for(&input, &readings, 100.0), None);
}

// covers: S18
#[test]
fn the_check_runs_3_s_after_a_start_then_every_2_s_and_reports_only_changes() {
    assert_eq!((FIRST_CHECK_AFTER, CHECK_EVERY), (3.0, 2.0));
    let mut check = CaptureCheck::started(10.0);
    assert_eq!(check.next_check_at(), Some(13.0));
    assert_eq!(check.check(12.0, 0, None), None, "not due yet");
    assert_eq!(
        check.check(13.0, 0, Some(1.0)),
        Some(CaptureVerdict::Waiting)
    );
    assert_eq!(check.next_check_at(), Some(15.0));
    assert_eq!(
        check.check(15.0, 0, Some(3.0)),
        None,
        "same verdict: no new notice"
    );
    assert_eq!(
        check.check(17.0, 0, Some(6.5)),
        Some(CaptureVerdict::NotWorking)
    );
    assert_eq!(check.check(19.0, 0, Some(8.5)), None);
    assert_eq!(
        check.check(21.0, 4800, Some(10.5)),
        Some(CaptureVerdict::Flowing)
    );
    assert_eq!(check.next_check_at(), None, "audio flows: no more checks");
    assert_eq!(check.check(40.0, 0, None), None);

    let mut ended = CaptureCheck::started(0.0);
    ended.session_ended();
    assert_eq!(ended.check(3.0, 0, None), None);
}

// covers: S157, S20, S140
#[test]
fn a_new_capture_rate_rebuilds_the_session_in_either_direction() {
    // A lower rate rebuilds the session; so does a higher one.
    assert_eq!(
        capture_rate_decision(48_000.0, 44_100.0),
        RateDecision::Rebuild
    );
    assert_eq!(
        capture_rate_decision(44_100.0, 96_000.0),
        RateDecision::Rebuild
    );
    // The rebuild is the format-change restart: stop, then an automatic start.
    let restart = reaction(&Incident::FormatChanged);
    assert!(restart.stop_session);
    assert_eq!(restart.restart, Some(StartOrigin::FormatChange));
}

// covers: S157
#[test]
fn the_same_rate_or_an_unreadable_one_keeps_playing() {
    assert_eq!(
        capture_rate_decision(48_000.0, 48_000.0),
        RateDecision::KeepPlaying
    );
    // A rate that couldn't be read is reported as 0.
    assert_eq!(
        capture_rate_decision(48_000.0, 0.0),
        RateDecision::KeepPlaying
    );
    for odd in [-44_100.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            capture_rate_decision(48_000.0, odd),
            RateDecision::KeepPlaying,
            "{odd}"
        );
    }
}

// covers: S164, S142
#[test]
fn only_the_source_apps_own_quit_ends_the_session() {
    let spotify = app("com.spotify.client");
    assert!(spotify.depends_on_app(Some("com.spotify.client")));
    let ended = reaction(&Incident::SourceAppQuit);
    assert!(ended.stop_session && ended.deselect_airplay && !ended.alert);
    // Another app quitting changes nothing.
    assert!(!spotify.depends_on_app(Some("com.apple.Music")));
    assert!(!spotify.depends_on_app(Some("com.spotify.client.helper")));
    // An app without a bundle ID changes nothing.
    assert!(!spotify.depends_on_app(None));
    assert!(!app("").depends_on_app(Some("")));
    // The whole-system source and an input source never end on a quit.
    let input = AudioSource::Input {
        uid: "BuiltInMicrophoneDevice".to_owned(),
    };
    for quitting in [Some("com.spotify.client"), Some(""), None] {
        assert!(!AudioSource::System.depends_on_app(quitting));
        assert!(!input.depends_on_app(quitting));
    }
}
