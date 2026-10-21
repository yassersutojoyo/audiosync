//! What a running session does when the app it captures quits (S226).

use audiosync_core::policy::session::{Incident, reaction};
use audiosync_core::policy::source::AudioSource;
use audiosync_core::policy::source_quit::{
    SourceQuitAction, SourceQuitWatch, Streaming, WhenSourceQuits,
};

const PLAYER: &str = "com.example.player";

fn app(bundle_id: &str) -> AudioSource {
    AudioSource::App {
        bundle_id: bundle_id.to_owned(),
    }
}

fn input() -> AudioSource {
    AudioSource::Input {
        uid: "BuiltInMicrophoneDevice".to_owned(),
    }
}

const SETTINGS: [WhenSourceQuits; 2] = [WhenSourceQuits::Stop, WhenSourceQuits::SwitchToSystem];

// covers: S226
#[test]
fn the_setting_reads_stop_unless_it_says_system() {
    assert_eq!(WhenSourceQuits::default(), WhenSourceQuits::Stop);
    assert_eq!(SourceQuitWatch::default().setting(), WhenSourceQuits::Stop);
    for value in [None, Some(""), Some("stop"), Some("System"), Some("switch")] {
        assert_eq!(
            WhenSourceQuits::from_stored(value),
            WhenSourceQuits::Stop,
            "{value:?}"
        );
    }
    for setting in SETTINGS {
        assert_eq!(
            WhenSourceQuits::from_stored(Some(setting.stored())),
            setting
        );
    }
}

// covers: S142, S164, S226
#[test]
fn stop_does_what_a_source_app_quit_did_before_the_setting() {
    let source = app(PLAYER);
    for streaming in [Streaming::Off, Streaming::Running] {
        for quitting in [Some(PLAYER), Some("com.example.other"), Some(""), None] {
            let mut watch = SourceQuitWatch::default();
            let ended = streaming == Streaming::Running && source.depends_on_app(quitting);
            let expected = if ended {
                SourceQuitAction::Stop
            } else {
                SourceQuitAction::Keep
            };
            assert_eq!(
                watch.app_exited(&source, quitting, streaming),
                expected,
                "{streaming:?} {quitting:?}"
            );
            assert_eq!(watch.switched_from(), None);
        }
    }
    let before = reaction(&Incident::SourceAppQuit);
    assert!(before.stop_session && before.deselect_airplay && before.restart.is_none());
}

// covers: S226
#[test]
fn switch_moves_a_running_session_to_whole_system_audio() {
    let mut watch = SourceQuitWatch::new(WhenSourceQuits::SwitchToSystem);
    assert_eq!(
        watch.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Running),
        SourceQuitAction::SwitchToSystem
    );
    assert_eq!(watch.switched_from(), Some(PLAYER));
}

// covers: S142, S226
#[test]
fn a_quit_while_no_session_runs_changes_nothing() {
    for setting in SETTINGS {
        let mut watch = SourceQuitWatch::new(setting);
        assert_eq!(
            watch.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Off),
            SourceQuitAction::Keep,
            "{setting:?}"
        );
        assert_eq!(watch.switched_from(), None);
    }
}

// covers: S164, S226
#[test]
fn a_whole_system_or_input_source_never_stops_or_switches_on_a_quit() {
    for setting in SETTINGS {
        for source in [AudioSource::System, input()] {
            for streaming in [Streaming::Running, Streaming::Rebuilding] {
                let mut watch = SourceQuitWatch::new(setting);
                assert_eq!(
                    watch.app_exited(&source, Some(PLAYER), streaming),
                    SourceQuitAction::Keep,
                    "{setting:?} {source:?} {streaming:?}"
                );
                assert_eq!(watch.switched_from(), None);
            }
        }
    }
}

// covers: S164, S226
#[test]
fn another_app_or_one_without_an_id_quitting_changes_nothing() {
    for setting in SETTINGS {
        for quitting in [Some("com.example.other"), Some(""), None] {
            let mut watch = SourceQuitWatch::new(setting);
            assert_eq!(
                watch.app_exited(&app(PLAYER), quitting, Streaming::Running),
                SourceQuitAction::Keep,
                "{setting:?} {quitting:?}"
            );
        }
    }
}

// covers: S226
#[test]
fn the_app_starting_again_after_a_switch_keeps_whole_system_audio() {
    let mut watch = SourceQuitWatch::new(WhenSourceQuits::SwitchToSystem);
    watch.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Running);
    assert_eq!(watch.app_relaunched(Some(PLAYER)), SourceQuitAction::Keep);
    assert_eq!(watch.switched_from(), Some(PLAYER));
    // Quitting once more concerns no session: the source is the whole system.
    assert_eq!(
        watch.app_exited(&AudioSource::System, Some(PLAYER), Streaming::Running),
        SourceQuitAction::Keep
    );
}

// covers: S226
#[test]
fn a_setting_changed_mid_play_decides_only_the_next_quit() {
    let mut watch = SourceQuitWatch::default();
    assert_eq!(
        watch.set_setting(WhenSourceQuits::SwitchToSystem),
        SourceQuitAction::Keep
    );
    assert_eq!(
        watch.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Running),
        SourceQuitAction::SwitchToSystem
    );
    // Going back to Stop after the switch doesn't undo it.
    assert_eq!(
        watch.set_setting(WhenSourceQuits::Stop),
        SourceQuitAction::Keep
    );
    assert_eq!(watch.switched_from(), Some(PLAYER));
    watch.source_picked();
    assert_eq!(
        watch.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Running),
        SourceQuitAction::Stop
    );
}

// covers: S226
#[test]
fn a_quit_during_a_rebuild_stops_it_or_rebuilds_with_whole_system_audio() {
    let mut stop = SourceQuitWatch::new(WhenSourceQuits::Stop);
    assert_eq!(
        stop.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Rebuilding),
        SourceQuitAction::Stop
    );
    let mut switch = SourceQuitWatch::new(WhenSourceQuits::SwitchToSystem);
    assert_eq!(
        switch.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Rebuilding),
        SourceQuitAction::SwitchToSystem
    );
    assert_eq!(switch.switched_from(), Some(PLAYER));
}

// covers: S226
#[test]
fn the_same_quit_reported_twice_acts_once() {
    let mut stop = SourceQuitWatch::new(WhenSourceQuits::Stop);
    assert_eq!(
        stop.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Running),
        SourceQuitAction::Stop
    );
    assert_eq!(
        stop.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Off),
        SourceQuitAction::Keep
    );
    let mut switch = SourceQuitWatch::new(WhenSourceQuits::SwitchToSystem);
    assert_eq!(
        switch.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Running),
        SourceQuitAction::SwitchToSystem
    );
    assert_eq!(
        switch.app_exited(&AudioSource::System, Some(PLAYER), Streaming::Running),
        SourceQuitAction::Keep
    );
    assert_eq!(switch.switched_from(), Some(PLAYER));
}

// covers: S226
#[test]
fn picking_a_source_forgets_the_switch() {
    let mut watch = SourceQuitWatch::new(WhenSourceQuits::SwitchToSystem);
    watch.app_exited(&app(PLAYER), Some(PLAYER), Streaming::Running);
    watch.source_picked();
    assert_eq!(watch.switched_from(), None);
    assert_eq!(watch.setting(), WhenSourceQuits::SwitchToSystem);
}
