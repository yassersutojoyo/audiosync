//! Microphone calibration of one output's offset (S232 to S234), against
//! synthetic rooms: the signal at a known delay, an echo and noise.

use audiosync_core::calibrate::{
    self, Arrival, CLIP_RUN, GUARD_MS, MAX_ECHO_SHARE, MAX_RECORDING_FRAMES, MIN_PEAK_OVER_FLOOR,
    Recording, Refusal, Refused, SIGNAL_PEAK,
};

const RATE: u32 = 48_000;

/// Deterministic noise of about the given RMS: a sum of four uniform values
/// from xorshift, close enough to Gaussian for a noise floor.
struct Noise(u32);

impl Noise {
    fn next_uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        f64::from(self.0) / f64::from(u32::MAX) - 0.5
    }

    fn sample(&mut self, rms: f64) -> f32 {
        // Four uniforms on ±0.5 sum to a variance of 4 / 12.
        let sum: f64 = (0..4).map(|_| self.next_uniform()).sum();
        (sum * rms * 3f64.sqrt()) as f32
    }
}

/// One arrival of the signal in a synthetic recording.
struct Path {
    at_ms: f64,
    gain: f32,
}

/// A recording of `seconds` at `rate`: every path's copy of the signal,
/// starting at its fractional time by linear interpolation, plus noise.
fn room(rate: u32, seconds: f64, paths: &[Path], noise_rms: f64, seed: u32) -> Vec<f32> {
    let signal = calibrate::signal(rate).expect("a supported rate");
    let frames = (seconds * f64::from(rate)) as usize;
    let mut noise = Noise(seed);
    let mut recording: Vec<f32> = (0..frames).map(|_| noise.sample(noise_rms)).collect();
    for path in paths {
        let start = path.at_ms * f64::from(rate) / 1000.0;
        let whole = start.floor() as usize;
        let part = (start - start.floor()) as f32;
        for (i, sample) in signal.iter().enumerate() {
            let value = sample * path.gain;
            if let Some(slot) = recording.get_mut(whole + i) {
                *slot += value * (1.0 - part);
            }
            if let Some(slot) = recording.get_mut(whole + i + 1) {
                *slot += value * part;
            }
        }
    }
    recording
}

/// The signal heard at `at_ms` at a gain of 0.4, with an echo of 0.45 of it
/// 7 ms later and noise 40 dB under the signal's peak.
fn heard(rate: u32, at_ms: f64, seed: u32) -> Vec<f32> {
    let gain = 0.4;
    room(
        rate,
        2.5,
        &[
            Path { at_ms, gain },
            Path {
                at_ms: at_ms + 7.0,
                gain: gain * 0.45,
            },
        ],
        f64::from(SIGNAL_PEAK * gain) * 0.01,
        seed,
    )
}

fn arrival(recording: &[f32], rate: u32) -> Result<Arrival, Refusal> {
    let signal = calibrate::signal(rate).expect("a supported rate");
    calibrate::arrival(&signal, recording, rate)
}

fn estimate(reference: &[f32], chosen: &[f32], rate: u32, old_ms: f64) -> Result<f64, Refused> {
    let signal = calibrate::signal(rate).expect("a supported rate");
    calibrate::estimate(&signal, reference, chosen, rate, old_ms).map(|found| found.offset_ms)
}

// covers: S232, S234
#[test]
fn a_known_delay_with_noise_and_an_echo_gives_the_offset_within_a_millisecond() {
    // The 12 ms of input latency and capture start are in both recordings,
    // so only the difference of 243.47 ms counts.
    for rate in [44_100, 48_000, 96_000] {
        for (old, reference_ms, chosen_ms) in [
            (0.0, 12.0 + 37.4, 12.0 + 280.87),
            (-150.0, 21.3, 21.3 + 150.2),
            (400.0, 610.0, 9.6),
        ] {
            let reference = heard(rate, reference_ms, 7);
            let chosen = heard(rate, chosen_ms, 11);
            let expected = old - (chosen_ms - reference_ms);
            let found = estimate(&reference, &chosen, rate, old).expect("accepted");
            assert!(
                (found - expected).abs() <= 1.0,
                "{rate} Hz: {found} against {expected}"
            );
            assert_eq!(found, found.round(), "whole ms");
        }
    }
}

// covers: S232
#[test]
fn a_chosen_output_heard_later_than_the_reference_ends_up_with_a_lower_offset() {
    let reference = heard(RATE, 100.0, 1);
    let later = heard(RATE, 350.0, 2);
    let earlier = heard(RATE, 20.0, 3);
    assert_eq!(estimate(&reference, &later, RATE, 0.0), Ok(-250.0));
    assert_eq!(estimate(&reference, &earlier, RATE, 0.0), Ok(80.0));
    assert_eq!(estimate(&reference, &reference, RATE, -120.0), Ok(-120.0));
}

// covers: S232
#[test]
fn the_offset_is_clamped_at_both_ends_of_the_range() {
    let reference = heard(RATE, 100.0, 1);
    let later = heard(RATE, 400.0, 2);
    let earlier = heard(RATE, 20.0, 3);
    assert_eq!(estimate(&reference, &later, RATE, -1900.0), Ok(-2000.0));
    assert_eq!(estimate(&reference, &earlier, RATE, 19_990.0), Ok(20_000.0));
    // An old offset counts as stored ones do (S175): clamped, or 0 when it
    // isn't finite.
    assert_eq!(estimate(&reference, &later, RATE, -5000.0), Ok(-2000.0));
    assert_eq!(estimate(&reference, &later, RATE, f64::NAN), Ok(-300.0));
    // A clamped offset is still accepted, and says so: the output stays off
    // by what was cut away. Landing exactly on an end is no clamp.
    let clamped = |chosen: &[f32], old: f64| {
        let signal = calibrate::signal(RATE).expect("48 kHz");
        calibrate::estimate(&signal, &reference, chosen, RATE, old)
            .expect("accepted")
            .clamped
    };
    assert!(clamped(&later, -1900.0));
    assert!(clamped(&earlier, 19_990.0));
    assert!(clamped(&later, -5000.0));
    assert!(!clamped(&later, -1700.0));
    assert!(!clamped(&earlier, 19_920.0));
    assert!(!clamped(&later, 0.0));
}

// covers: S232
#[test]
fn a_difference_no_two_offsets_in_range_explain_is_refused() {
    let rate = 22_050;
    let reference = room(
        rate,
        23.5,
        &[Path {
            at_ms: 100.0,
            gain: 0.5,
        }],
        1e-4,
        5,
    );
    let chosen = room(
        rate,
        23.5,
        &[Path {
            at_ms: 22_400.0,
            gain: 0.5,
        }],
        1e-4,
        6,
    );
    let refused = Refused {
        reason: Refusal::Implausible,
        recording: None,
    };
    assert_eq!(estimate(&reference, &chosen, rate, 0.0), Err(refused));
    assert_eq!(estimate(&chosen, &reference, rate, 0.0), Err(refused));
}

fn refused(reason: Refusal, recording: Recording) -> Result<f64, Refused> {
    Err(Refused {
        reason,
        recording: Some(recording),
    })
}

// covers: S234
#[test]
fn silence_and_noise_alone_are_too_quiet() {
    let good = heard(RATE, 50.0, 1);
    let silence = vec![0.0; 2 * RATE as usize];
    let mut noise = Noise(9);
    let hiss: Vec<f32> = (0..2 * RATE as usize).map(|_| noise.sample(0.01)).collect();
    for bad in [&silence, &hiss] {
        assert_eq!(arrival(bad, RATE), Err(Refusal::TooQuiet));
        assert_eq!(
            estimate(bad, &good, RATE, 30.0),
            refused(Refusal::TooQuiet, Recording::Reference)
        );
        assert_eq!(
            estimate(&good, bad, RATE, 30.0),
            refused(Refusal::TooQuiet, Recording::Chosen)
        );
    }
}

// covers: S234
#[test]
fn a_clipped_recording_is_refused() {
    let good = heard(RATE, 50.0, 1);
    let loud: Vec<f32> = good.iter().map(|s| (s * 40.0).clamp(-1.0, 1.0)).collect();
    assert_eq!(
        estimate(&good, &loud, RATE, 0.0),
        refused(Refusal::Clipped, Recording::Chosen)
    );
    let mut spike = good.clone();
    spike[1000..1000 + CLIP_RUN - 1].fill(1.0);
    assert!(arrival(&spike, RATE).is_ok(), "a shorter run is no clip");
    spike[2000..2000 + CLIP_RUN].fill(-1.0);
    assert_eq!(arrival(&spike, RATE), Err(Refusal::Clipped));
    let mut broken = good.clone();
    broken[5] = f32::NAN;
    assert_eq!(arrival(&broken, RATE), Err(Refusal::Clipped));
}

// covers: S234
#[test]
fn a_recording_too_short_or_too_long_is_refused() {
    let signal = calibrate::signal(RATE).expect("48 kHz");
    let shortest = signal.len() + RATE as usize / 10;
    let good = heard(RATE, 50.0, 1);
    assert_eq!(
        estimate(&good, &good[..shortest - 1], RATE, 0.0),
        refused(Refusal::TooShort, Recording::Chosen)
    );
    assert_eq!(
        estimate(&good[..100], &good, RATE, 0.0),
        refused(Refusal::TooShort, Recording::Reference)
    );
    let mut padded = good[..shortest].to_vec();
    assert!(arrival(&padded, RATE).is_ok(), "the shortest one counts");
    padded.resize(MAX_RECORDING_FRAMES + 1, 0.0);
    assert_eq!(arrival(&padded, RATE), Err(Refusal::TooLong));
    // A signal that isn't the one for this rate.
    assert_eq!(
        calibrate::arrival(&signal[1..], &good, RATE),
        Err(Refusal::TooShort)
    );
}

// covers: S234
#[test]
fn an_echo_nearly_as_strong_as_the_first_arrival_is_ambiguous() {
    let with_echo = |share: f32, after_ms: f64| {
        room(
            RATE,
            2.0,
            &[
                Path {
                    at_ms: 80.0,
                    gain: 0.5,
                },
                Path {
                    at_ms: 80.0 + after_ms,
                    gain: 0.5 * share,
                },
            ],
            1e-4,
            3,
        )
    };
    assert_eq!(
        arrival(&with_echo(0.9, 40.0), RATE),
        Err(Refusal::AmbiguousEcho)
    );
    assert_eq!(
        arrival(&with_echo(1.0, 300.0), RATE),
        Err(Refusal::AmbiguousEcho)
    );
    let clear = arrival(&with_echo(0.6, 40.0), RATE).expect("accepted");
    assert_eq!(clear.frame, 80 * RATE as usize / 1000);
    assert!((clear.echo_share - 0.6).abs() < 0.05, "{clear:?}");
    // Inside the guard window a reflection is part of the arrival itself.
    let close = arrival(&with_echo(0.95, GUARD_MS / 2.0), RATE).expect("accepted");
    assert_eq!(close.frame, 80 * RATE as usize / 1000);
}

// A speaker facing away from the microphone, or hidden behind a laptop lid,
// can reach it more strongly by a wall than directly; the sound still
// arrived first by the direct path, so that is the arrival to time.
// covers: S234
#[test]
fn a_reflection_stronger_than_the_direct_path_still_times_the_direct_path() {
    let recording = room(
        RATE,
        2.0,
        &[
            Path {
                at_ms: 80.0,
                gain: 0.3,
            },
            Path {
                at_ms: 102.0,
                gain: 0.5,
            },
        ],
        1e-4,
        5,
    );
    let found = arrival(&recording, RATE).expect("accepted");
    assert_eq!(found.frame, 80 * RATE as usize / 1000, "{found:?}");
}

/// Why the thresholds sit where they do: noise alone stays well under the
/// floor threshold, a signal 20 dB under the noise stays well above it, and
/// the signal's own sidelobes outside the guard window stay far under the
/// echo threshold.
// covers: S233, S234
#[test]
fn the_thresholds_leave_room_on_both_sides() {
    let signal = calibrate::signal(RATE).expect("48 kHz");
    for seed in 1..=8 {
        let mut noise = Noise(seed);
        let hiss: Vec<f32> = (0..3 * RATE as usize).map(|_| noise.sample(0.02)).collect();
        let floor_ratio = calibrate::measure(&signal, &hiss, RATE)
            .expect("measured")
            .peak_over_floor;
        assert!(
            floor_ratio < MIN_PEAK_OVER_FLOOR * 0.75,
            "seed {seed}: {floor_ratio}"
        );
    }
    let signal_rms = f64::from(SIGNAL_PEAK) / 2f64.sqrt();
    let buried = room(
        RATE,
        2.0,
        &[Path {
            at_ms: 33.3,
            gain: 0.1,
        }],
        signal_rms * 0.1 * 10.0,
        4,
    );
    let found = arrival(&buried, RATE).expect("a signal 20 dB under the noise");
    assert!(
        found.peak_over_floor > MIN_PEAK_OVER_FLOOR * 1.5,
        "{found:?}"
    );
    let clean = room(
        RATE,
        2.0,
        &[Path {
            at_ms: 33.3,
            gain: 1.0,
        }],
        0.0,
        4,
    );
    let found = arrival(&clean, RATE).expect("the signal alone");
    assert!(found.echo_share < MAX_ECHO_SHARE / 10.0, "{found:?}");
    let drowned = room(
        RATE,
        2.0,
        &[Path {
            at_ms: 33.3,
            gain: 0.01,
        }],
        signal_rms * 0.01 * 100.0,
        4,
    );
    assert_eq!(arrival(&drowned, RATE), Err(Refusal::TooQuiet));
}

// covers: S233
#[test]
fn the_signal_is_a_quiet_faded_sweep_that_sounds_the_same_at_any_rate() {
    for rate in [22_050, 44_100, 48_000, 96_000, 192_000, 384_000] {
        let signal = calibrate::signal(rate).expect("a supported rate");
        assert_eq!(signal.len(), (0.75 * f64::from(rate)).round() as usize);
        assert_eq!(calibrate::signal_frames(rate), Some(signal.len()));
        let peak = signal.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= SIGNAL_PEAK && peak > SIGNAL_PEAK * 0.99, "{peak}");
        // The fades: the first and last millisecond stay under 1 % of the
        // peak, so neither end clicks.
        let ms = rate as usize / 1000;
        for sample in signal[..ms].iter().chain(&signal[signal.len() - ms..]) {
            assert!(sample.abs() < SIGNAL_PEAK * 0.01, "{rate} Hz: {sample}");
        }
    }
    let low = calibrate::signal(48_000).expect("48 kHz");
    let high = calibrate::signal(96_000).expect("96 kHz");
    for (i, sample) in low.iter().enumerate().take(low.len() - 1) {
        assert!((sample - high[2 * i]).abs() < 1e-5, "frame {i}");
    }
    for rate in [0, 8000, 22_049, 384_001] {
        assert_eq!(calibrate::signal(rate), None);
        assert_eq!(calibrate::signal_frames(rate), None);
    }
}
