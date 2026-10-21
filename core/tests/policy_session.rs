//! Sessions, output sync, streaming, source switches and the host's reactions
//! (S20, S21, S68, S69, S116 to S122, S136 to S138, S141, S142, S144 to S146,
//! S161 to S163, S165).

use audiosync_core::policy::session::{
    AFTER_START, AfterStart, AutoStartGate, Click, DropReason, Incident, OutputSync, Reaction,
    SourcePick, SpeakerEvent, SpeakerKind, SpeakerState, StartOrigin, StreamingAction, TurnOff,
    TurnOn, click, may_start, reaction, source_pick, source_switch, speaker_state, streaming_after,
    sync_outputs, turn_off, turn_on,
};
use audiosync_core::policy::source::AudioSource;
use audiosync_core::policy::speakers::{NoticeSubject, SpeakerKey};
use proptest::prelude::*;

const NONE: [&str; 0] = [];

// covers: S20, S140
#[test]
fn a_capture_format_change_restarts_the_session_automatically() {
    let reaction = reaction(&Incident::FormatChanged);
    assert!(reaction.stop_session);
    assert_eq!(reaction.restart, Some(StartOrigin::FormatChange));
    assert!(StartOrigin::FormatChange.is_automatic());
    assert!(!reaction.deselect_airplay);
    assert_eq!(reaction.notice, None);
}

// covers: S21
#[test]
fn a_lost_source_stops_deselects_airplay_and_posts_a_source_notice() {
    assert_eq!(
        reaction(&Incident::SourceLost),
        Reaction {
            stop_session: true,
            deselect_airplay: true,
            notice: Some(NoticeSubject::Source),
            ..Reaction::default()
        }
    );
}

// covers: S142
#[test]
fn the_source_app_quitting_stops_keeps_the_speakers_chosen_and_notices_without_an_alert() {
    let reaction = reaction(&Incident::SourceAppQuit);
    assert!(reaction.stop_session && !reaction.deselect_airplay);
    assert_eq!(reaction.notice, Some(NoticeSubject::Source));
    assert!(!reaction.alert);
}

// covers: S68, S137, S138
#[test]
fn a_lost_device_stays_enabled_and_is_added_again_once_listed() {
    let reaction = reaction(&Incident::OutputLost {
        uid: "usb".to_owned(),
    });
    assert!(!reaction.disable_speaker && !reaction.stop_session);
    // Unplugged (or a Bluetooth drop): its renderer is gone, the UID stays enabled.
    let enabled = ["usb", "hdmi"];
    assert_eq!(
        sync_outputs(true, &enabled, &["hdmi"], &["hdmi"]),
        OutputSync::default()
    );
    // Plugged back in: the next poll adds it again.
    let sync = sync_outputs(true, &enabled, &["hdmi", "usb"], &["hdmi"]);
    assert_eq!(sync.add, ["usb"]);
    assert!(sync.remove.is_empty());
}

// covers: S69
#[test]
fn a_failed_output_is_disabled_with_a_notice_and_an_alert_only_when_the_user_caused_it() {
    let failed = reaction(&Incident::OutputStartFailed {
        uid: "usb".to_owned(),
        by_user: true,
        enabled_left: 2,
    });
    assert!(failed.disable_speaker && failed.alert);
    assert_eq!(
        failed.notice,
        Some(NoticeSubject::Speaker(SpeakerKey::Local("usb".to_owned())))
    );
    assert!(!failed.stop_session);
    let automatic = reaction(&Incident::OutputStartFailed {
        uid: "usb".to_owned(),
        by_user: false,
        enabled_left: 0,
    });
    assert!(!automatic.alert);
    assert!(
        automatic.stop_session,
        "nothing else is enabled: the session stops"
    );
}

// covers: S116
#[test]
fn a_session_starts_only_with_a_speaker_enabled_then_runs_its_steps_in_order() {
    assert!(!may_start(0));
    assert!(may_start(1));
    let refused = reaction(&Incident::NoSpeakerEnabled);
    assert_eq!(refused.notice, Some(NoticeSubject::Start));
    assert!(refused.informational && !refused.stop_session);
    assert_eq!(
        AFTER_START,
        [
            AfterStart::ApplyAirPlaySelection,
            AfterStart::SendMasterVolume,
            AfterStart::AddLocalOutputs,
            AfterStart::StartCaptureCheck,
        ]
    );
}

// covers: S117
#[test]
fn output_sync_adds_enabled_present_outputs_and_removes_disabled_ones() {
    let sync = sync_outputs(true, &["a", "b", "c"], &["a", "b", "x"], &["b", "z"]);
    assert_eq!(sync.add, ["a"], "c is enabled but not present: it waits");
    assert_eq!(sync.remove, ["z"]);
    // Idempotent: once applied, nothing more to do.
    assert!(sync_outputs(true, &["a", "b", "c"], &["a", "b", "x"], &["b", "a"]).is_empty());
    // Outputs are added only while running.
    assert!(sync_outputs(false, &["a"], &["a"], &NONE).is_empty());
}

// covers: S118, S144
#[test]
fn switching_the_last_speaker_off_stops_the_session_and_deselects_airplay() {
    assert_eq!(turn_off(SpeakerKind::Local, true, 0), TurnOff::StopSession);
    assert_eq!(
        turn_off(SpeakerKind::AirPlay, true, 0),
        TurnOff::StopSession
    );
    assert_eq!(
        turn_off(SpeakerKind::Local, true, 1),
        TurnOff::SyncLocalOutputs
    );
    assert_eq!(
        turn_off(SpeakerKind::AirPlay, true, 2),
        TurnOff::DeselectAirPlay
    );
    assert_eq!(turn_off(SpeakerKind::Local, false, 0), TurnOff::Nothing);
    // All off, then one on: a new session (with the saved delays and volumes,
    // which switching off never touches).
    assert_eq!(turn_on(SpeakerKind::Local, false), TurnOn::StartSession);
}

// covers: S119
#[test]
fn turning_a_speaker_on_starts_streaming_or_joins_the_running_session() {
    assert_eq!(turn_on(SpeakerKind::AirPlay, false), TurnOn::StartSession);
    assert_eq!(turn_on(SpeakerKind::Local, true), TurnOn::SyncLocalOutputs);
    assert_eq!(turn_on(SpeakerKind::AirPlay, true), TurnOn::SelectAirPlay);
}

// covers: S120
#[test]
fn a_failed_automatic_start_holds_automatic_starts_until_the_user_acts() {
    let mut gate = AutoStartGate::default();
    assert!(gate.allows(StartOrigin::AutoConnect) && gate.allows(StartOrigin::FormatChange));
    gate.start_failed(StartOrigin::User);
    assert!(
        gate.allows(StartOrigin::AutoConnect),
        "a failed user start holds nothing"
    );
    gate.start_failed(StartOrigin::AutoConnect);
    assert!(!gate.allows(StartOrigin::AutoConnect));
    assert!(!gate.allows(StartOrigin::FormatChange));
    assert!(gate.allows(StartOrigin::User));
    gate.user_acted();
    assert!(gate.allows(StartOrigin::AutoConnect));
}

// covers: S120
#[test]
fn switching_streaming_off_by_hand_holds_auto_connect() {
    let mut gate = AutoStartGate::default();
    gate.user_switched_off();
    assert!(!gate.allows(StartOrigin::AutoConnect));
    assert!(gate.allows(StartOrigin::FormatChange));
    assert!(gate.allows(StartOrigin::User));
    gate.user_acted();
    assert!(gate.allows(StartOrigin::AutoConnect));
}

// covers: S121
#[test]
fn a_failed_start_stops_everything_deselects_airplay_and_alerts_only_a_user() {
    for incident in [
        Incident::StartFailed {
            origin: StartOrigin::User,
        },
        Incident::MicrophoneDenied {
            origin: StartOrigin::User,
        },
    ] {
        let reaction = reaction(&incident);
        assert!(reaction.stop_session && reaction.deselect_airplay && reaction.alert);
        assert_eq!(reaction.notice, Some(NoticeSubject::Start));
    }
    let automatic = reaction(&Incident::StartFailed {
        origin: StartOrigin::AutoConnect,
    });
    assert!(automatic.stop_session && automatic.deselect_airplay && !automatic.alert);
}

// covers: S122
#[test]
fn speaker_states_and_clicks() {
    assert_eq!(
        speaker_state(true, true),
        SpeakerState::Playing,
        "on while streaming plays"
    );
    assert_eq!(
        speaker_state(true, false),
        SpeakerState::WillPlay,
        "on while off waits"
    );
    assert_eq!(speaker_state(false, true), SpeakerState::Off);
    assert_eq!(speaker_state(false, false), SpeakerState::Off);
    assert_eq!(click(SpeakerState::Playing), Click::TurnOff);
    assert_eq!(click(SpeakerState::Off), Click::TurnOn);
}

// covers: S119, S122
#[test]
fn a_click_on_a_waiting_speaker_turns_it_on_and_starts_streaming() {
    let state = speaker_state(true, false);
    assert_eq!(click(state), Click::TurnOn);
    assert_eq!(turn_on(SpeakerKind::Local, false), TurnOn::StartSession);
}

// covers: S136
#[test]
fn every_error_state_has_its_reaction() {
    let user = StartOrigin::User;
    let table: Vec<(Incident, bool, bool, Option<NoticeSubject>)> = vec![
        (
            Incident::NoSpeakerEnabled,
            false,
            false,
            Some(NoticeSubject::Start),
        ),
        (
            Incident::MicrophoneDenied { origin: user },
            true,
            true,
            Some(NoticeSubject::Start),
        ),
        (
            Incident::StartFailed { origin: user },
            true,
            true,
            Some(NoticeSubject::Start),
        ),
        (
            Incident::NoAudio,
            false,
            false,
            Some(NoticeSubject::Capture),
        ),
        (Incident::FormatChanged, true, false, None),
        (
            Incident::SourceLost,
            true,
            true,
            Some(NoticeSubject::Source),
        ),
        (
            Incident::SourceAppQuit,
            true,
            false,
            Some(NoticeSubject::Source),
        ),
        (
            Incident::OutputLost {
                uid: "u".to_owned(),
            },
            false,
            false,
            None,
        ),
    ];
    for (incident, stops, deselects, notice) in table {
        let reaction = reaction(&incident);
        assert_eq!(reaction.stop_session, stops, "{incident:?}");
        assert_eq!(reaction.deselect_airplay, deselects, "{incident:?}");
        assert_eq!(reaction.notice, notice, "{incident:?}");
    }
    let dropped = reaction(&Incident::SpeakerDropped {
        id: "7".to_owned(),
        reason: DropReason::CodeRejected,
        enabled_left: 1,
    });
    assert!(dropped.disable_speaker && dropped.alert && !dropped.stop_session);
    assert_eq!(
        dropped.notice,
        Some(NoticeSubject::Speaker(SpeakerKey::AirPlay("7".to_owned())))
    );
    let unreachable = reaction(&Incident::SpeakerDropped {
        id: "7".to_owned(),
        reason: DropReason::Unreachable,
        enabled_left: 0,
    });
    assert!(!unreachable.alert && unreachable.stop_session);
    for reason in [DropReason::Unreachable, DropReason::NeedsCode] {
        assert!(!reason.alerts(), "only a rejected code alerts: {reason:?}");
    }
    assert!(DropReason::CodeRejected.alerts());
}

// covers: S141
#[test]
fn a_source_switch_stops_without_deselecting_then_starts_as_the_user() {
    let switch = source_switch(true);
    assert!(switch.stop_keeping_airplay);
    assert_eq!(switch.start, Some(StartOrigin::User));
    let idle = source_switch(false);
    assert!(!idle.stop_keeping_airplay);
    assert_eq!(idle.start, None);
}

// covers: S145
#[test]
fn renderers_follow_device_uids_not_the_default_output() {
    // The same enabled set and devices give the same sync whichever device is
    // the system default: the sync never looks at it.
    let enabled = ["usb", "hdmi"];
    let present = ["builtin", "usb", "hdmi"];
    let before = sync_outputs(true, &enabled, &present, &["usb", "hdmi"]);
    assert!(before.is_empty());
}

// covers: S161, S117
#[test]
fn output_sync_attaches_and_detaches_in_uid_order() {
    let sync = sync_outputs(
        true,
        &["usb-b", "hdmi", "usb-a", "bt"],
        &["bt", "usb-a", "hdmi", "usb-b", "spare"],
        &["zz", "hdmi", "aa"],
    );
    assert_eq!(
        sync.add,
        ["bt", "usb-a", "usb-b"],
        "enabled ∩ available − active"
    );
    assert_eq!(sync.remove, ["aa", "zz"], "active − enabled");
    // A UID the host lists twice counts once.
    let sync = sync_outputs(true, &["b", "a", "b"], &["a", "b", "a"], &NONE);
    assert_eq!(sync.add, ["a", "b"]);
}

// covers: S161, S68, S117, S137
#[test]
fn an_unplugged_speaker_stays_on_and_is_attached_again_once_listed() {
    let enabled = ["usb", "builtin"];
    // A playing speaker that is still present is left alone.
    assert!(sync_outputs(true, &enabled, &["builtin", "usb"], &["builtin", "usb"]).is_empty());
    // Unplugged: its renderer is gone (S68), its UID stays enabled and waits.
    assert!(sync_outputs(true, &enabled, &["builtin"], &["builtin"]).is_empty());
    // A renderer whose UID is still enabled is never detached, listed or not.
    assert!(
        sync_outputs(true, &enabled, &["builtin"], &["builtin", "usb"])
            .remove
            .is_empty()
    );
    // Plugged back in: the first sync that finds it listed attaches it.
    let replug = sync_outputs(true, &enabled, &["builtin", "usb"], &["builtin"]);
    assert_eq!(replug.add, ["usb"]);
    assert!(replug.remove.is_empty());
    // Switched off by the user: detached.
    let off = sync_outputs(true, &["builtin"], &["builtin", "usb"], &["builtin", "usb"]);
    assert_eq!(off.remove, ["usb"]);
    assert!(off.add.is_empty());
}

// covers: S161, S138
#[test]
fn a_headset_out_of_range_waits_while_the_other_speaker_plays() {
    let enabled = ["headset", "desk"];
    // Disconnected: nothing to attach or detach; the desk keeps playing.
    assert!(sync_outputs(true, &enabled, &["desk"], &["desk"]).is_empty());
    // Reconnected: attached again.
    let back = sync_outputs(true, &enabled, &["desk", "headset"], &["desk"]);
    assert_eq!(back.add, ["headset"]);
    assert!(back.remove.is_empty());
}

// covers: S165, S161, S146
#[test]
fn eight_local_speakers_are_attached_by_one_sync() {
    let enabled: Vec<String> = (1..=8).rev().map(|i| format!("usb-{i}")).collect();
    let mut present = enabled.clone();
    present.push("BuiltInSpeakerDevice".to_owned());
    let sync = sync_outputs(true, &enabled, &present, &["BuiltInSpeakerDevice"]);
    let all: Vec<String> = (1..=8).map(|i| format!("usb-{i}")).collect();
    assert_eq!(sync.add, all, "all eight, in UID order");
    assert_eq!(sync.remove, ["BuiltInSpeakerDevice"]);
}

// covers: S162, S118, S119, S144
#[test]
fn streaming_follows_the_enabled_sets_alone() {
    use SpeakerEvent::{StartRequested, SwitchedOff, SwitchedOn};
    use StreamingAction::{AskForSpeaker, Nothing, Start, Stop};
    // Switching the last speaker off stops streaming.
    assert_eq!(streaming_after(SwitchedOff, true, 0, 0), Stop);
    // One speaker still on, an AirPlay one included, keeps it running.
    assert_eq!(streaming_after(SwitchedOff, true, 0, 1), Nothing);
    assert_eq!(streaming_after(SwitchedOff, true, 1, 0), Nothing);
    // While stopped there is nothing to stop, and a switch-off starts nothing.
    assert_eq!(streaming_after(SwitchedOff, false, 0, 0), Nothing);
    assert_eq!(streaming_after(SwitchedOff, false, 2, 1), Nothing);
    // Turning a speaker on again starts streaming ...
    assert_eq!(streaming_after(SwitchedOn, false, 1, 0), Start);
    assert_eq!(streaming_after(SwitchedOn, false, 0, 1), Start);
    // ... but only while nothing streams.
    assert_eq!(streaming_after(SwitchedOn, true, 2, 0), Nothing);
    // Start with no speaker on asks for one first, with an informational notice.
    assert_eq!(streaming_after(StartRequested, false, 0, 0), AskForSpeaker);
    let notice = reaction(&Incident::NoSpeakerEnabled);
    assert!(notice.informational && !notice.stop_session);
    assert_eq!(streaming_after(StartRequested, false, 0, 1), Start);
    assert_eq!(streaming_after(StartRequested, true, 1, 0), Nothing);
}

// covers: S162, S137
#[test]
fn streaming_keeps_running_while_the_only_speaker_is_unplugged() {
    // The only speaker is still enabled; its device just isn't listed now.
    assert_eq!(
        streaming_after(SpeakerEvent::ListingChanged, true, 1, 0),
        StreamingAction::Nothing
    );
    assert_eq!(
        streaming_after(SpeakerEvent::ListingChanged, false, 1, 0),
        StreamingAction::Nothing,
        "a listing change is not a switch"
    );
}

// covers: S163, S141
#[test]
fn picking_a_source_switches_now_stores_it_or_does_nothing() {
    let system = AudioSource::System;
    let spotify = AudioSource::App {
        bundle_id: "com.spotify.client".to_owned(),
    };
    let mic = AudioSource::Input {
        uid: "BuiltInMicrophoneDevice".to_owned(),
    };
    // Switching while streaming rebuilds the capture; the speakers stay on
    // and the AirPlay speakers stay selected.
    let switch = source_pick(&system, &spotify, true);
    let SourcePick::SwitchNow(how) = switch else {
        panic!("expected a switch now, got {switch:?}");
    };
    assert_eq!(how, source_switch(true));
    assert!(how.stop_keeping_airplay);
    assert_eq!(how.start, Some(StartOrigin::User));
    // So does switching to an input device.
    assert_eq!(
        source_pick(&spotify, &mic, true),
        SourcePick::SwitchNow(source_switch(true))
    );
    // Switching while stopped only stores the pick for the next start.
    assert_eq!(
        source_pick(&system, &mic, false),
        SourcePick::StoreForNextStart
    );
    let music = AudioSource::App {
        bundle_id: "com.apple.Music".to_owned(),
    };
    assert_eq!(
        source_pick(&spotify, &music, false),
        SourcePick::StoreForNextStart
    );
    // Picking the source already in use changes nothing.
    for streaming in [true, false] {
        assert_eq!(
            source_pick(&spotify, &spotify.clone(), streaming),
            SourcePick::Nothing
        );
        assert_eq!(
            source_pick(&system, &AudioSource::System, streaming),
            SourcePick::Nothing
        );
    }
    // Any real change clears the old source's notices; no change clears none.
    let cleared = [NoticeSubject::Source, NoticeSubject::Capture];
    assert_eq!(switch.clears(), cleared);
    assert_eq!(SourcePick::StoreForNextStart.clears(), cleared);
    assert!(SourcePick::Nothing.clears().is_empty());
}

// covers: S146
#[test]
fn sixteen_and_more_local_outputs_sync_without_a_limit() {
    let uids: Vec<String> = (0..40).map(|i| format!("device-{i}")).collect();
    let sync = sync_outputs(true, &uids, &uids, &NONE);
    assert_eq!(sync.add.len(), 40);
    assert!(sync_outputs(true, &uids, &uids, &uids).is_empty());
}

proptest! {
    // covers: S117, S161
    #[test]
    fn output_sync_is_idempotent(
        enabled in proptest::collection::vec(0u8..12, 0..12),
        present in proptest::collection::vec(0u8..12, 0..12),
        active in proptest::collection::vec(0u8..12, 0..12),
    ) {
        let names = |ids: &[u8]| ids.iter().map(|id| format!("uid-{id}")).collect::<Vec<_>>();
        let (enabled, present, active) = (names(&enabled), names(&present), names(&active));
        let sync = sync_outputs(true, &enabled, &present, &active);
        // Sorted by UID, each once.
        prop_assert!(sync.add.windows(2).all(|pair| pair[0] < pair[1]));
        prop_assert!(sync.remove.windows(2).all(|pair| pair[0] < pair[1]));
        let mut after: Vec<String> = active.iter().filter(|uid| !sync.remove.contains(uid)).cloned().collect();
        after.extend(sync.add.iter().cloned());
        prop_assert!(sync_outputs(true, &enabled, &present, &after).is_empty());
        for uid in &after {
            prop_assert!(enabled.contains(uid));
        }
    }
}
