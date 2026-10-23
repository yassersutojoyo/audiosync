//! Calibrate Zone (S255): the zone's plan, the sheet's off rows and the
//! AirPlay line, with synthetic recordings of the core's own signal.

use audiosync_core::calibrate;
use audiosync_core::calibrate_all::{
    AirPlayMoves, Class, EndReason, PlanError, RowState, Run, SharedState, Speaker, Take, ZonePlan,
    ZoneSpeaker, airplay_moves, zone_plan,
};
use audiosync_core::policy::zones::{Zone, Zones};

const RATE: u32 = 48_000;

fn listed(key: &str, present: bool, on: bool) -> ZoneSpeaker {
    ZoneSpeaker {
        key: key.to_owned(),
        present,
        on,
    }
}

fn keys(members: &[&str]) -> Vec<String> {
    members.iter().map(|key| (*key).to_owned()).collect()
}

fn speaker(id: &str, class: Class, offset_ms: Option<f64>, latency_ms: f64) -> Speaker {
    Speaker {
        id: id.to_owned(),
        class,
        offset_ms,
        latency_ms,
    }
}

fn heard_at(frames: usize, at_ms: f64) -> Vec<f32> {
    let signal = calibrate::signal(RATE).expect("a supported rate");
    let mut recording = vec![0.0f32; frames];
    let start = (at_ms * f64::from(RATE) / 1000.0).round() as usize;
    for (slot, sample) in recording[start..].iter_mut().zip(&signal) {
        *slot = sample * 2.0;
    }
    recording
}

/// Plays every step, the reference heard at 50 ms and row `n` at `at(n)`;
/// returns the rows swept.
fn drive(run: &mut Run, at: impl Fn(usize) -> f64) -> Vec<usize> {
    let mut rows = Vec::new();
    while let Some(step) = run.step() {
        let ms = match step.take {
            Take::Reference => 50.0,
            Take::Sweep(_) => at(step.row),
        };
        assert!(run.record(&heard_at(step.window_frames, ms)));
        rows.push(step.row);
    }
    rows
}

// covers: S255, S235
#[test]
fn a_zone_run_measures_the_reference_and_present_members_that_are_on() {
    // The host's list order differs from the zone's, and the reference is in no zone.
    let speakers = [
        listed("ap:kitchen", true, true),
        listed("lc:mac", true, true),
        listed("lc:den", true, false),
        listed("lc:usb", true, true),
    ];
    let zone = keys(&["lc:usb", "lc:den", "ap:kitchen"]);
    let plan = zone_plan(&zone, "lc:mac", &speakers).unwrap();
    assert_eq!(
        plan,
        ZonePlan {
            rows: vec![1, 3, 2, 0],
            off: vec![false, false, true, false],
            silenced: vec![],
        },
        "the reference first, then the members in zone order, the one that is off listed"
    );

    let run_speakers = vec![
        speaker("lc:mac", Class::Local, None, 50.0),
        speaker("lc:usb", Class::Local, Some(10.0), 200.0),
        speaker("lc:den", Class::Local, Some(5.0), 0.0),
        speaker("ap:kitchen", Class::AirPlay, Some(2000.0), 2000.0),
    ];
    let mut run = Run::listing(RATE, "lc:mac", run_speakers, &plan.off).unwrap();
    let swept = drive(&mut run, |row| if row == 1 { 250.0 } else { 150.0 });
    assert_eq!(
        swept,
        [0, 1, 1, 3, 3],
        "the member that is off is never swept"
    );
    let mut sheet = run.finish();
    assert_eq!(sheet.ended(), None);
    assert_eq!(sheet.rows()[2].state, RowState::NotMeasured);
    assert_eq!(sheet.rows()[2].old_ms, Some(5.0));
    assert!(sheet.is_off(2) && !sheet.is_off(1) && !sheet.is_off(0));
    assert!(sheet.rows()[1].state.is_certain() && sheet.rows()[3].state.is_certain());
    let changes = sheet.changes().unwrap().changes;
    assert_eq!(changes.len(), 2, "the off row writes nothing: {changes:?}");

    // Turned on, the off member is measured by Measure Again from its values now.
    assert!(
        sheet.measure_again(2).is_none(),
        "an off row needs the host's values now"
    );
    assert!(
        sheet.measure_again_off(1, None, 0.0).is_none(),
        "only an off row"
    );
    let mut again = sheet.measure_again_off(2, Some(30.0), 100.0).unwrap();
    assert_eq!(drive(&mut again, |_| 90.0), [0, 2, 2]);
    assert!(sheet.replace(again.finish()));
    assert!(!sheet.is_off(2));
    assert_eq!(sheet.rows()[2].old_ms, Some(30.0));
    match sheet.rows()[2].state {
        RowState::Certain(m) => assert_eq!(m.new_ms, -10.0),
        other => panic!("not measured: {other:?}"),
    }
    assert!(sheet.apply().is_ok());

    let only_off = [listed("lc:mac", true, true), listed("lc:den", true, false)];
    assert_eq!(
        zone_plan(&keys(&["lc:den"]), "lc:mac", &only_off),
        Err(PlanError::NothingToMeasure),
        "only the reference measurable"
    );
    assert_eq!(
        zone_plan(&keys(&["lc:mac"]), "lc:mac", &only_off),
        Err(PlanError::NothingToMeasure),
        "the reference as the zone's one member"
    );
    let reference_off = [listed("lc:mac", true, false), listed("lc:usb", true, true)];
    assert_eq!(
        zone_plan(&keys(&["lc:usb"]), "lc:mac", &reference_off),
        Err(PlanError::NoReference)
    );
    let all_off = vec![
        speaker("lc:mac", Class::Local, None, 50.0),
        speaker("lc:den", Class::Local, None, 0.0),
    ];
    assert_eq!(
        Run::listing(RATE, "lc:mac", all_off, &[true, true]).err(),
        Some(PlanError::NothingToMeasure),
        "the reference is swept whatever its entry, and nothing else is left"
    );
}

// covers: S255, S246
#[test]
fn a_zone_run_leaves_hidden_absent_and_target_members_out() {
    let speakers = [
        listed("lc:mac", true, true),
        listed("lc:usb", true, true),
        // Hidden: listed, not present.
        listed("lc:hidden", false, false),
        // Plays as part of a stereo pair: on, but not present.
        listed("ap:left", false, true),
        listed("ap:pair:left+right", true, true),
    ];
    let zone = keys(&["lc:gone", "lc:hidden", "ap:left", "lc:usb", "lc:usb"]);
    let plan = zone_plan(&zone, "lc:mac", &speakers).unwrap();
    assert_eq!(
        plan.rows,
        [0, 1],
        "absent, hidden and in-target members are left out"
    );
    assert_eq!(plan.off, [false, false]);
    assert_eq!(
        plan.silenced,
        [3, 4],
        "what plays but isn't measured is silenced"
    );
}

// covers: S255
#[test]
fn a_zone_run_silences_speakers_outside_the_zone() {
    let speakers = [
        listed("lc:mac", true, true),
        listed("lc:desk", true, true),
        listed("ap:den", true, true),
        listed("cc:tv", true, true),
        listed("ap:off", true, false),
        listed("ap:kitchen", true, true),
    ];
    let plan = zone_plan(&keys(&["ap:kitchen"]), "lc:mac", &speakers).unwrap();
    assert_eq!(plan.rows, [0, 5]);
    assert_eq!(
        plan.silenced,
        [1, 2, 3],
        "every speaker on outside the zone, never the reference or one that is off"
    );

    // An off member's window never refuses the run: it isn't swept.
    let run_speakers = vec![
        speaker("lc:mac", Class::Local, None, 50.0),
        speaker("lc:far", Class::Local, None, 1.0e9),
        speaker("ap:kitchen", Class::AirPlay, Some(2000.0), 2000.0),
    ];
    assert!(Run::new(RATE, "lc:mac", run_speakers.clone()).is_err());
    assert!(Run::listing(RATE, "lc:mac", run_speakers, &[false, true, false]).is_ok());
}

// covers: S255, S237
#[test]
fn the_airplay_line_names_the_other_zones_it_moves() {
    let zone = |id: &str, name: &str, members: &[&str]| Zone {
        id: id.to_owned(),
        name: name.to_owned(),
        members: keys(members),
        volume: 100.0,
    };
    let zones = Zones::from_stored([
        zone("z1", "Upstairs", &["ap:bedroom", "lc:usb"]),
        zone("z2", "Kitchen", &["ap:kitchen"]),
        zone("z3", "Desk", &["lc:desk"]),
        zone("z4", "Garden", &["ap:garden"]),
    ]);
    let listed = ["ap:garden", "ap:bedroom", "ap:kitchen"];
    assert_eq!(
        airplay_moves(&zones, "z1", listed),
        AirPlayMoves {
            zones: vec!["Kitchen".to_owned(), "Garden".to_owned()],
            no_zone: false,
        },
        "in zone order, never the measured zone or one without an AirPlay speaker"
    );
    let moves = airplay_moves(&zones, "z2", ["ap:kitchen", "ap:porch"]);
    assert_eq!(moves.zones, Vec::<String>::new());
    assert!(moves.no_zone, "a listed AirPlay speaker in no zone");

    // The off AirPlay row neither sets the old delay nor counts toward the median.
    let run_speakers = vec![
        speaker("lc:mac", Class::Local, None, 50.0),
        speaker("ap:off", Class::AirPlay, None, 0.0),
        speaker("ap:kitchen", Class::AirPlay, Some(2000.0), 2000.0),
    ];
    let mut run = Run::listing(RATE, "lc:mac", run_speakers, &[false, true, false]).unwrap();
    drive(&mut run, |_| 150.0);
    let sheet = run.finish();
    let shared = sheet.airplay().copied().unwrap();
    assert_eq!(shared.old_ms, Some(2000.0));
    assert_eq!(shared.new_ms, Some(2100.0));
    assert_eq!(
        (shared.certain_rows, shared.state),
        (1, SharedState::Certain)
    );
}

// covers: S240, S255
#[test]
fn a_zone_run_ended_by_a_change_keeps_its_off_rows_and_applies_nothing() {
    let run_speakers = vec![
        speaker("lc:mac", Class::Local, None, 50.0),
        speaker("lc:usb", Class::Local, None, 50.0),
        speaker("lc:den", Class::Local, None, 50.0),
    ];
    let mut run = Run::listing(RATE, "lc:mac", run_speakers, &[false, false, true]).unwrap();
    let step = run.step().unwrap();
    assert!(run.record(&heard_at(step.window_frames, 50.0)));
    run.cancel(EndReason::SpeakersChanged);
    let mut sheet = run.finish();
    assert_eq!(sheet.ended(), Some(EndReason::SpeakersChanged));
    assert!(sheet.is_off(2));
    assert!(sheet.apply().is_err());
    assert!(sheet.measure_again_off(2, None, 50.0).is_none());
}
