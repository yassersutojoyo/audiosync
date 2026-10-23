//! Calibrate All Speakers through the C interface: a run the host drives
//! step by step and the results sheet it turns into.
//!
//! The rules are in [`calibrate_all`]. The host names speakers by their
//! index in the array it passed to [`asc_calibration_run_create`], and rows
//! keep that order. Every call is for one control thread at a time per
//! handle; [`asc_calibration_run_record`] runs an FFT over the recording.

use std::ptr;

use audiosync_cast::audio::Codec;

use super::calibrate::{
    CALIBRATION_RECORDING_CHOSEN, CALIBRATION_RECORDING_NONE, CALIBRATION_RECORDING_REFERENCE,
    refusal_code,
};
use super::cast::{CAST_FORMAT_FLAC, CAST_FORMAT_WAV};
use super::host_slice;
use crate::calibrate::Recording;
use crate::calibrate_all::{
    self, ApplyRefusal, Class, EndReason, Measurement, RowState, Run, SharedState, Sheet, Speaker,
    Take, Target,
};

/// [`Class::Local`].
pub const CALIBRATION_KIND_LOCAL: u32 = 0;
/// [`Class::Cast`].
pub const CALIBRATION_KIND_CAST: u32 = 1;
/// [`Class::AirPlay`].
pub const CALIBRATION_KIND_AIRPLAY: u32 = 2;

/// [`RowState::Reference`].
pub const CALIBRATION_ROW_REFERENCE: u32 = 0;
/// [`RowState::NotMeasured`].
pub const CALIBRATION_ROW_NOT_MEASURED: u32 = 1;
/// [`RowState::Certain`].
pub const CALIBRATION_ROW_CERTAIN: u32 = 2;
/// [`RowState::Uncertain`].
pub const CALIBRATION_ROW_UNCERTAIN: u32 = 3;
/// [`RowState::Refused`].
pub const CALIBRATION_ROW_REFUSED: u32 = 4;

/// [`SharedState::NoneCertain`].
pub const CALIBRATION_AIRPLAY_NONE_CERTAIN: u32 = 0;
/// [`SharedState::Certain`].
pub const CALIBRATION_AIRPLAY_CERTAIN: u32 = 1;
/// [`SharedState::Spread`].
pub const CALIBRATION_AIRPLAY_SPREAD: u32 = 2;

/// The run or sheet goes on.
pub const CALIBRATION_END_NONE: u32 = 0;
/// [`EndReason::Cancelled`].
pub const CALIBRATION_END_CANCELLED: u32 = 1;
/// [`EndReason::SpeakersChanged`].
pub const CALIBRATION_END_SPEAKERS_CHANGED: u32 = 2;
/// [`EndReason::SpeakerTurnedOff`].
pub const CALIBRATION_END_SPEAKER_OFF: u32 = 3;
/// [`EndReason::SourceChanged`].
pub const CALIBRATION_END_SOURCE_CHANGED: u32 = 4;

/// The changes were written.
pub const CALIBRATION_APPLY_OK: u32 = 0;
/// [`ApplyRefusal::Ended`].
pub const CALIBRATION_APPLY_ENDED: u32 = 1;
/// [`ApplyRefusal::NothingCertain`].
pub const CALIBRATION_APPLY_NOTHING_CERTAIN: u32 = 2;
/// More changes than the array holds; `count` says how many.
pub const CALIBRATION_APPLY_TOO_MANY: u32 = 3;
/// A null argument.
pub const CALIBRATION_APPLY_INVALID: u32 = 4;
/// [`ApplyRefusal::AlreadyApplied`].
pub const CALIBRATION_APPLY_ALREADY_APPLIED: u32 = 5;

/// A Calibrate All Speakers run behind an opaque C pointer.
pub struct AscCalibrationRun {
    run: Run,
}

/// A results sheet behind an opaque C pointer.
pub struct AscCalibrationSheet {
    sheet: Sheet,
}

/// One speaker that is on when the run starts.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscCalibrationSpeaker {
    pub kind: u32,
    pub has_offset: bool,
    pub offset_ms: f64,
    pub latency_ms: f64,
}

/// What [`asc_calibration_run_step`] asks the host to play and record.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscCalibrationStep {
    pub row: usize,
    pub sweep: u32,
    pub window_frames: usize,
    pub searching: bool,
}

/// One row of the sheet.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscCalibrationRow {
    pub state: u32,
    pub reason: u32,
    pub recording: u32,
    pub has_old: bool,
    pub old_ms: f64,
    pub new_ms: f64,
    pub uncertainty_ms: f64,
    pub difference_ms: f64,
    pub clamped: bool,
}

/// The shared AirPlay delay.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscCalibrationAirPlay {
    pub state: u32,
    pub has_old: bool,
    pub old_ms: f64,
    pub new_ms: f64,
    pub spread_ms: f64,
    pub certain_rows: usize,
    pub clamped: bool,
}

/// One change Apply writes; writing back `old_ms` (or removing the stored
/// value when `has_old` is false) undoes it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscCalibrationChange {
    pub row: usize,
    pub airplay: bool,
    pub has_old: bool,
    pub old_ms: f64,
    pub new_ms: f64,
}

fn end_code(reason: Option<EndReason>) -> u32 {
    match reason {
        None => CALIBRATION_END_NONE,
        Some(EndReason::Cancelled) => CALIBRATION_END_CANCELLED,
        Some(EndReason::SpeakersChanged) => CALIBRATION_END_SPEAKERS_CHANGED,
        Some(EndReason::SpeakerTurnedOff) => CALIBRATION_END_SPEAKER_OFF,
        Some(EndReason::SourceChanged) => CALIBRATION_END_SOURCE_CHANGED,
    }
}

fn end_reason(code: u32) -> EndReason {
    match code {
        CALIBRATION_END_SPEAKERS_CHANGED => EndReason::SpeakersChanged,
        CALIBRATION_END_SPEAKER_OFF => EndReason::SpeakerTurnedOff,
        CALIBRATION_END_SOURCE_CHANGED => EndReason::SourceChanged,
        _ => EndReason::Cancelled,
    }
}

fn stored(value: Option<f64>) -> (bool, f64) {
    (value.is_some(), value.unwrap_or(0.0))
}

/// [`calibrate_all::cast_wait_ms`] (S241): a Cast speaker's expected
/// arrival and play-out. No stored offset when `has_offset` is false or the
/// offset is 0 or less; `format` is the session's [`CAST_FORMAT_WAV`] or
/// [`CAST_FORMAT_FLAC`], any other value while none is reported.
#[unsafe(no_mangle)]
pub extern "C" fn asc_calibration_cast_wait_ms(
    has_offset: bool,
    offset_ms: f64,
    hold_back_ms: f64,
    format: i32,
) -> f64 {
    let codec = match format {
        CAST_FORMAT_WAV => Some(Codec::Wav),
        CAST_FORMAT_FLAC => Some(Codec::Flac),
        _ => None,
    };
    calibrate_all::cast_wait_ms(has_offset.then_some(offset_ms), hold_back_ms, codec)
}

/// Starts a run over `count` speakers, `reference` the index of the
/// reference, a local output: null for an unsupported rate, a reference
/// that isn't a local output among them, only the reference, an expected
/// arrival whose recording wouldn't fit 2^21 frames at `rate`, an unknown
/// kind or a null array with a nonzero count.
///
/// # Safety
///
/// `speakers` is null or holds `count` aligned speakers that stay unchanged
/// during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_run_create(
    rate: u32,
    speakers: *const AscCalibrationSpeaker,
    count: usize,
    reference: usize,
) -> *mut AscCalibrationRun {
    // SAFETY: by the caller's contract null or `count` speakers nothing
    // writes during the call.
    let Some(speakers) = (unsafe { host_slice(speakers, count) }) else {
        return ptr::null_mut();
    };
    let mut given = Vec::with_capacity(speakers.len());
    for (index, speaker) in speakers.iter().enumerate() {
        let class = match speaker.kind {
            CALIBRATION_KIND_LOCAL => Class::Local,
            CALIBRATION_KIND_CAST => Class::Cast,
            CALIBRATION_KIND_AIRPLAY => Class::AirPlay,
            _ => return ptr::null_mut(),
        };
        given.push(Speaker {
            id: index.to_string(),
            class,
            offset_ms: speaker.has_offset.then_some(speaker.offset_ms),
            latency_ms: speaker.latency_ms,
        });
    }
    match Run::new(rate, &reference.to_string(), given) {
        Ok(run) => Box::into_raw(Box::new(AscCalibrationRun { run })),
        Err(_) => ptr::null_mut(),
    }
}

/// The next step into `*step`: `sweep` 0 for the reference's, else 1 or 2.
/// False, writing nothing, once the run is over or for a null argument.
///
/// # Safety
///
/// `run` is null or a live handle; `step` is null or points to an aligned,
/// writable step nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_run_step(
    run: *const AscCalibrationRun,
    step: *mut AscCalibrationStep,
) -> bool {
    if run.is_null() || step.is_null() {
        return false;
    }
    // SAFETY: a live handle, by the caller's contract.
    let Some(next) = (unsafe { &(*run).run }).step() else {
        return false;
    };
    let sweep = match next.take {
        Take::Reference => 0,
        Take::Sweep(n) => n as u32,
    };
    // SAFETY: non-null, and by the caller's contract aligned and writable.
    unsafe {
        step.write(AscCalibrationStep {
            row: next.row,
            sweep,
            window_frames: next.window_frames,
            searching: next.searching,
        })
    };
    true
}

/// The recording the last step asked for, mono at the run's rate. False
/// when no step is waiting or for a null argument.
///
/// # Safety
///
/// `run` is null or a live handle no other call uses now; `recording` is
/// null or holds `frames` aligned floats unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_run_record(
    run: *mut AscCalibrationRun,
    recording: *const f32,
    frames: usize,
) -> bool {
    if run.is_null() {
        return false;
    }
    // SAFETY: by the caller's contract null or `frames` floats nothing
    // writes during the call.
    let Some(recording) = (unsafe { host_slice(recording, frames) }) else {
        return false;
    };
    // SAFETY: a live handle only this call uses.
    unsafe { &mut (*run).run }.record(recording)
}

/// Ends the run with an ASC_CALIBRATION_END_ reason (an unknown one counts
/// as cancelled): no more steps, and its sheet applies nothing.
///
/// # Safety
///
/// `run` is null or a live handle no other call uses now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_run_cancel(run: *mut AscCalibrationRun, reason: u32) {
    if run.is_null() {
        return;
    }
    // SAFETY: a live handle only this call uses.
    unsafe { &mut (*run).run }.cancel(end_reason(reason));
}

/// Turns the run into its sheet and frees the run; a run with steps still
/// waiting ends as cancelled. Null for a null run.
///
/// # Safety
///
/// `run` is null or a live handle no other call uses now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_run_finish(
    run: *mut AscCalibrationRun,
) -> *mut AscCalibrationSheet {
    if run.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: a handle from Box::into_raw that nothing uses any more.
    let run = unsafe { Box::from_raw(run) }.run;
    Box::into_raw(Box::new(AscCalibrationSheet {
        sheet: run.finish(),
    }))
}

/// Frees a run without a sheet.
///
/// # Safety
///
/// `run` is null or a live handle no other call uses now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_run_destroy(run: *mut AscCalibrationRun) {
    if !run.is_null() {
        // SAFETY: a handle from Box::into_raw that nothing uses any more.
        drop(unsafe { Box::from_raw(run) });
    }
}

/// The number of rows, one per speaker; 0 for a null sheet.
///
/// # Safety
///
/// `sheet` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_sheet_rows(sheet: *const AscCalibrationSheet) -> usize {
    if sheet.is_null() {
        return 0;
    }
    // SAFETY: a live handle, by the caller's contract.
    unsafe { &(*sheet).sheet }.rows().len()
}

fn row_of(row: &crate::calibrate_all::Row) -> AscCalibrationRow {
    let (has_old, old_ms) = stored(row.old_ms);
    let measured = |state: u32, m: Measurement| AscCalibrationRow {
        state,
        has_old,
        old_ms,
        new_ms: m.new_ms,
        uncertainty_ms: m.uncertainty_ms,
        difference_ms: m.difference_ms,
        clamped: m.clamped,
        ..AscCalibrationRow::default()
    };
    let plain = |state: u32| AscCalibrationRow {
        state,
        has_old,
        old_ms,
        ..AscCalibrationRow::default()
    };
    match row.state {
        RowState::Reference => plain(CALIBRATION_ROW_REFERENCE),
        RowState::NotMeasured => plain(CALIBRATION_ROW_NOT_MEASURED),
        RowState::Certain(m) => measured(CALIBRATION_ROW_CERTAIN, m),
        RowState::Uncertain(m) => measured(CALIBRATION_ROW_UNCERTAIN, m),
        RowState::Refused(refused) => AscCalibrationRow {
            reason: refusal_code(refused.reason),
            recording: match refused.recording {
                None => CALIBRATION_RECORDING_NONE,
                Some(Recording::Reference) => CALIBRATION_RECORDING_REFERENCE,
                Some(Recording::Chosen) => CALIBRATION_RECORDING_CHOSEN,
            },
            ..plain(CALIBRATION_ROW_REFUSED)
        },
    }
}

/// Row `index` into `*row`; false, writing nothing, for a row out of range
/// or a null argument.
///
/// # Safety
///
/// `sheet` is null or a live handle; `row` is null or points to an aligned,
/// writable row nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_sheet_row(
    sheet: *const AscCalibrationSheet,
    index: usize,
    row: *mut AscCalibrationRow,
) -> bool {
    if sheet.is_null() || row.is_null() {
        return false;
    }
    // SAFETY: a live handle, by the caller's contract.
    let Some(found) = unsafe { &(*sheet).sheet }.rows().get(index) else {
        return false;
    };
    // SAFETY: non-null, and by the caller's contract aligned and writable.
    unsafe { row.write(row_of(found)) };
    true
}

/// The shared AirPlay delay into `*airplay`; false, writing nothing, for a
/// sheet without AirPlay speakers or a null argument.
///
/// # Safety
///
/// `sheet` is null or a live handle; `airplay` is null or points to an
/// aligned, writable value nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_sheet_airplay(
    sheet: *const AscCalibrationSheet,
    airplay: *mut AscCalibrationAirPlay,
) -> bool {
    if sheet.is_null() || airplay.is_null() {
        return false;
    }
    // SAFETY: a live handle, by the caller's contract.
    let Some(shared) = unsafe { &(*sheet).sheet }.airplay().copied() else {
        return false;
    };
    let (has_old, old_ms) = stored(shared.old_ms);
    let state = match shared.state {
        SharedState::NoneCertain => CALIBRATION_AIRPLAY_NONE_CERTAIN,
        SharedState::Certain => CALIBRATION_AIRPLAY_CERTAIN,
        SharedState::Spread => CALIBRATION_AIRPLAY_SPREAD,
    };
    // SAFETY: non-null, and by the caller's contract aligned and writable.
    unsafe {
        airplay.write(AscCalibrationAirPlay {
            state,
            has_old,
            old_ms,
            new_ms: shared.new_ms.unwrap_or(old_ms),
            spread_ms: shared.spread_ms,
            certain_rows: shared.certain_rows,
            clamped: shared.clamped,
        })
    };
    true
}

/// Why the sheet ended, an ASC_CALIBRATION_END_ code; END_NONE while it can
/// be applied and for a null sheet.
///
/// # Safety
///
/// `sheet` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_sheet_ended(sheet: *const AscCalibrationSheet) -> u32 {
    if sheet.is_null() {
        return CALIBRATION_END_NONE;
    }
    // SAFETY: a live handle, by the caller's contract.
    end_code(unsafe { &(*sheet).sheet }.ended())
}

fn apply_code(refusal: ApplyRefusal) -> u32 {
    match refusal {
        ApplyRefusal::Ended(_) => CALIBRATION_APPLY_ENDED,
        ApplyRefusal::NothingCertain => CALIBRATION_APPLY_NOTHING_CERTAIN,
        ApplyRefusal::AlreadyApplied => CALIBRATION_APPLY_ALREADY_APPLIED,
    }
}

/// The changes Apply writes into `changes`, their number into `*count`,
/// with an ASC_CALIBRATION_APPLY_ code. Only APPLY_OK writes changes, and
/// spends the sheet: a second Apply and Measure Again are refused.
/// APPLY_TOO_MANY sets `*count` to the number needed, which is never more
/// than the rows, and leaves the sheet as it was.
///
/// # Safety
///
/// `sheet` is null or a live handle no other call uses now; `changes` is
/// null or holds `capacity` aligned, writable changes and `count` is null or
/// points to an aligned, writable size, none of which anything else touches
/// during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_sheet_apply(
    sheet: *mut AscCalibrationSheet,
    changes: *mut AscCalibrationChange,
    capacity: usize,
    count: *mut usize,
) -> u32 {
    if sheet.is_null() || count.is_null() || (changes.is_null() && capacity > 0) {
        return CALIBRATION_APPLY_INVALID;
    }
    // SAFETY: a live handle only this call uses.
    let sheet = unsafe { &mut (*sheet).sheet };
    let needed = match sheet.changes() {
        Ok(applied) => applied.changes.len(),
        Err(refusal) => return apply_code(refusal),
    };
    // SAFETY: non-null, and by the caller's contract aligned and writable.
    unsafe { count.write(needed) };
    if needed > capacity {
        return CALIBRATION_APPLY_TOO_MANY;
    }
    let applied = match sheet.apply() {
        Ok(applied) => applied,
        Err(refusal) => return apply_code(refusal),
    };
    for (slot, change) in applied.changes.iter().enumerate() {
        let (has_old, old_ms) = stored(change.old_ms);
        let (row, airplay) = match &change.target {
            Target::Speaker(id) => (
                sheet
                    .rows()
                    .iter()
                    .position(|row| &row.id == id)
                    .unwrap_or(0),
                false,
            ),
            Target::AirPlay => (0, true),
        };
        // SAFETY: `slot` < `capacity`, an aligned, writable change by the
        // caller's contract.
        unsafe {
            changes.add(slot).write(AscCalibrationChange {
                row,
                airplay,
                has_old,
                old_ms,
                new_ms: change.new_ms,
            })
        };
    }
    CALIBRATION_APPLY_OK
}

/// A run of the reference and row `index` alone, for Measure Again; its
/// steps name the sheet's rows. Null for the reference's row, a row out of
/// range, an ended, applied or null sheet. Its sheet goes back through
/// [`asc_calibration_sheet_replace`].
///
/// # Safety
///
/// `sheet` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_sheet_measure_again(
    sheet: *const AscCalibrationSheet,
    index: usize,
) -> *mut AscCalibrationRun {
    if sheet.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: a live handle, by the caller's contract.
    match unsafe { &(*sheet).sheet }.measure_again(index) {
        Some(run) => Box::into_raw(Box::new(AscCalibrationRun { run })),
        None => ptr::null_mut(),
    }
}

/// Puts the row a Measure Again run's sheet holds in place of the old one,
/// and frees `again`. False, changing nothing in `sheet`, for an `again`
/// that was cancelled or ended by a change, one that doesn't come from this
/// sheet's Measure Again, or a null argument; `again` is freed either way.
///
/// # Safety
///
/// `sheet` is null or a live handle no other call uses now; `again` is
/// null or a live handle no other call uses now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_sheet_replace(
    sheet: *mut AscCalibrationSheet,
    again: *mut AscCalibrationSheet,
) -> bool {
    if again.is_null() {
        return false;
    }
    // SAFETY: a handle from Box::into_raw that nothing uses any more.
    let again = unsafe { Box::from_raw(again) }.sheet;
    if sheet.is_null() {
        return false;
    }
    // SAFETY: a live handle only this call uses.
    unsafe { &mut (*sheet).sheet }.replace(again)
}

/// Frees a sheet.
///
/// # Safety
///
/// `sheet` is null or a live handle no other call uses now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_sheet_destroy(sheet: *mut AscCalibrationSheet) {
    if !sheet.is_null() {
        // SAFETY: a handle from Box::into_raw that nothing uses any more.
        drop(unsafe { Box::from_raw(sheet) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibrate;
    use crate::ffi::{CALIBRATION_RECORDING_REFERENCE, CALIBRATION_TOO_QUIET};

    const RATE: u32 = 48_000;

    #[test]
    fn the_cast_wait_reads_the_format_code_and_a_missing_offset() {
        assert_eq!(
            asc_calibration_cast_wait_ms(true, 1000.0, 750.0, CAST_FORMAT_WAV),
            2250.0
        );
        assert_eq!(
            asc_calibration_cast_wait_ms(false, 1000.0, 750.0, CAST_FORMAT_WAV),
            3750.0
        );
        assert_eq!(
            asc_calibration_cast_wait_ms(true, 0.0, 0.0, CAST_FORMAT_FLAC),
            6000.0
        );
        assert_eq!(asc_calibration_cast_wait_ms(false, 0.0, 0.0, 0), 6000.0);
    }

    fn heard_at(frames: usize, at_ms: f64) -> Vec<f32> {
        let signal = calibrate::signal(RATE).unwrap();
        let mut recording = vec![0.0; frames];
        let start = (at_ms * 48.0) as usize;
        for (slot, sample) in recording[start..].iter_mut().zip(&signal) {
            *slot = sample * 2.0;
        }
        recording
    }

    /// Plays every step: the reference's sweep heard at 50 ms, row `n`'s at
    /// `at_ms[n]`; every reference step must name row `reference`.
    unsafe fn drive(run: *mut AscCalibrationRun, reference: usize, at_ms: &[f64]) {
        let mut step = AscCalibrationStep::default();
        // SAFETY: a live run and a local step.
        while unsafe { asc_calibration_run_step(run, &mut step) } {
            let at = if step.sweep == 0 {
                assert_eq!(step.row, reference);
                50.0
            } else {
                at_ms[step.row]
            };
            let recording = heard_at(step.window_frames, at);
            // SAFETY: a live run and a live buffer.
            assert!(unsafe {
                asc_calibration_run_record(run, recording.as_ptr(), recording.len())
            });
        }
    }

    fn row(sheet: *const AscCalibrationSheet, index: usize) -> AscCalibrationRow {
        let mut row = AscCalibrationRow::default();
        // SAFETY: a live sheet and a local row.
        assert!(unsafe { asc_calibration_sheet_row(sheet, index, &mut row) });
        row
    }

    fn apply(
        sheet: *mut AscCalibrationSheet,
        changes: &mut [AscCalibrationChange],
    ) -> (u32, usize) {
        let mut count = 99;
        // SAFETY: a live sheet, a live array and a local count.
        let status = unsafe {
            asc_calibration_sheet_apply(sheet, changes.as_mut_ptr(), changes.len(), &mut count)
        };
        (status, count)
    }

    // covers: S235, S236, S237, S238, S239
    #[test]
    fn a_run_its_sheet_apply_and_measure_again_cross_the_boundary() {
        let local = |offset: Option<f64>, latency_ms: f64| AscCalibrationSpeaker {
            kind: CALIBRATION_KIND_LOCAL,
            has_offset: offset.is_some(),
            offset_ms: offset.unwrap_or(0.0),
            latency_ms,
        };
        let speakers = [
            local(Some(10.0), 200.0),
            local(None, 50.0),
            AscCalibrationSpeaker {
                kind: CALIBRATION_KIND_AIRPLAY,
                has_offset: true,
                offset_ms: 2000.0,
                latency_ms: 2000.0,
            },
            local(None, 50.0),
        ];
        // SAFETY: a live array of four speakers; the reference is row 1.
        let run = unsafe { asc_calibration_run_create(RATE, speakers.as_ptr(), 4, 1) };
        assert!(!run.is_null());
        // SAFETY: a live run, then freed by finish.
        let sheet = unsafe {
            drive(run, 1, &[250.0, 0.0, 150.0, 80.0]);
            asc_calibration_run_finish(run)
        };
        // SAFETY: a live sheet.
        assert_eq!(unsafe { asc_calibration_sheet_rows(sheet) }, 4);
        // SAFETY: a live sheet.
        let ended = unsafe { asc_calibration_sheet_ended(sheet) };
        assert_eq!(ended, CALIBRATION_END_NONE);
        assert_eq!(row(sheet, 1).state, CALIBRATION_ROW_REFERENCE);
        let bt = row(sheet, 0);
        assert_eq!(bt.state, CALIBRATION_ROW_CERTAIN);
        assert_eq!((bt.has_old, bt.old_ms), (true, 10.0));
        assert_eq!(bt.new_ms, -190.0);
        assert_eq!(row(sheet, 3).new_ms, -30.0);
        let mut airplay = AscCalibrationAirPlay::default();
        // SAFETY: a live sheet and a local value.
        assert!(unsafe { asc_calibration_sheet_airplay(sheet, &mut airplay) });
        assert_eq!(airplay.state, CALIBRATION_AIRPLAY_CERTAIN);
        assert_eq!(airplay.new_ms, 2100.0);

        // SAFETY: a live sheet.
        assert!(unsafe { asc_calibration_sheet_measure_again(sheet, 1) }.is_null());
        // SAFETY: a live sheet.
        let again = unsafe { asc_calibration_sheet_measure_again(sheet, 3) };
        assert!(!again.is_null());
        // SAFETY: a live run, then freed by finish; the sheets are live and
        // replace frees the second.
        let replaced = unsafe {
            drive(again, 1, &[0.0, 0.0, 0.0, 40.0]);
            let again = asc_calibration_run_finish(again);
            asc_calibration_sheet_replace(sheet, again)
        };
        assert!(replaced);
        assert_eq!(row(sheet, 3).new_ms, 10.0);

        let before = row(sheet, 0);
        // SAFETY: a live sheet.
        let again = unsafe { asc_calibration_sheet_measure_again(sheet, 0) };
        // SAFETY: live handles; finish and replace free `again`.
        unsafe {
            asc_calibration_run_cancel(again, CALIBRATION_END_SOURCE_CHANGED);
            assert!(!asc_calibration_sheet_replace(
                sheet,
                asc_calibration_run_finish(again)
            ));
        }
        assert_eq!(row(sheet, 0), before);
        // SAFETY: a live sheet.
        let ended = unsafe { asc_calibration_sheet_ended(sheet) };
        assert_eq!(ended, CALIBRATION_END_NONE);

        let mut changes = [AscCalibrationChange::default(); 4];
        assert_eq!(
            apply(sheet, &mut changes[..1]),
            (CALIBRATION_APPLY_TOO_MANY, 3)
        );
        assert_eq!(apply(sheet, &mut changes), (CALIBRATION_APPLY_OK, 3));
        let change = |row, airplay, has_old, old_ms, new_ms| AscCalibrationChange {
            row,
            airplay,
            has_old,
            old_ms,
            new_ms,
        };
        assert_eq!(
            changes[..3],
            [
                change(0, false, true, 10.0, -190.0),
                change(3, false, false, 0.0, 10.0),
                change(0, true, true, 2000.0, 2100.0),
            ]
        );
        assert_eq!(
            apply(sheet, &mut changes),
            (CALIBRATION_APPLY_ALREADY_APPLIED, 99)
        );
        // SAFETY: a live sheet.
        assert!(unsafe { asc_calibration_sheet_measure_again(sheet, 0) }.is_null());
        // SAFETY: a live sheet nothing uses afterwards.
        unsafe { asc_calibration_sheet_destroy(sheet) };
    }

    // covers: S236
    #[test]
    fn a_refused_reference_and_bad_arguments() {
        let speakers = [
            AscCalibrationSpeaker {
                kind: CALIBRATION_KIND_LOCAL,
                ..AscCalibrationSpeaker::default()
            },
            AscCalibrationSpeaker {
                kind: CALIBRATION_KIND_CAST,
                ..AscCalibrationSpeaker::default()
            },
        ];
        // SAFETY: a live array of two speakers.
        let run = unsafe { asc_calibration_run_create(RATE, speakers.as_ptr(), 2, 0) };
        let mut step = AscCalibrationStep::default();
        // SAFETY: a live run and a local step.
        assert!(unsafe { asc_calibration_run_step(run, &mut step) });
        assert_eq!((step.row, step.sweep), (0, 0));
        let silence = vec![0.0; step.window_frames];
        // SAFETY: a live run and a live buffer.
        assert!(unsafe { asc_calibration_run_record(run, silence.as_ptr(), silence.len()) });
        // SAFETY: a live run and a local step.
        assert!(!unsafe { asc_calibration_run_step(run, &mut step) });
        // SAFETY: a live run, freed by finish.
        let sheet = unsafe { asc_calibration_run_finish(run) };
        let refused = row(sheet, 1);
        assert_eq!(refused.state, CALIBRATION_ROW_REFUSED);
        assert_eq!(refused.reason, CALIBRATION_TOO_QUIET);
        assert_eq!(refused.recording, CALIBRATION_RECORDING_REFERENCE);
        let mut count = 9;
        // SAFETY: a live sheet, no room and a local count.
        let status = unsafe { asc_calibration_sheet_apply(sheet, ptr::null_mut(), 0, &mut count) };
        assert_eq!(status, CALIBRATION_APPLY_NOTHING_CERTAIN);
        // SAFETY: a live sheet and a null out pointer.
        assert!(!unsafe { asc_calibration_sheet_airplay(sheet, ptr::null_mut()) });
        // SAFETY: a live sheet, freed once.
        unsafe { asc_calibration_sheet_destroy(sheet) };

        let mut bad = speakers;
        bad[1].kind = 9;
        // SAFETY: a live array of two speakers.
        assert!(unsafe { asc_calibration_run_create(RATE, bad.as_ptr(), 2, 0) }.is_null());
        // SAFETY: the reference is the Cast speaker, which the call refuses.
        assert!(unsafe { asc_calibration_run_create(RATE, speakers.as_ptr(), 2, 1) }.is_null());
        // SAFETY: a null array with a nonzero count, which the call refuses.
        assert!(unsafe { asc_calibration_run_create(RATE, ptr::null(), 2, 0) }.is_null());
        // SAFETY: null handles, which every call ignores.
        unsafe {
            assert!(!asc_calibration_run_record(ptr::null_mut(), ptr::null(), 0));
            asc_calibration_run_cancel(ptr::null_mut(), 1);
            assert!(asc_calibration_run_finish(ptr::null_mut()).is_null());
            asc_calibration_run_destroy(ptr::null_mut());
            assert_eq!(asc_calibration_sheet_rows(ptr::null()), 0);
            assert!(!asc_calibration_sheet_replace(
                ptr::null_mut(),
                ptr::null_mut()
            ));
            asc_calibration_sheet_destroy(ptr::null_mut());
        }
    }

    fn speaker(kind: u32, offset: Option<f64>, latency_ms: f64) -> AscCalibrationSpeaker {
        AscCalibrationSpeaker {
            kind,
            has_offset: offset.is_some(),
            offset_ms: offset.unwrap_or(0.0),
            latency_ms,
        }
    }

    // covers: S235, S237, S238, S239
    #[test]
    fn a_searching_cast_speaker_and_missing_values_cross_the_boundary() {
        let speakers = [
            speaker(CALIBRATION_KIND_LOCAL, None, 50.0),
            speaker(CALIBRATION_KIND_CAST, None, 0.0),
            speaker(CALIBRATION_KIND_AIRPLAY, None, 2000.0),
        ];
        // SAFETY: a null array with no speakers, then live arrays of one and
        // of three speakers.
        unsafe {
            assert!(asc_calibration_run_create(RATE, ptr::null(), 0, 0).is_null());
            assert!(asc_calibration_run_create(RATE, speakers.as_ptr(), 1, 0).is_null());
            assert!(asc_calibration_run_create(RATE, speakers.as_ptr(), 3, 3).is_null());
            assert!(asc_calibration_run_create(8000, speakers.as_ptr(), 3, 0).is_null());
        }
        // SAFETY: a live array of three speakers.
        let run = unsafe { asc_calibration_run_create(RATE, speakers.as_ptr(), 3, 0) };
        assert!(!run.is_null());
        let mut step = AscCalibrationStep::default();
        // SAFETY: a live run, null pointers the calls refuse, a local step.
        unsafe {
            assert!(!asc_calibration_run_step(run, ptr::null_mut()));
            assert!(!asc_calibration_run_step(ptr::null(), &mut step));
            assert_eq!(step, AscCalibrationStep::default());
            assert!(asc_calibration_run_step(run, &mut step));
        }
        let reference = heard_at(step.window_frames, 50.0);
        let mut waiting = AscCalibrationStep::default();
        // SAFETY: a live run, a null recording the call refuses, a live
        // buffer and local steps.
        unsafe {
            assert!(!asc_calibration_run_record(
                run,
                ptr::null(),
                reference.len()
            ));
            assert!(asc_calibration_run_step(run, &mut waiting));
            assert_eq!(waiting, step, "a null recording leaves the step waiting");
            assert!(asc_calibration_run_record(
                run,
                reference.as_ptr(),
                reference.len()
            ));
            assert!(asc_calibration_run_step(run, &mut step));
        }
        assert_eq!((step.row, step.sweep, step.searching), (1, 1, true));
        assert_eq!(step.window_frames, calibrate::MAX_RECORDING_FRAMES);
        // SAFETY: a live run, then freed by finish.
        let sheet = unsafe {
            drive(run, 0, &[0.0, 650.0, 150.0]);
            asc_calibration_run_finish(run)
        };

        let tv = row(sheet, 1);
        assert_eq!(tv.state, CALIBRATION_ROW_CERTAIN);
        assert_eq!((tv.has_old, tv.new_ms, tv.clamped), (false, 600.0, false));
        let mut airplay = AscCalibrationAirPlay::default();
        // SAFETY: a live sheet and a local value.
        assert!(unsafe { asc_calibration_sheet_airplay(sheet, &mut airplay) });
        assert_eq!(
            (airplay.has_old, airplay.new_ms, airplay.clamped),
            (false, 2100.0, false)
        );
        assert_eq!(airplay.certain_rows, 1);

        let mut untouched = AscCalibrationRow::default();
        // SAFETY: a live sheet, null pointers the calls refuse, a local row.
        unsafe {
            assert!(!asc_calibration_sheet_row(sheet, 3, &mut untouched));
            assert!(!asc_calibration_sheet_row(sheet, 0, ptr::null_mut()));
            assert!(!asc_calibration_sheet_row(ptr::null(), 0, &mut untouched));
            assert!(!asc_calibration_sheet_airplay(ptr::null(), &mut airplay));
            assert!(asc_calibration_sheet_measure_again(ptr::null(), 1).is_null());
            assert!(asc_calibration_sheet_measure_again(sheet, 3).is_null());
            assert_eq!(
                asc_calibration_sheet_ended(ptr::null()),
                CALIBRATION_END_NONE
            );
        }
        assert_eq!(untouched, AscCalibrationRow::default());

        let mut changes = [AscCalibrationChange::default(); 3];
        let mut count = 7;
        // SAFETY: null pointers the call refuses, a live sheet, a live array
        // and a local count.
        unsafe {
            let invalid = [
                asc_calibration_sheet_apply(ptr::null_mut(), changes.as_mut_ptr(), 3, &mut count),
                asc_calibration_sheet_apply(sheet, changes.as_mut_ptr(), 3, ptr::null_mut()),
                asc_calibration_sheet_apply(sheet, ptr::null_mut(), 3, &mut count),
            ];
            assert_eq!(invalid, [CALIBRATION_APPLY_INVALID; 3]);
        }
        assert_eq!(count, 7);
        assert_eq!(apply(sheet, &mut changes), (CALIBRATION_APPLY_OK, 2));
        let change = |row, airplay, new_ms| AscCalibrationChange {
            row,
            airplay,
            has_old: false,
            old_ms: 0.0,
            new_ms,
        };
        assert_eq!(
            changes[..2],
            [change(1, false, 600.0), change(0, true, 2100.0)]
        );

        // SAFETY: a live array of three speakers; the run is freed by finish
        // and its sheet by replace, which refuses the null sheet.
        unsafe {
            let other = asc_calibration_run_create(RATE, speakers.as_ptr(), 3, 0);
            let other = asc_calibration_run_finish(other);
            assert!(!asc_calibration_sheet_replace(ptr::null_mut(), other));
            asc_calibration_sheet_destroy(sheet);
        }
    }

    // covers: S240
    #[test]
    fn every_end_code_crosses_the_boundary_and_an_ended_sheet_is_refused() {
        let speakers = [
            speaker(CALIBRATION_KIND_LOCAL, None, 50.0),
            speaker(CALIBRATION_KIND_CAST, Some(100.0), 300.0),
        ];
        for (code, ended) in [
            (CALIBRATION_END_CANCELLED, CALIBRATION_END_CANCELLED),
            (
                CALIBRATION_END_SPEAKERS_CHANGED,
                CALIBRATION_END_SPEAKERS_CHANGED,
            ),
            (CALIBRATION_END_SPEAKER_OFF, CALIBRATION_END_SPEAKER_OFF),
            (
                CALIBRATION_END_SOURCE_CHANGED,
                CALIBRATION_END_SOURCE_CHANGED,
            ),
            (CALIBRATION_END_NONE, CALIBRATION_END_CANCELLED),
            (77, CALIBRATION_END_CANCELLED),
        ] {
            // SAFETY: a live array of two speakers; the run is cancelled,
            // then freed by finish.
            let sheet = unsafe {
                let run = asc_calibration_run_create(RATE, speakers.as_ptr(), 2, 0);
                asc_calibration_run_cancel(run, code);
                let mut step = AscCalibrationStep::default();
                assert!(!asc_calibration_run_step(run, &mut step));
                asc_calibration_run_finish(run)
            };
            // SAFETY: a live sheet.
            assert_eq!(unsafe { asc_calibration_sheet_ended(sheet) }, ended);
            assert_eq!(row(sheet, 1).state, CALIBRATION_ROW_NOT_MEASURED);
            assert_eq!((row(sheet, 1).has_old, row(sheet, 1).old_ms), (true, 100.0));
            let mut changes = [AscCalibrationChange::default(); 2];
            assert_eq!(apply(sheet, &mut changes), (CALIBRATION_APPLY_ENDED, 99));
            let mut airplay = AscCalibrationAirPlay::default();
            // SAFETY: a live sheet nothing uses afterwards, a local value.
            unsafe {
                assert!(!asc_calibration_sheet_airplay(sheet, &mut airplay));
                assert!(asc_calibration_sheet_measure_again(sheet, 1).is_null());
                asc_calibration_sheet_destroy(sheet);
            }
        }
        // SAFETY: a live array of two speakers; the run is freed once.
        unsafe {
            let run = asc_calibration_run_create(RATE, speakers.as_ptr(), 2, 0);
            assert!(!run.is_null());
            asc_calibration_run_destroy(run);
        }
    }
}
