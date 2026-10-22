//! Calibrate All Speakers: synthetic recordings with the core's own signal
//! at known arrivals give the right sheet.

use audiosync_core::calibrate::{self, MAX_RECORDING_FRAMES, Recording, Refusal, Refused};
use audiosync_core::calibrate_all::{
    ApplyRefusal, Class, EndReason, Measurement, PlanError, Row, RowState, Run, SharedState, Sheet,
    Speaker, Step, Take, Target,
};
use audiosync_core::policy::delay::DEFAULT_AIRPLAY_DELAY_MS;

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

// covers: S235
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

// covers: S236
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

// covers: S236
#[test]
fn a_cast_offset_past_the_range_counts_from_its_end_and_only_a_cut_value_is_clamped() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("above", Class::Cast, Some(30_000.0), 50.0),
        speaker("lands", Class::Cast, Some(19_900.0), 50.0),
        speaker("cut", Class::Cast, Some(19_990.0), 50.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(10.0), Heard::At(10.0)]),
            (2, [Heard::At(150.0), Heard::At(150.0)]),
            (3, [Heard::At(150.0), Heard::At(150.0)]),
        ]),
    );
    let sheet = run.finish();
    let moved = |row: usize| match sheet.rows()[row].state {
        RowState::Certain(m) => (m.new_ms, m.clamped),
        other => panic!("row {row} not certain: {other:?}"),
    };
    assert_eq!(moved(1), (19_960.0, false), "30 000 counts as 20 000");
    assert_eq!(moved(2), (20_000.0, false), "landing on the end is no cut");
    assert_eq!(moved(3), (20_000.0, true));
}

// covers: S236, S238
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
    let mut sheet = run.finish();
    let (new, u) = measured(&sheet.rows()[1]);
    assert!(matches!(sheet.rows()[1].state, RowState::Uncertain(_)));
    assert_eq!(new, -211.0);
    close(u, 11.0 + 3.0);
    assert_eq!(sheet.apply(), Err(ApplyRefusal::NothingCertain));
}

// covers: S236
#[test]
fn uncertainty_at_the_limit_is_still_certain_and_just_over_it_is_not() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("bt", Class::Local, None, 200.0),
        speaker("den", Class::Local, None, 200.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    // 14 ms apart: U = 7 + 3 = 10; 15 ms apart: U = 10.5.
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(250.0), Heard::At(264.0)]),
            (2, [Heard::At(250.0), Heard::At(265.0)]),
        ]),
    );
    let sheet = run.finish();
    assert!(sheet.rows()[1].state.is_certain());
    close(measured(&sheet.rows()[1]).1, 10.0);
    assert!(matches!(sheet.rows()[2].state, RowState::Uncertain(_)));
    close(measured(&sheet.rows()[2]).1, 10.5);
}

// covers: S235, S236
#[test]
fn a_searching_window_can_find_an_arrival_no_offset_explains() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("tv", Class::Cast, None, 0.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    // 24 950 ms after the reference, past the 22 000 ms any two offsets span.
    let steps = drive(&mut run, arrivals(&[(1, [Heard::At(25_000.0); 2])]));
    assert_eq!(steps.len(), 2, "no second sweep");
    assert_eq!(
        run.finish().rows()[1].state,
        RowState::Refused(Refused {
            reason: Refusal::Implausible,
            recording: None,
        })
    );
}

// covers: S236
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
    let mut sheet = run.finish();
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

// covers: S236
#[test]
fn a_refused_reference_refuses_every_row_and_ends_the_steps() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    let steps = drive(&mut run, |_| Heard::Silence);
    assert_eq!(steps.len(), 1);
    let mut sheet = run.finish();
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

// covers: S237
#[test]
fn the_airplay_delay_is_the_median_of_the_certain_airplay_rows() {
    // Heard 100, 103 and 105 ms after the reference.
    let mut sheet = airplay_sheet(&[
        (1, [Heard::At(150.0), Heard::At(150.0)]),
        (2, [Heard::At(153.0), Heard::At(153.0)]),
        (3, [Heard::At(155.0), Heard::At(155.0)]),
    ]);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::Certain);
    assert_eq!(shared.new_ms, Some(2103.0));
    close(shared.spread_ms, 5.0);
    assert_eq!(shared.certain_rows, 3);
    assert!(!shared.clamped);
    let applied = sheet.apply().unwrap();
    assert_eq!(applied.changes.len(), 1, "one change for every AirPlay row");
    assert_eq!(applied.changes[0].target, Target::AirPlay);
    assert_eq!(applied.changes[0].old_ms, Some(2000.0));
    assert_eq!(applied.changes[0].new_ms, 2103.0);
}

// covers: S237
#[test]
fn an_airplay_spread_over_the_limit_is_reported_with_the_median() {
    // Heard 100, 104 and 120 ms late.
    let mut sheet = airplay_sheet(&[
        (1, [Heard::At(150.0), Heard::At(150.0)]),
        (2, [Heard::At(154.0), Heard::At(154.0)]),
        (3, [Heard::At(170.0), Heard::At(170.0)]),
    ]);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::Spread);
    close(shared.spread_ms, 20.0);
    assert_eq!(shared.new_ms, Some(2104.0));
    assert!(sheet.apply().is_ok(), "the median still applies");

    // The uncertain row doesn't count.
    let sheet = airplay_sheet(&[
        (1, [Heard::At(150.0), Heard::At(150.0)]),
        (2, [Heard::At(153.0), Heard::At(153.0)]),
        (3, [Heard::At(170.0), Heard::At(200.0)]),
    ]);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::Certain);
    assert_eq!(shared.certain_rows, 2);
    assert_eq!(shared.new_ms, Some(2102.0), "101.5 rounds up");

    let mut sheet = airplay_sheet(&[
        (1, [Heard::Silence, Heard::Silence]),
        (2, [Heard::At(150.0), Heard::At(180.0)]),
        (3, [Heard::Silence, Heard::Silence]),
    ]);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::NoneCertain);
    assert_eq!(shared.new_ms, None);
    assert_eq!(sheet.apply(), Err(ApplyRefusal::NothingCertain));
}

// covers: S235
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

// covers: S238
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
    let mut sheet = run.finish();
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

// covers: S239
#[test]
fn measure_again_replaces_one_row_from_a_new_reference() {
    // The reference isn't first, and one latency isn't even a number.
    let speakers = vec![
        speaker("bt", Class::Local, Some(10.0), 200.0),
        speaker("ref", Class::Local, None, 50.0),
        speaker("tv", Class::Cast, Some(400.0), 600.0),
        speaker("den", Class::Local, None, f64::NAN),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(
        &mut run,
        arrivals(&[
            (0, [Heard::At(250.0), Heard::At(250.0)]),
            (2, [Heard::At(650.0), Heard::At(650.0)]),
            (3, [Heard::At(80.0), Heard::At(110.0)]),
        ]),
    );
    let mut sheet = run.finish();
    assert!(matches!(sheet.rows()[3].state, RowState::Uncertain(_)));
    let untouched: Vec<Row> = sheet.rows()[..3].to_vec();

    assert!(sheet.measure_again(1).is_none(), "not the reference");
    assert!(sheet.measure_again(4).is_none());
    let mut again = sheet.measure_again(3).unwrap();
    let steps = drive(&mut again, |step| match step.take {
        Take::Reference => Heard::At(60.0),
        Take::Sweep(_) => Heard::At(80.0),
    });
    let named: Vec<(usize, Take)> = steps.iter().map(|s| (s.row, s.take)).collect();
    assert_eq!(
        named,
        [
            (1, Take::Reference),
            (3, Take::Sweep(1)),
            (3, Take::Sweep(2))
        ],
        "the steps name the sheet's rows"
    );
    assert!(sheet.replace(again.finish()));
    assert!(sheet.rows()[3].state.is_certain());
    assert_eq!(sheet.rows()[3].id, "den");
    assert_eq!(measured(&sheet.rows()[3]).0, -20.0);
    assert_eq!(sheet.rows()[..3], untouched[..]);

    let other = Run::new(RATE, "ref", mixed()).unwrap().finish();
    assert!(!sheet.replace(other), "only a Measure Again sheet");
}

// covers: S239, S240
#[test]
fn a_cancelled_or_changed_measure_again_leaves_the_sheet_as_it_was() {
    for reason in [EndReason::Cancelled, EndReason::SpeakerTurnedOff] {
        let mut run = Run::new(RATE, "ref", mixed()).unwrap();
        drive(&mut run, |_| Heard::At(50.0));
        let mut sheet = run.finish();
        let rows = sheet.rows().to_vec();
        let mut again = sheet.measure_again(2).unwrap();
        let step = again.step().unwrap();
        assert!(again.record(&recording(step.window_frames, Heard::At(50.0))));
        again.cancel(reason);
        assert!(!sheet.replace(again.finish()));
        assert_eq!(sheet.ended(), None);
        assert_eq!(sheet.rows(), &rows[..]);
        assert!(sheet.apply().is_ok());
    }
}

// covers: S238
#[test]
fn an_applied_sheet_is_spent() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    drive(&mut run, |_| Heard::At(60.0));
    let mut sheet = run.finish();
    let mut again = sheet.measure_again(1).unwrap();
    drive(&mut again, |_| Heard::At(60.0));
    let again = again.finish();
    let preview = sheet.changes().unwrap();
    assert!(!sheet.applied());
    let applied = sheet.apply().unwrap();
    assert_eq!(applied, preview);
    assert!(sheet.applied());
    assert_eq!(sheet.apply(), Err(ApplyRefusal::AlreadyApplied));
    assert!(sheet.measure_again(1).is_none());
    assert!(!sheet.replace(again), "nothing replaced once applied");
    assert_eq!(applied.undo.undo().len(), 3, "the undo still holds");
}

// covers: S240
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
        let mut sheet = run.finish();
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

// covers: S235
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

fn measurement(row: &Row) -> Measurement {
    match row.state {
        RowState::Certain(m) | RowState::Uncertain(m) => m,
        other => panic!("{} not measured: {other:?}", row.id),
    }
}

// covers: S235
#[test]
fn an_empty_speaker_list_has_no_reference() {
    assert_eq!(
        Run::new(RATE, "ref", Vec::new()).err(),
        Some(PlanError::NoReference)
    );
}

// covers: S235
#[test]
fn a_latency_that_is_negative_or_infinite_keeps_the_shortest_window() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("early", Class::Local, None, -500.0),
        speaker("inf", Class::Local, None, f64::INFINITY),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    let steps = drive(&mut run, |_| Heard::At(50.0));
    let window = |row| {
        let windows: Vec<usize> = steps
            .iter()
            .filter(|s| s.row == row)
            .map(|s| s.window_frames)
            .collect();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0], windows[1]);
        windows[0]
    };
    assert_eq!(window(1), (1750.0 * 48.0) as usize);
    assert_eq!(window(2), (1750.0 * 48.0) as usize);
    assert!(
        run.finish().rows()[1..]
            .iter()
            .all(|r| r.state.is_certain())
    );
}

// covers: S235
#[test]
fn a_window_the_recording_limit_would_cut_refuses_the_run() {
    // At 192 kHz 2^21 frames hold about 10.9 s: an arrival at 9100 ms plus
    // the sweep and 1 s fits, one at 9200 ms doesn't.
    const HIGH: u32 = 192_000;
    let arriving = |latency_ms| {
        vec![
            speaker("ref", Class::Local, None, 50.0),
            speaker("air", Class::AirPlay, Some(9000.0), latency_ms),
        ]
    };
    assert!(Run::new(HIGH, "ref", arriving(9100.0)).is_ok());
    assert_eq!(
        Run::new(HIGH, "ref", arriving(9200.0)).err(),
        Some(PlanError::TooLong)
    );
    assert!(Run::new(RATE, "ref", arriving(9200.0)).is_ok());
    assert_eq!(
        Run::new(RATE, "ref", arriving(1e9)).err(),
        Some(PlanError::TooLong)
    );
    let late_reference = vec![
        speaker("ref", Class::Local, None, 9200.0),
        speaker("usb", Class::Local, None, 50.0),
    ];
    assert_eq!(
        Run::new(HIGH, "ref", late_reference).err(),
        Some(PlanError::TooLong)
    );
    // A Cast speaker without an offset searches the whole limit anyway.
    let searching = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("tv", Class::Cast, None, 1e9),
    ];
    let mut run = Run::new(RATE, "ref", searching).unwrap();
    let reference = run.step().unwrap();
    assert!(run.record(&recording(reference.window_frames, Heard::At(50.0))));
    assert_eq!(run.step().unwrap().window_frames, MAX_RECORDING_FRAMES);
}

// covers: S237
#[test]
fn a_shared_airplay_delay_the_range_cuts_says_it_was_clamped() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("a", Class::AirPlay, Some(19_990.0), 50.0),
        speaker("b", Class::AirPlay, Some(19_990.0), 50.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    // Both heard 40 ms after the reference: 20 030 ms is wanted.
    drive(
        &mut run,
        arrivals(&[(1, [Heard::At(90.0); 2]), (2, [Heard::At(90.0); 2])]),
    );
    let sheet = run.finish();
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.state, SharedState::Certain);
    assert_eq!((shared.new_ms, shared.clamped), (Some(20_000.0), true));
}

// covers: S236
#[test]
fn each_class_is_clamped_to_its_own_range_and_says_so() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("low", Class::Local, Some(-1900.0), 300.0),
        speaker("high", Class::Local, Some(19_980.0), 50.0),
        speaker("cast-low", Class::Cast, Some(30.0), 50.0),
        speaker("cast-high", Class::Cast, Some(19_900.0), 300.0),
        // Above the largest hold-back the sheet counts it as 20 000 ms, the
        // most that plays, and moves it from there.
        speaker("cast-over", Class::Cast, Some(30_000.0), 50.0),
        speaker("air", Class::AirPlay, Some(19_950.0), 300.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(250.0); 2]),
            (2, [Heard::At(10.0); 2]),
            (3, [Heard::At(10.0); 2]),
            (4, [Heard::At(250.0); 2]),
            (5, [Heard::At(10.0); 2]),
            (6, [Heard::At(150.0); 2]),
        ]),
    );
    let sheet = run.finish();
    let got: Vec<(f64, bool)> = sheet.rows()[1..]
        .iter()
        .map(|row| {
            let m = measurement(row);
            (m.new_ms, m.clamped)
        })
        .collect();
    assert_eq!(
        got,
        [
            (-2000.0, true),
            (20_000.0, true),
            (0.0, true),
            (20_000.0, true),
            (19_960.0, false),
            (20_000.0, true),
        ]
    );
    assert_eq!(sheet.airplay().unwrap().new_ms, Some(20_000.0));

    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("air", Class::AirPlay, Some(20.0), 50.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(&mut run, arrivals(&[(1, [Heard::At(10.0); 2])]));
    let sheet = run.finish();
    let air = measurement(&sheet.rows()[1]);
    assert_eq!((air.new_ms, air.clamped), (0.0, true));
    assert_eq!(sheet.airplay().unwrap().new_ms, Some(0.0));
}

// covers: S236
#[test]
fn the_implausible_limit_is_22000_ms_on_either_sweep() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("edge", Class::Local, Some(0.0), 23_000.0),
        speaker("past", Class::Local, None, 23_000.0),
        speaker("tv", Class::Cast, None, 0.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    let steps = drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(22_050.0); 2]),
            (2, [Heard::At(22_050.0), Heard::At(22_051.0)]),
            (3, [Heard::At(22_050.0); 2]),
        ]),
    );
    assert_eq!(steps.len(), 7, "a good first sweep is followed by a second");
    let sheet = run.finish();
    let edge = measurement(&sheet.rows()[1]);
    assert!(sheet.rows()[1].state.is_certain());
    close(edge.difference_ms, 22_000.0);
    assert_eq!((edge.new_ms, edge.clamped), (-2000.0, true));
    assert_eq!(
        sheet.rows()[2].state,
        RowState::Refused(Refused {
            reason: Refusal::Implausible,
            recording: None,
        })
    );
    let tv = measurement(&sheet.rows()[3]);
    assert!(sheet.rows()[3].state.is_certain());
    assert_eq!((tv.new_ms, tv.clamped), (20_000.0, true));
}

// covers: S237
#[test]
fn an_even_count_takes_the_mean_of_the_middle_two_and_a_spread_at_the_limit_is_certain() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("a", Class::AirPlay, Some(2000.0), 2000.0),
        speaker("b", Class::AirPlay, Some(2000.0), 2000.0),
        speaker("c", Class::AirPlay, Some(2000.0), 2000.0),
        speaker("d", Class::AirPlay, Some(2000.0), 2000.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    // Heard 110, 100, 107 and 103 ms late: the middle two are 103 and 107.
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(160.0); 2]),
            (2, [Heard::At(150.0); 2]),
            (3, [Heard::At(157.0); 2]),
            (4, [Heard::At(153.0); 2]),
        ]),
    );
    let shared = *run.finish().airplay().unwrap();
    assert_eq!(shared.certain_rows, 4);
    assert_eq!(shared.new_ms, Some(2105.0));
    close(shared.spread_ms, 10.0);
    assert_eq!(shared.state, SharedState::Certain);
}

// covers: S237, S238
#[test]
fn speakers_without_a_stored_delay_apply_from_their_default_and_undo_to_none() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("air", Class::AirPlay, None, 2000.0),
        speaker("den", Class::Local, None, 50.0),
        speaker("tv", Class::Cast, None, 0.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(150.0); 2]),
            (2, [Heard::At(70.0); 2]),
            (3, [Heard::At(650.0); 2]),
        ]),
    );
    let mut sheet = run.finish();
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.old_ms, None);
    assert_eq!(shared.new_ms, Some(DEFAULT_AIRPLAY_DELAY_MS + 100.0));
    let applied = sheet.apply().unwrap();
    let written: Vec<(Target, Option<f64>, f64)> = applied
        .changes
        .iter()
        .map(|c| (c.target.clone(), c.old_ms, c.new_ms))
        .collect();
    assert_eq!(
        written,
        [
            (Target::Speaker("den".to_owned()), None, -20.0),
            (Target::Speaker("tv".to_owned()), None, 600.0),
            (Target::AirPlay, None, DEFAULT_AIRPLAY_DELAY_MS + 100.0),
        ],
        "speakers in the order given, the AirPlay delay last"
    );
    assert_eq!(
        applied.undo.undo(),
        [
            (Target::Speaker("den".to_owned()), None),
            (Target::Speaker("tv".to_owned()), None),
            (Target::AirPlay, None),
        ]
    );
}

// covers: S237, S238
#[test]
fn without_a_certain_airplay_row_only_the_other_speakers_change() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("air", Class::AirPlay, Some(1800.0), 2000.0),
        speaker("den", Class::Local, Some(5.0), 50.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(
        &mut run,
        arrivals(&[
            (1, [Heard::At(150.0), Heard::At(190.0)]),
            (2, [Heard::At(60.0); 2]),
        ]),
    );
    let mut sheet = run.finish();
    assert_eq!(sheet.airplay().unwrap().state, SharedState::NoneCertain);
    let applied = sheet.apply().unwrap();
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].target, Target::Speaker("den".to_owned()));
    assert_eq!(applied.changes[0].new_ms, -5.0);
}

// covers: S239
#[test]
fn measure_again_on_an_airplay_row_works_the_shared_delay_out_again() {
    let speakers = vec![
        speaker("ref", Class::Local, None, 50.0),
        speaker("a", Class::AirPlay, Some(2000.0), 2000.0),
        speaker("b", Class::AirPlay, Some(2000.0), 2000.0),
    ];
    let mut run = Run::new(RATE, "ref", speakers).unwrap();
    drive(
        &mut run,
        arrivals(&[(1, [Heard::At(150.0); 2]), (2, [Heard::Silence; 2])]),
    );
    let mut sheet = run.finish();
    assert_eq!(sheet.airplay().unwrap().certain_rows, 1);
    assert_eq!(sheet.airplay().unwrap().new_ms, Some(2100.0));
    let first = sheet.rows()[1].clone();

    let mut again = sheet.measure_again(2).unwrap();
    drive(&mut again, |step| match step.take {
        Take::Reference => Heard::At(50.0),
        Take::Sweep(_) => Heard::At(160.0),
    });
    assert!(sheet.replace(again.finish()));
    assert_eq!(sheet.rows()[1], first);
    let shared = *sheet.airplay().unwrap();
    assert_eq!(shared.certain_rows, 2);
    assert_eq!(shared.new_ms, Some(2105.0));
    close(shared.spread_ms, 10.0);
}

// covers: S239, S240
#[test]
fn an_ended_sheet_takes_no_row_from_another_sheet() {
    let mut ended = Run::new(RATE, "ref", mixed()).unwrap();
    ended.cancel(EndReason::SpeakersChanged);
    let mut ended = ended.finish();
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    drive(&mut run, |_| Heard::At(50.0));
    let sheet = run.finish();
    let mut again = sheet.measure_again(1).unwrap();
    drive(&mut again, |_| Heard::At(50.0));
    let rows = ended.rows().to_vec();
    assert!(!ended.replace(again.finish()));
    assert_eq!(ended.rows(), &rows[..]);
    assert_eq!(ended.ended(), Some(EndReason::SpeakersChanged));
}

// covers: S240
#[test]
fn a_cancel_after_the_last_step_changes_nothing() {
    let mut run = Run::new(RATE, "ref", mixed()).unwrap();
    drive(&mut run, |_| Heard::At(50.0));
    run.cancel(EndReason::SourceChanged);
    let mut sheet = run.finish();
    assert_eq!(sheet.ended(), None);
    assert!(sheet.apply().is_ok());
}
