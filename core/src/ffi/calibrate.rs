//! The microphone calibration through the C interface (S232 to S234): the
//! test signal and the estimate of one output's offset.
//!
//! The rules are in [`calibrate`]; this module only reads the host's arrays.
//! For the control thread: both calls allocate, and the estimate takes tens
//! of ms for recordings of a few seconds.

use super::{host_slice, host_slice_mut};
use crate::calibrate::{self, Calibration, NotMeasured, Recording, Refusal, Refused, SWEEPS};
use crate::policy::delay;

/// The estimate gave a new offset.
pub const CALIBRATION_ACCEPTED: u32 = 0;
/// [`Refusal::TooShort`].
pub const CALIBRATION_TOO_SHORT: u32 = 1;
/// [`Refusal::TooLong`].
pub const CALIBRATION_TOO_LONG: u32 = 2;
/// [`Refusal::Clipped`].
pub const CALIBRATION_CLIPPED: u32 = 3;
/// [`Refusal::TooQuiet`].
pub const CALIBRATION_TOO_QUIET: u32 = 4;
/// [`Refusal::AmbiguousEcho`].
pub const CALIBRATION_AMBIGUOUS_ECHO: u32 = 5;
/// [`Refusal::Implausible`].
pub const CALIBRATION_IMPLAUSIBLE: u32 = 6;
/// [`NotMeasured::Disagree`], from [`asc_calibration_estimate_agreed`] only.
pub const CALIBRATION_DISAGREE: u32 = 7;
/// The runs [`asc_calibration_estimate_agreed`] takes.
pub const CALIBRATION_SWEEPS: usize = SWEEPS;

/// A result about both recordings, or about neither.
pub const CALIBRATION_RECORDING_NONE: u32 = 0;
/// [`Recording::Reference`].
pub const CALIBRATION_RECORDING_REFERENCE: u32 = 1;
/// [`Recording::Chosen`].
pub const CALIBRATION_RECORDING_CHOSEN: u32 = 2;

/// What [`asc_calibration_estimate`] found (S232, S234).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscCalibration {
    pub status: u32,
    pub recording: u32,
    pub offset_ms: f64,
    pub difference_ms: f64,
    pub reference_ms: f64,
    pub chosen_ms: f64,
    pub clamped: bool,
}

/// [`calibrate::signal_frames`] (S233): 0 for a rate outside 22 050 to
/// 384 000 Hz.
#[unsafe(no_mangle)]
pub extern "C" fn asc_calibration_signal_frames(rate: u32) -> usize {
    calibrate::signal_frames(rate).unwrap_or(0)
}

/// [`calibrate::signal`] (S233) into `samples`: false, writing nothing, for
/// an unsupported rate, a `frames` other than
/// [`asc_calibration_signal_frames`] or a null `samples`.
///
/// # Safety
///
/// `samples` is null or holds `frames` aligned floats that nothing else
/// touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_signal(
    rate: u32,
    samples: *mut f32,
    frames: usize,
) -> bool {
    if calibrate::signal_frames(rate) != Some(frames) {
        return false;
    }
    // SAFETY: by the caller's contract null or `frames` floats only this call
    // touches.
    let Some(out) = (unsafe { host_slice_mut(samples, frames) }) else {
        return false;
    };
    let Some(signal) = calibrate::signal(rate) else {
        return false;
    };
    out.copy_from_slice(&signal);
    true
}

pub(super) fn refusal_code(reason: Refusal) -> u32 {
    match reason {
        Refusal::TooShort => CALIBRATION_TOO_SHORT,
        Refusal::TooLong => CALIBRATION_TOO_LONG,
        Refusal::Clipped => CALIBRATION_CLIPPED,
        Refusal::TooQuiet => CALIBRATION_TOO_QUIET,
        Refusal::AmbiguousEcho => CALIBRATION_AMBIGUOUS_ECHO,
        Refusal::Implausible => CALIBRATION_IMPLAUSIBLE,
    }
}

/// [`calibrate::estimate`] (S232, S234) into `result`. Accepted, the new
/// offset; refused, the reason, the recording it concerns and the old offset
/// as S175 reads it, with the three times 0. False, writing nothing, for a
/// null array with a nonzero count or a null `result`.
///
/// # Safety
///
/// Each array is null or holds its count of aligned floats that stay
/// unchanged during the call; `result` is null or points to an aligned,
/// writable [`AscCalibration`] that nothing else touches during the call.
// Three plain arrays with their counts keep the C side free of a struct of
// pointers the host would fill in for one call.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_estimate(
    signal: *const f32,
    signal_frames: usize,
    reference: *const f32,
    reference_frames: usize,
    chosen: *const f32,
    chosen_frames: usize,
    rate: u32,
    old_offset_ms: f64,
    result: *mut AscCalibration,
) -> bool {
    // SAFETY: by the caller's contract each array is null or holds its count
    // of floats that nothing writes during the call.
    let (Some(signal), Some(reference), Some(chosen)) = (unsafe {
        (
            host_slice(signal, signal_frames),
            host_slice(reference, reference_frames),
            host_slice(chosen, chosen_frames),
        )
    }) else {
        return false;
    };
    if result.is_null() {
        return false;
    }
    let found = match calibrate::estimate(signal, reference, chosen, rate, old_offset_ms) {
        Ok(found) => accepted(found),
        Err(refused) => refusal(refused, old_offset_ms),
    };
    // SAFETY: non-null, and by the caller's contract aligned, writable and
    // not touched by anyone else.
    unsafe { result.write(found) };
    true
}

fn accepted(found: Calibration) -> AscCalibration {
    AscCalibration {
        status: CALIBRATION_ACCEPTED,
        recording: CALIBRATION_RECORDING_NONE,
        offset_ms: found.offset_ms,
        difference_ms: found.difference_ms,
        reference_ms: found.reference_ms,
        chosen_ms: found.chosen_ms,
        clamped: found.clamped,
    }
}

fn refusal(refused: Refused, old_offset_ms: f64) -> AscCalibration {
    AscCalibration {
        status: refusal_code(refused.reason),
        recording: match refused.recording {
            None => CALIBRATION_RECORDING_NONE,
            Some(Recording::Reference) => CALIBRATION_RECORDING_REFERENCE,
            Some(Recording::Chosen) => CALIBRATION_RECORDING_CHOSEN,
        },
        offset_ms: delay::offset_ms(Some(old_offset_ms)),
        ..AscCalibration::default()
    }
}

/// [`calibrate::estimate_agreed`] (S232) of [`CALIBRATION_SWEEPS`] runs into
/// `result` as [`asc_calibration_estimate`] writes one, with
/// [`CALIBRATION_DISAGREE`] and the old offset on a disagreement, and into
/// `spread_ms` that disagreement's spread, else 0. False, writing nothing,
/// for a null array with a nonzero count, a null list or a null out-pointer.
///
/// # Safety
///
/// `signal` is null or holds `signal_frames` aligned floats; each list is
/// null or holds [`CALIBRATION_SWEEPS`] aligned entries, each recording null
/// or holding its count of aligned floats; none of these changes during the
/// call. `result` and `spread_ms` are null or point to an aligned, writable
/// value that nothing else touches during the call.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_estimate_agreed(
    signal: *const f32,
    signal_frames: usize,
    references: *const *const f32,
    reference_frames: *const usize,
    chosen: *const *const f32,
    chosen_frames: *const usize,
    rate: u32,
    old_offset_ms: f64,
    result: *mut AscCalibration,
    spread_ms: *mut f64,
) -> bool {
    // SAFETY: by the caller's contract the signal is null or holds its count
    // of floats, and each list is null or holds SWEEPS entries, unchanged
    // during the call.
    let (Some(signal), Some(references), Some(reference_frames), Some(chosen), Some(chosen_frames)) =
        (unsafe {
            (
                host_slice(signal, signal_frames),
                host_slice(references, SWEEPS),
                host_slice(reference_frames, SWEEPS),
                host_slice(chosen, SWEEPS),
                host_slice(chosen_frames, SWEEPS),
            )
        })
    else {
        return false;
    };
    let mut runs: [(&[f32], &[f32]); SWEEPS] = [(&[], &[]); SWEEPS];
    for (i, run) in runs.iter_mut().enumerate() {
        // SAFETY: by the caller's contract each recording is null or holds
        // its count of floats that nothing writes during the call.
        let (Some(reference), Some(chosen)) = (unsafe {
            (
                host_slice(references[i], reference_frames[i]),
                host_slice(chosen[i], chosen_frames[i]),
            )
        }) else {
            return false;
        };
        *run = (reference, chosen);
    }
    if result.is_null() || spread_ms.is_null() {
        return false;
    }
    let (found, spread) = match calibrate::estimate_agreed(signal, &runs, rate, old_offset_ms) {
        Ok(found) => (accepted(found), 0.0),
        Err(NotMeasured::Refused(refused)) => (refusal(refused, old_offset_ms), 0.0),
        Err(NotMeasured::Disagree { spread_ms }) => (
            AscCalibration {
                status: CALIBRATION_DISAGREE,
                offset_ms: delay::offset_ms(Some(old_offset_ms)),
                ..AscCalibration::default()
            },
            spread_ms,
        ),
    };
    // SAFETY: both non-null, and by the caller's contract aligned, writable
    // and not touched by anyone else.
    unsafe {
        result.write(found);
        spread_ms.write(spread);
    }
    true
}

/// What [`asc_calibration_measure`] found in one recording, for the debug
/// log: S234's arrival and how the peak stands out, before the thresholds
/// that would refuse it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscCalibrationArrival {
    /// [`CALIBRATION_ACCEPTED`] when measured, else the refusal's code.
    pub status: u32,
    pub frame: usize,
    pub peak_frame: usize,
    pub echo_frame: usize,
    pub peak_over_floor: f64,
    pub echo_share: f64,
}

/// [`calibrate::measure`] (S234) of one recording into `result`. False,
/// writing nothing, for a null array with a nonzero count or a null `result`.
///
/// # Safety
///
/// Each array is null or holds its count of aligned floats that stay
/// unchanged during the call; `result` is null or points to an aligned,
/// writable [`AscCalibrationArrival`] that nothing else touches during the
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_calibration_measure(
    signal: *const f32,
    signal_frames: usize,
    recording: *const f32,
    recording_frames: usize,
    rate: u32,
    result: *mut AscCalibrationArrival,
) -> bool {
    // SAFETY: by the caller's contract each array is null or holds its count
    // of floats that nothing writes during the call.
    let (Some(signal), Some(recording)) = (unsafe {
        (
            host_slice(signal, signal_frames),
            host_slice(recording, recording_frames),
        )
    }) else {
        return false;
    };
    if result.is_null() {
        return false;
    }
    let found = match calibrate::measure(signal, recording, rate) {
        Ok(found) => AscCalibrationArrival {
            status: CALIBRATION_ACCEPTED,
            frame: found.frame,
            peak_frame: found.peak_frame,
            echo_frame: found.echo_frame,
            peak_over_floor: found.peak_over_floor,
            echo_share: found.echo_share,
        },
        Err(reason) => AscCalibrationArrival {
            status: refusal_code(reason),
            ..AscCalibrationArrival::default()
        },
    };
    // SAFETY: non-null, and by the caller's contract aligned, writable and
    // not touched by anyone else.
    unsafe { result.write(found) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heard_at(rate: u32, frame: usize, gain: f32) -> Vec<f32> {
        let signal = calibrate::signal(rate).expect("a supported rate");
        let mut recording = vec![0.0; 2 * rate as usize];
        for (slot, sample) in recording[frame..].iter_mut().zip(&signal) {
            *slot = sample * gain;
        }
        recording
    }

    fn estimate(signal: &[f32], reference: &[f32], chosen: &[f32], old: f64) -> AscCalibration {
        let mut result = AscCalibration {
            status: 99,
            ..AscCalibration::default()
        };
        // SAFETY: live slices and a local result.
        let ok = unsafe {
            asc_calibration_estimate(
                signal.as_ptr(),
                signal.len(),
                reference.as_ptr(),
                reference.len(),
                chosen.as_ptr(),
                chosen.len(),
                48_000,
                old,
                &mut result,
            )
        };
        assert!(ok);
        result
    }

    fn measure(signal: &[f32], recording: &[f32], result: *mut AscCalibrationArrival) -> bool {
        // SAFETY: live slices, and a local result or null.
        unsafe {
            asc_calibration_measure(
                signal.as_ptr(),
                signal.len(),
                recording.as_ptr(),
                recording.len(),
                48_000,
                result,
            )
        }
    }

    // covers: S234
    #[test]
    fn a_recording_is_measured_across_the_boundary() {
        let signal = calibrate::signal(48_000).expect("48 kHz");
        let recording = heard_at(48_000, 4800, 0.5);
        let mut result = AscCalibrationArrival {
            status: 99,
            ..AscCalibrationArrival::default()
        };
        assert!(measure(&signal, &recording, &mut result));
        assert_eq!(result.status, CALIBRATION_ACCEPTED);
        assert_eq!((result.frame, result.peak_frame), (4800, 4800));
        assert!(
            result.peak_over_floor > 12.0 && result.echo_share < 0.1,
            "{result:?}"
        );
        assert!(measure(&signal, &recording[..100], &mut result));
        assert_eq!(result.status, CALIBRATION_TOO_SHORT);
        assert!(!measure(&signal, &recording, std::ptr::null_mut()));
    }

    // covers: S232, S233
    #[test]
    fn the_signal_and_the_estimate_cross_the_boundary() {
        let frames = asc_calibration_signal_frames(48_000);
        assert_eq!(frames, 36_000);
        assert_eq!(asc_calibration_signal_frames(8000), 0);
        let mut signal = vec![9.0f32; frames];
        // SAFETY: a live buffer of `frames` floats.
        assert!(unsafe { asc_calibration_signal(48_000, signal.as_mut_ptr(), frames) });
        assert_eq!(Some(signal.clone()), calibrate::signal(48_000));
        let mut short = vec![9.0f32; frames - 1];
        // SAFETY: a live buffer of `frames - 1` floats.
        assert!(!unsafe { asc_calibration_signal(48_000, short.as_mut_ptr(), frames - 1) });
        assert!(short.iter().all(|s| *s == 9.0), "nothing written");
        // SAFETY: a null buffer, which the call refuses.
        assert!(!unsafe { asc_calibration_signal(48_000, std::ptr::null_mut(), frames) });

        let reference = heard_at(48_000, 4800, 0.5);
        let chosen = heard_at(48_000, 4800 + 12_000, 0.5);
        let found = estimate(&signal, &reference, &chosen, 100.0);
        assert_eq!(
            found,
            AscCalibration {
                status: CALIBRATION_ACCEPTED,
                recording: CALIBRATION_RECORDING_NONE,
                offset_ms: -150.0,
                difference_ms: 250.0,
                reference_ms: 100.0,
                chosen_ms: 350.0,
                clamped: false,
            }
        );
        let clamped = estimate(&signal, &reference, &chosen, -1900.0);
        assert_eq!(clamped.status, CALIBRATION_ACCEPTED);
        assert_eq!(clamped.offset_ms, -2000.0);
        assert!(clamped.clamped);
        let silence = vec![0.0; chosen.len()];
        let refused = estimate(&signal, &reference, &silence, 30_000.0);
        assert_eq!(
            refused,
            AscCalibration {
                status: CALIBRATION_TOO_QUIET,
                recording: CALIBRATION_RECORDING_CHOSEN,
                offset_ms: 20_000.0,
                ..AscCalibration::default()
            }
        );
    }

    #[test]
    fn null_arguments_write_nothing() {
        let mut result = AscCalibration::default();
        // SAFETY: a null array with a nonzero count, which the call refuses.
        let ok = unsafe {
            asc_calibration_estimate(
                std::ptr::null(),
                10,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                48_000,
                0.0,
                &mut result,
            )
        };
        assert!(!ok);
        // SAFETY: empty arrays and a null result, which the call refuses.
        let ok = unsafe {
            asc_calibration_estimate(
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                48_000,
                0.0,
                std::ptr::null_mut(),
            )
        };
        assert!(!ok);
        // SAFETY: empty arrays and a local result.
        let ok = unsafe {
            asc_calibration_estimate(
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                48_000,
                0.0,
                &mut result,
            )
        };
        assert!(ok);
        assert_eq!(result.status, CALIBRATION_TOO_SHORT);
        assert_eq!(result.recording, CALIBRATION_RECORDING_REFERENCE);
    }

    fn estimate_agreed(
        signal: &[f32],
        reference: &[f32],
        chosen: [&[f32]; SWEEPS],
    ) -> Option<(AscCalibration, f64)> {
        let (mut result, mut spread) = (
            AscCalibration {
                status: 99,
                ..AscCalibration::default()
            },
            -1.0,
        );
        let references = [reference.as_ptr(); SWEEPS];
        let reference_frames = [reference.len(); SWEEPS];
        let chosen_frames = chosen.map(<[f32]>::len);
        let chosen = chosen.map(<[f32]>::as_ptr);
        // SAFETY: live slices, lists of SWEEPS entries and local results.
        let ok = unsafe {
            asc_calibration_estimate_agreed(
                signal.as_ptr(),
                signal.len(),
                references.as_ptr(),
                reference_frames.as_ptr(),
                chosen.as_ptr(),
                chosen_frames.as_ptr(),
                48_000,
                100.0,
                &mut result,
                &mut spread,
            )
        };
        ok.then_some((result, spread))
    }

    // covers: S232
    #[test]
    fn three_measurements_cross_the_boundary_agreed_or_not() {
        let signal = calibrate::signal(48_000).expect("48 kHz");
        let reference = heard_at(48_000, 4800, 0.5);
        let late = |ms: usize| heard_at(48_000, 4800 + 48 * ms, 0.5);
        let (at_250, at_251, at_252, at_260) = (late(250), late(251), late(252), late(260));

        let (agreed, spread) =
            estimate_agreed(&signal, &reference, [&at_252, &at_250, &at_251]).expect("valid");
        assert_eq!(
            (agreed.status, agreed.offset_ms, spread),
            (CALIBRATION_ACCEPTED, -151.0, 0.0)
        );
        assert_eq!(agreed.difference_ms, 251.0);

        let (apart, spread) =
            estimate_agreed(&signal, &reference, [&at_250, &at_250, &at_260]).expect("valid");
        assert_eq!(
            apart,
            AscCalibration {
                status: CALIBRATION_DISAGREE,
                offset_ms: 100.0,
                ..AscCalibration::default()
            }
        );
        assert_eq!(spread, 10.0);

        let silence = vec![0.0; at_250.len()];
        let (refused, spread) =
            estimate_agreed(&signal, &reference, [&at_250, &silence, &at_251]).expect("valid");
        assert_eq!(
            refused,
            AscCalibration {
                status: CALIBRATION_TOO_QUIET,
                recording: CALIBRATION_RECORDING_CHOSEN,
                offset_ms: 100.0,
                ..AscCalibration::default()
            }
        );
        assert_eq!(spread, 0.0);

        let mut result = AscCalibration::default();
        let mut spread = -1.0;
        let frames = [0; SWEEPS];
        // SAFETY: null lists, which the call refuses.
        let ok = unsafe {
            asc_calibration_estimate_agreed(
                signal.as_ptr(),
                signal.len(),
                std::ptr::null(),
                frames.as_ptr(),
                std::ptr::null(),
                frames.as_ptr(),
                48_000,
                0.0,
                &mut result,
                &mut spread,
            )
        };
        assert!(!ok);
        assert_eq!(result, AscCalibration::default());
        assert_eq!(spread, -1.0);
    }
}
