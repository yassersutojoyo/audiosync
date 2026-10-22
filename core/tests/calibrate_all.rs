//! Calibrate All Speakers: synthetic recordings with the core's own signal
//! at known arrivals give the right sheet.

use audiosync_core::calibrate::{self, MAX_RECORDING_FRAMES, Recording, Refusal, Refused};
use audiosync_core::calibrate_all::{
    ApplyRefusal, Class, EndReason, PlanError, Row, RowState, Run, SharedState, Sheet, Speaker,
    Step, Take, Target,
};

const RATE: u32 = 48_000;

fn speaker(id: &str, class: Class, offset_ms: Option<f64>, latency_ms: f64) -> Speaker {
    Speaker {
        id: id.to_owned(),
        class,
        offset_ms,
        latency_ms,
    }
}

/// What the microphone hears in one step.
#[derive(Clone, Copy)]
enum Heard {
    At(f64),
    Silence,
    Clipped(f64),
}

/// A recording of `frames` with the signal at `heard`.
fn recording(frames: usize, heard: Heard) -> Vec<f32> {
    let mut samples = vec![0.0f32; frames];
    let at_ms = match heard {
        Heard::Silence => return samples,
        Heard::At(at_ms) | Heard::Clipped(at_ms) => at_ms,
    };
    let start = (at_ms * f64::from(RATE) / 1000.0).round() as usize;
    let signal = calibrate::signal(RATE).expect("a supported rate");
    for (slot, sample) in samples[start..].iter_mut().zip(&signal) {
        *slot = sample * 2.0;
    }
    if let Heard::Clipped(_) = heard {
        samples[start..start + 4].fill(1.0);
    }
    samples
}

/// Drives `run` to its end: `heard` gives, for each step, where the sweep
/// is heard. Returns the steps taken.
fn drive(run: &mut Run, mut heard: impl FnMut(&Step) -> Heard) -> Vec<Step> {
    let mut steps = Vec::new();
    while let Some(step) = run.step() {
        assert!(run.record(&recording(step.window_frames, heard(&step))));
        steps.push(step);
    }
    assert!(!run.record(&[]), "nothing waits once the run is over");
    steps
}

/// A table of arrivals by row and take; the reference is heard at 50 ms.
fn arrivals(table: &[(usize, [Heard; 2])]) -> impl FnMut(&Step) -> Heard + '_ {
    move |step| match step.take {
        Take::Reference => Heard::At(50.0),
        Take::Sweep(n) => {
            table
                .iter()
                .find(|(row, _)| *row == step.row)
                .expect("a row in the table")
                .1[n - 1]
        }
    }
}

fn measured(row: &Row) -> (f64, f64) {
    match row.state {
        RowState::Certain(m) | RowState::Uncertain(m) => (m.new_ms, m.uncertainty_ms),
        other => panic!("{} not measured: {other:?}", row.id),
    }
}

fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 0.05,
        "{actual} against {expected}"
    );
}

fn mixed() -> Vec<Speaker> {
    vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("bt", Class::Local, Some(10.0), 200.0),
        speaker("tv", Class::Cast, Some(400.0), 600.0),
        speaker("den", Class::Local, None, 50.0),
    ]
}

#[test]
fn the_reference_comes_first_then_each_speaker_twice_with_its_window() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    let steps = drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(250.0), Heard::At(252.0)]),
            (2, [Heard::At(650.0), Heard::At(650.0)]),
            (3, [Heard::At(50.0), Heard::At(50.0)]),
        ]),
    );
    let order: Vec<(usize, Take)> = steps.iter().map(|s| (s.row, s.take)).collect();
    assert_eq!(
        order,
        [
            (0, Take::Reference),
            (1, Take::Sweep(1)),
            (1, Take::Sweep(2)),
            (2, Take::Sweep(1)),
            (2, Take::Sweep(2)),
            (3, Take::Sweep(1)),
            (3, Take::Sweep(2)),
        ]
    );
    // The expected arrival, the 0.75 s sweep and 1 s more.
    let frames = |ms: f64| (ms * 48.0) as usize;
    assert_eq!(steps[0].window_frames, frames(50.0 + 1750.0));
    assert_eq!(steps[1].window_frames, frames(200.0 + 1750.0));
    assert_eq!(steps[3].window_frames, frames(600.0 + 1750.0));
    assert!(steps.iter().all(|s| !s.searching));
}

#[test]
fn certain_rows_move_each_class_its_own_way() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(250.0), Heard::At(254.0)]),
            (2, [Heard::At(650.0), Heard::At(650.0)]),
            (3, [Heard::At(40.0), Heard::At(40.0)]),
        ]),
    );
    let sheet = run.finish();
    assert_eq!(sheet.ended(), None);
    assert_eq!(sheet.rows()[0].state, RowState::Reference);
    // Heard 202 ms after the reference: a local offset drops by that.
    let (new, u) = measured(&sheet.rows()[1]);
    assert!(sheet.rows()[1].state.is_certain());
    assert_eq!(new, 10.0 - 202.0);
    close(u, 2.0 + 3.0);
    // A Cast speaker heard 600 ms late: the others wait 600 ms longer.
    let (new, u) = measured(&sheet.rows()[2]);
    assert_eq!(new, 1000.0);
    close(u, 3.0);
    // Heard before the reference: a local offset without a stored value rises.
    assert_eq!(measured(&sheet.rows()[3]).0, 10.0);
    assert!(sheet.airplay().is_none());
}

#[test]
fn sweeps_too_far_apart_leave_the_row_uncertain_and_apply_is_refused() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("bt", Class::Local, Some(0.0), 200.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(
        &mut run,
        arrivals(&[(1, [Heard::At(250.0), Heard::At(272.0)])]),
    );
    let sheet = run.finish();
    let (new, u) = measured(&sheet.rows()[1]);
    assert!(matches!(sheet.rows()[1].state, RowState::Uncertain(_)));
    assert_eq!(new, -211.0);
    close(u, 11.0 + 3.0);
    assert_eq!(sheet.apply(), Err(ApplyRefusal::NothingCertain));
}

#[test]
fn uncertainty_at_the_limit_is_still_certain() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("bt", Class::Local, None, 200.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    // 14 ms apart: U = 7 + 3 = 10.
    drive(
        &mut run,
        arrivals(&[(1, [Heard::At(250.0), Heard::At(264.0)])]),
    );
    let sheet = run.finish();
    assert!(sheet.rows()[1].state.is_certain());
    close(measured(&sheet.rows()[1]).1, 10.0);
}

#[test]
fn a_quiet_or_clipped_sweep_refuses_the_row_and_is_never_averaged() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    let steps = drive(
        &mut run,
        arrivals(&[
            (1, [Heard::Silence, Heard::At(250.0)]),
            (2, [Heard::At(650.0), Heard::Clipped(650.0)]),
            (3, [Heard::At(50.0), Heard::At(50.0)]),
        ]),
    );
    // A refused first sweep skips the second.
    assert_eq!(steps.iter().filter(|s| s.row == 1).count(), 1);
    let sheet = run.finish();
    let chosen = |reason| {
        RowState::Refused(Refused {
            reason,
            recording: Some(Recording::Chosen),
        })
    };
    assert_eq!(sheet.rows()[1].state, chosen(Refusal::TooQuiet));
    assert_eq!(sheet.rows()[2].state, chosen(Refusal::Clipped));
    assert!(sheet.rows()[3].state.is_certain());
    let applied = sheet.apply().unwrap();
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].target, Target::Speaker("den".to_owned()));
}

#[test]
fn a_refused_reference_refuses_every_row_and_ends_the_steps() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    let steps = drive(&mut run, |_| Heard::Silence);
    assert_eq!(steps.len(), 1);
    let sheet = run.finish();
    assert_eq!(sheet.ended(), None);
    for row in &sheet.rows()[1..] {
        assert_eq!(
            row.state,
            RowState::Refused(Refused {
                reason: Refusal::TooQuiet,
                recording: Some(Recording::Reference),
            })
        );
    }
    assert_eq!(sheet.apply(), Err(ApplyRefusal::NothingCertain));
}

fn airplay_sheet(heard: &[(usize, [Heard; 2])]) -> Sheet {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("a", Class::AirPlay, Some(2000.0), 2000.0),
        speaker("b", Class::AirPlay, Some(2000.0), 2000.0),
        speaker("c", Class::AirPlay, Some(2000.0), 2000.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(&mut run, arrivals(heard));
    run.finish()
}

#[test]
fn the_airplay_delay_is_the_median_of_the_certain_airplay_rows() {
    // Heard 100, 103 and 105 ms after the reference.
    let sheet = airplay_sheet(&[
        (1, [Heard::At(150.0), Heard::At(150.0)]),
        (2, [Heard::At(153.0), Heard::At(153.0)]),
        (3, [Heard::At(155.0), Heard::At(155.0)]),
    ]);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::Certain);
    assert_eq!(shared.new_ms, Some(2103.0));
    close(shared.spread_ms, 5.0);
    assert_eq!(shared.certain_rows, 3);
    let applied = sheet.apply().unwrap();
    assert_eq!(applied.changes.len(), 1, "one change for every AirPlay row");
    assert_eq!(applied.changes[0].target, Target::AirPlay);
    assert_eq!(applied.changes[0].old_ms, Some(2000.0));
    assert_eq!(applied.changes[0].new_ms, 2103.0);
}

#[test]
fn an_airplay_spread_over_the_limit_is_reported_with_the_median() {
    // Heard 100, 104 and 120 ms late; the uncertain row doesn't count.
    let sheet = airplay_sheet(&[
        (1, [Heard::At(150.0), Heard::At(150.0)]),
        (2, [Heard::At(154.0), Heard::At(154.0)]),
        (3, [Heard::At(170.0), Heard::At(170.0)]),
    ]);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::Spread);
    close(shared.spread_ms, 20.0);
    assert_eq!(shared.new_ms, Some(2104.0));
    assert!(sheet.apply().is_ok(), "the median still applies");

    let sheet = airplay_sheet(&[
        (1, [Heard::At(150.0), Heard::At(150.0)]),
        (2, [Heard::At(153.0), Heard::At(153.0)]),
        (3, [Heard::At(170.0), Heard::At(200.0)]),
    ]);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::Certain);
    assert_eq!(shared.certain_rows, 2);
    assert_eq!(shared.new_ms, Some(2102.0), "101.5 rounds up");

    let sheet = airplay_sheet(&[
        (1, [Heard::Silence, Heard::Silence]),
        (2, [Heard::At(150.0), Heard::At(180.0)]),
        (3, [Heard::Silence, Heard::Silence]),
    ]);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::NoneCertain);
    assert_eq!(shared.new_ms, None);
    assert_eq!(sheet.apply(), Err(ApplyRefusal::NothingCertain));
}

#[test]
fn a_cast_speaker_without_an_offset_searches_the_longest_window_first() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("tv", Class::Cast, None, 0.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    let steps = drive(
        &mut run,
        arrivals(&[(1, [Heard::At(3050.0), Heard::At(3052.0)])]),
    );
    assert_eq!(steps[1].window_frames, MAX_RECORDING_FRAMES);
    assert!(steps[1].searching);
    // The first arrival, the sweep and 1 s.
    assert_eq!(steps[2].window_frames, (4800.0 * 48.0) as usize);
    assert!(!steps[2].searching);
    let sheet = run.finish();
    assert_eq!(measured(&sheet.rows()[1]).0, 3001.0);
    assert_eq!(sheet.rows()[1].old_ms, None);
}

#[test]
fn apply_and_undo_round_trip_to_the_stored_values() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("bt", Class::Local, Some(-20.0), 200.0),
        speaker("den", Class::Local, None, 50.0),
        speaker("tv", Class::Cast, Some(400.0), 600.0),
        speaker("a", Class::AirPlay, Some(1800.0), 2000.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers.clone()).unwrap();
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(250.0), Heard::At(250.0)]),
            (2, [Heard::At(60.0), Heard::At(60.0)]),
            (3, [Heard::Silence, Heard::Silence]),
            (4, [Heard::At(150.0), Heard::At(150.0)]),
        ]),
    );
    let sheet = run.finish();
    let mut store: Vec<(Target, Option<f64>)> = speakers[1..4]
        .iter()
        .map(|s| (Target::Speaker(s.id.clone()), s.offset_ms))
        .chain([(Target::AirPlay, Some(1800.0))])
        .collect();
    let before = store.clone();
    let applied = sheet.apply().unwrap();
    for change in &applied.changes {
        let slot = store.iter_mut().find(|(t, _)| *t == change.target).unwrap();
        assert_eq!(slot.1, change.old_ms);
        slot.1 = Some(change.new_ms);
    }
    assert_eq!(
        store,
        [
            (Target::Speaker("bt".to_owned()), Some(-220.0)),
            (Target::Speaker("den".to_owned()), Some(-10.0)),
            (Target::Speaker("tv".to_owned()), Some(400.0)),
            (Target::AirPlay, Some(1900.0)),
        ]
    );
    let previous = applied.undo.undo();
    assert_eq!(
        previous,
        [
            (Target::Speaker("bt".to_owned()), Some(-20.0)),
            (Target::Speaker("den".to_owned()), None),
            (Target::AirPlay, Some(1800.0)),
        ]
    );
    for (target, old) in previous {
        store.iter_mut().find(|(t, _)| *t == target).unwrap().1 = old;
    }
    assert_eq!(store, before);
}

#[test]
fn measure_again_replaces_one_row_from_a_new_reference() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(250.0), Heard::At(280.0)]),
            (2, [Heard::At(650.0), Heard::At(650.0)]),
            (3, [Heard::At(50.0), Heard::At(50.0)]),
        ]),
    );
    let mut sheet = run.finish();
    assert!(matches!(sheet.rows()[1].state, RowState::Uncertain(_)));
    let untouched: Vec<Row> = sheet.rows()[2..].to_vec();

    assert!(sheet.measure_again(0).is_none(), "not the reference");
    assert!(sheet.measure_again(4).is_none());
    let mut again = sheet.measure_again(1).unwrap();
    let steps = drive(&mut again, |step| match step.take {
        Take::Reference => Heard::At(60.0),
        Take::Sweep(_) => Heard::At(260.0),
    });
    assert_eq!(steps.len(), 3);
    assert_eq!((steps[0].row, steps[1].row), (0, 1));
    assert!(sheet.replace(again.finish()));
    assert!(sheet.rows()[1].state.is_certain());
    assert_eq!(sheet.rows()[1].id, "bt");
    assert_eq!(measured(&sheet.rows()[1]).0, 10.0 - 200.0);
    assert_eq!(sheet.rows()[2..], untouched[..]);

    let other = Run::new(RATE, "ref", mixed()).unwrap().finish();
    assert!(!sheet.replace(other), "only a Measure Again sheet");
}

#[test]
fn a_cancelled_measure_again_ends_the_sheet() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    drive(&mut run, |_| Heard::At(50.0));
    let mut sheet = run.finish();
    assert!(sheet.apply().is_ok());
    let mut again = sheet.measure_again(2).unwrap();
    again.cancel(EndReason::SpeakerTurnedOff);
    assert!(sheet.replace(again.finish()));
    assert_eq!(sheet.ended(), Some(EndReason::SpeakerTurnedOff));
    assert_eq!(
        sheet.apply(),
        Err(ApplyRefusal::Ended(EndReason::SpeakerTurnedOff))
    );
}

#[test]
fn cancel_or_a_change_mid_run_applies_nothing() {
    for reason in [
        EndReason::Cancelled,
        EndReason::SpeakersChanged,
        EndReason::SpeakerTurnedOff,
        EndReason::SourceChanged,
    ] {
        let mut run = Run::new(RATE, "ref", mixed()).unwrap();
        for _ in 0..3 {
            let step = run.step().unwrap();
            assert!(run.record(&recording(step.window_frames, Heard::At(50.0))));
        }
        run.cancel(reason);
        assert_eq!(run.step(), None);
        assert!(!run.record(&recording(100_000, Heard::At(50.0))));
        let sheet = run.finish();
        assert_eq!(sheet.ended(), Some(reason));
        assert!(
            sheet.rows()[1].state.is_certain(),
            "measured rows stay shown"
        );
        assert_eq!(sheet.rows()[2].state, RowState::NotMeasured);
        assert_eq!(sheet.apply(), Err(ApplyRefusal::Ended(reason)));
        assert!(sheet.measure_again(1).is_none());
    }
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    let step = run.step().unwrap();
    assert!(run.record(&recording(step.window_frames, Heard::At(50.0))));
    assert_eq!(
        run.finish().ended(),
        Some(EndReason::Cancelled),
        "finished early"
    );
}

#[test]
fn a_run_needs_a_local_reference_and_something_to_measure() {
    let cast_ref = vec![
        speaker("ref", Class::Cast, None, 50.0),
        speaker("bt", Class::Local, None, 50.0),
    ];
    assert_eq!(
        Run::new(RATE, "ref", cast_ref).err(),
        Some(PlanError::NoReference)
    );
    assert_eq!(
        Run::new(RATE, "nope", mixed()).err(),
        Some(PlanError::NoReference)
    );
    let alone = vec![speaker("ref", Class::Local, None, 50.0)];
    assert_eq!(
        Run::new(RATE, "ref", alone).err(),
        Some(PlanError::NothingToMeasure)
    );
    let mut twice = mixed();
    twice.push(speaker("bt", Class::Local, None, 0.0));
    assert_eq!(
        Run::new(RATE, "ref", twice).err(),
        Some(PlanError::DuplicateId)
    );
    assert_eq!(
        Run::new(8000, "ref", mixed()).err(),
        Some(PlanError::UnsupportedRate)
    );
}
