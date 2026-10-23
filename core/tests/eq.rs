//! Equalizer behavior (spec section 3).

use std::f64::consts::PI;

use audiosync_core::eq::{
    BAND_CENTERS_HZ, BAND_COUNT, BAND_Q, Biquad, CurveMatch, DEFAULT_SAMPLE_RATE, EqControl,
    EqProcessor, EqSettings, MAX_CENTER_RATIO, MAX_CURVE_NAME_CHARS, MAX_USER_CURVES, PRESETS,
    UserCurve, delete_curve, design_bands, equalizer, matching_curve, preset, quantize_gain_db,
    rename_curve, sanitized_curves, save_curve,
};
use proptest::prelude::*;

const BLOCK: usize = 512;

fn enabled(gains_db: [f64; BAND_COUNT], preamp_db: f64) -> EqSettings {
    EqSettings {
        enabled: true,
        preamp_db,
        gains_db,
    }
}

fn with_band(band: usize, gain_db: f64) -> [f64; BAND_COUNT] {
    let mut gains = [0.0; BAND_COUNT];
    gains[band] = gain_db;
    gains
}

/// A stereo sine block starting at frame `start`.
fn sine(start: usize, frames: usize, freq: f64, rate: f64, amplitude: f64) -> Vec<f32> {
    (start..start + frames)
        .flat_map(|i| {
            let x = (amplitude * (2.0 * PI * freq * i as f64 / rate).sin()) as f32;
            [x, x]
        })
        .collect()
}

/// The spec's measurement (S32): a sine at `amplitude` for 1 s in 512-frame
/// blocks, RMS over the settled second half, output against input, in dB.
fn measure_db(processor: &mut EqProcessor, rate: f64, freq: f64, amplitude: f64) -> f64 {
    let total = rate as usize;
    let (mut energy_in, mut energy_out) = (0.0f64, 0.0f64);
    for start in (0..total).step_by(BLOCK) {
        let frames = BLOCK.min(total - start);
        let input = sine(start, frames, freq, rate, amplitude);
        let mut block = input.clone();
        processor.process(&mut block);
        if start >= total / 2 {
            for (x, y) in input.iter().zip(&block).step_by(2) {
                energy_in += f64::from(*x).powi(2);
                energy_out += f64::from(*y).powi(2);
            }
        }
    }
    10.0 * (energy_out / energy_in).log10()
}

fn configured(rate: f64, settings: EqSettings) -> (EqControl, EqProcessor) {
    let (mut control, processor) = equalizer();
    assert!(control.set_sample_rate(rate));
    control.set_settings(settings);
    (control, processor)
}

/// The spec's coefficient formulas, written out independently (S25).
fn spec_peaking(center: f64, gain_db: f64, fs: f64) -> [f64; 5] {
    let f = center.min(0.45 * fs);
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * f / fs;
    let alpha = w0.sin() / (2.0 * 1.41);
    let a0 = 1.0 + alpha / a;
    [
        (1.0 + alpha * a) / a0,
        -2.0 * w0.cos() / a0,
        (1.0 - alpha * a) / a0,
        -2.0 * w0.cos() / a0,
        (1.0 - alpha / a) / a0,
    ]
}

/// Magnitude response of one biquad at `freq`, in dB.
fn response_db(band: &Biquad, freq: f64, fs: f64) -> f64 {
    let w = 2.0 * PI * freq / fs;
    let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
    let num = (
        band.b0 + band.b1 * c1 + band.b2 * c2,
        band.b1 * s1 + band.b2 * s2,
    );
    let den = (
        1.0 + band.a1 * c1 + band.a2 * c2,
        band.a1 * s1 + band.a2 * s2,
    );
    10.0 * ((num.0 * num.0 + num.1 * num.1) / (den.0 * den.0 + den.1 * den.1)).log10()
}

/// A plain float64 transposed-direct-form-II reference of S26.
struct Reference {
    bands: [[f64; 5]; BAND_COUNT],
    preamp: f64,
    state: [[(f64, f64); BAND_COUNT]; 2],
}

impl Reference {
    fn new(settings: &EqSettings, fs: f64) -> Reference {
        Reference {
            bands: std::array::from_fn(|i| {
                spec_peaking(BAND_CENTERS_HZ[i], settings.gains_db[i], fs)
            }),
            preamp: 10f64.powf(settings.preamp_db / 20.0),
            state: [[(0.0, 0.0); BAND_COUNT]; 2],
        }
    }

    fn retune(&mut self, settings: &EqSettings, fs: f64) {
        let state = self.state;
        *self = Reference::new(settings, fs);
        self.state = state;
    }

    fn process(&mut self, block: &mut [f32]) {
        for frame in block.chunks_exact_mut(2) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let mut y = f64::from(*sample) * self.preamp;
                for (c, z) in self.bands.iter().zip(self.state[channel].iter_mut()) {
                    let x = y;
                    y = c[0] * x + z.0;
                    z.0 = c[1] * x - c[3] * y + z.1;
                    z.1 = c[2] * x - c[4] * y;
                }
                *sample = y.clamp(-1.0, 1.0) as f32;
            }
        }
    }
}

// covers: S22
#[test]
fn ten_bands_centered_from_32_hz_to_16_khz_in_ascending_order() {
    assert_eq!(BAND_COUNT, 10);
    assert_eq!(
        BAND_CENTERS_HZ,
        [
            32.0, 64.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16_000.0
        ]
    );
    assert!(BAND_CENTERS_HZ.windows(2).all(|pair| pair[0] < pair[1]));
}

// covers: S23
#[test]
fn the_host_quantizes_to_half_db_steps_between_minus_and_plus_12_db() {
    assert_eq!(quantize_gain_db(3.2), 3.0);
    assert_eq!(quantize_gain_db(3.3), 3.5);
    assert_eq!(quantize_gain_db(-0.1), 0.0);
    assert!(quantize_gain_db(-0.1).is_sign_positive());
    assert_eq!(quantize_gain_db(12.4), 12.0);
    assert_eq!(quantize_gain_db(-40.0), -12.0);
    assert_eq!(quantize_gain_db(f64::INFINITY), 12.0);
    assert_eq!(quantize_gain_db(f64::NAN), 0.0);
    let settings = enabled(with_band(2, 7.26), 13.0).quantized();
    assert_eq!(settings.gains_db[2], 7.5);
    assert_eq!(settings.preamp_db, 12.0);
}

// covers: S23
#[test]
fn the_dsp_applies_any_finite_gain_it_is_given() {
    // +18 dB is outside the host's range, yet the DSP applies it.
    let (control, mut processor) = configured(48_000.0, enabled(with_band(5, 18.0), 0.0));
    assert_eq!(control.settings().gains_db[5], 18.0);
    let measured = measure_db(&mut processor, 48_000.0, 1000.0, 0.02);
    assert!((measured - 18.0).abs() < 0.2, "measured {measured:.2} dB");
    // A non-finite value counts as 0 dB.
    let (control, mut processor) =
        configured(48_000.0, enabled(with_band(5, f64::NAN), f64::INFINITY));
    assert_eq!(control.settings().gains_db[5], 0.0);
    assert_eq!(control.settings().preamp_db, 0.0);
    assert!(measure_db(&mut processor, 48_000.0, 1000.0, 0.25).abs() < 0.01);
}

// covers: S24
#[test]
fn settings_start_disabled_flat_and_at_unity_preamp() {
    let defaults = EqSettings::default();
    assert!(!defaults.enabled);
    assert_eq!(defaults.preamp_db, 0.0);
    assert_eq!(defaults.gains_db, [0.0; BAND_COUNT]);
    let (control, _) = equalizer();
    assert_eq!(control.settings(), defaults);
}

// covers: S25
#[test]
fn bands_are_rbj_peaking_filters_with_q_141() {
    assert_eq!(BAND_Q, 1.41);
    assert_eq!(MAX_CENTER_RATIO, 0.45);
    for fs in [8_000.0, 32_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        for gain in [-12.0, -3.5, 0.0, 6.0, 12.0] {
            for (band, center) in BAND_CENTERS_HZ.iter().enumerate() {
                let designed = Biquad::peaking(*center, gain, fs);
                let spec = spec_peaking(*center, gain, fs);
                let got = [
                    designed.b0,
                    designed.b1,
                    designed.b2,
                    designed.a1,
                    designed.a2,
                ];
                assert_eq!(got, spec, "band {band} at {fs} Hz, {gain} dB");
                assert_eq!(designed.b1, designed.a1);
                // A peaking filter has exactly its gain at its (limited) center,
                // and 0 dB at DC.
                let f = center.min(0.45 * fs);
                assert!((response_db(&designed, f, fs) - gain).abs() < 1e-9);
                assert!(response_db(&designed, 0.0, fs).abs() < 1e-9);
            }
        }
    }
    // Above 0.45 × fs the design frequency is limited: at 32 kHz the 16 kHz band
    // is designed at 14.4 kHz.
    assert_eq!(
        Biquad::peaking(16_000.0, 6.0, 32_000.0),
        Biquad::peaking(14_400.0, 6.0, 32_000.0)
    );
    assert_eq!(
        design_bands(&PRESETS[1].gains_db, 48_000.0)[0],
        Biquad::peaking(32.0, 6.0, 48_000.0)
    );
}

// covers: S2, S26
#[test]
fn each_channel_runs_its_own_cascade_in_float64_and_is_clamped() {
    let settings = enabled(PRESETS[10].gains_db, -2.0);
    let (_control, mut processor) = configured(44_100.0, settings);
    let mut reference = Reference::new(&settings, 44_100.0);
    for start in (0..44_100).step_by(BLOCK) {
        // L carries a sine, R a different one: separate state per channel.
        let mut block: Vec<f32> = (start..start + BLOCK)
            .flat_map(|i| {
                let t = i as f64 / 44_100.0;
                [
                    (0.3 * (2.0 * PI * 70.0 * t).sin()) as f32,
                    (0.2 * (2.0 * PI * 5000.0 * t).sin()) as f32,
                ]
            })
            .collect();
        let mut expected = block.clone();
        processor.process(&mut block);
        reference.process(&mut expected);
        assert_eq!(block, expected, "block at frame {start}");
    }
    // One silent channel stays exactly silent next to a loud one.
    let mut block: Vec<f32> = sine(0, BLOCK, 440.0, 44_100.0, 0.5)
        .chunks_exact(2)
        .flat_map(|frame| [frame[0], 0.0])
        .collect();
    let (_control, mut fresh) = configured(44_100.0, settings);
    fresh.process(&mut block);
    assert!(block.iter().skip(1).step_by(2).all(|&r| r == 0.0));
    // The output is clamped to ±1.
    let (_control, mut loud) = configured(48_000.0, enabled([12.0; BAND_COUNT], 12.0));
    let mut block = sine(0, 4096, 1000.0, 48_000.0, 0.9);
    loud.process(&mut block);
    let peak = block.iter().fold(0.0f32, |peak, x| peak.max(x.abs()));
    assert_eq!(peak, 1.0);
}

// covers: S27, S123
#[test]
fn disabled_leaves_the_buffer_untouched() {
    let (mut control, mut processor) = equalizer();
    control.set_settings(EqSettings {
        enabled: false,
        preamp_db: -12.0,
        gains_db: [12.0; BAND_COUNT],
    });
    let payload_nan = f32::from_bits(0x7fc0_1234);
    let mut block = vec![
        1.5,
        -3.0,
        payload_nan,
        f32::INFINITY,
        f32::NEG_INFINITY,
        -0.0,
        0.25,
        1e-40,
    ];
    let before: Vec<u32> = block.iter().map(|x| x.to_bits()).collect();
    processor.process(&mut block);
    let after: Vec<u32> = block.iter().map(|x| x.to_bits()).collect();
    assert_eq!(after, before, "no preamp, no filtering, no clamping");
    assert_eq!(control.nan_resets(), 0);
}

// covers: S28
#[test]
fn the_first_enabled_block_after_being_disabled_starts_from_zero_state() {
    let settings = enabled(PRESETS[1].gains_db, 0.0);
    let (mut control, mut processor) = configured(48_000.0, settings);
    let mut loud = sine(0, BLOCK, 50.0, 48_000.0, 0.5);
    processor.process(&mut loud);
    // Without a disable, the filters ring on into silence.
    let mut silence = vec![0.0f32; BLOCK * 2];
    processor.process(&mut silence);
    assert!(silence.iter().any(|&x| x != 0.0), "state carries over");

    processor.process(&mut sine(BLOCK, BLOCK, 50.0, 48_000.0, 0.5));
    control.set_settings(EqSettings {
        enabled: false,
        ..settings
    });
    processor.process(&mut vec![0.3f32; BLOCK * 2]);
    control.set_settings(settings);
    let mut silence = vec![0.0f32; BLOCK * 2];
    processor.process(&mut silence);
    assert!(silence.iter().all(|&x| x == 0.0), "stale state was zeroed");
}

// covers: S29, S123, S131
#[test]
fn a_non_finite_cascade_output_resets_that_channel_and_silence_stays_silent() {
    for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let settings = enabled(PRESETS[5].gains_db, 3.0);
        let (control, mut processor) = configured(48_000.0, settings);
        let mut reference = Reference::new(&settings, 48_000.0);
        let mut warm = sine(0, BLOCK, 300.0, 48_000.0, 0.3);
        let mut warm_ref = warm.clone();
        processor.process(&mut warm);
        reference.process(&mut warm_ref);

        // L: sine, then the poison at frame 100, then silence. R: the sine throughout.
        let mut block = sine(BLOCK, BLOCK, 300.0, 48_000.0, 0.3);
        block[200] = poison;
        for frame in 101..BLOCK {
            block[2 * frame] = 0.0;
        }
        let mut expected = block.clone();
        processor.process(&mut block);
        reference.process(&mut expected);
        assert_eq!(block[200], 0.0, "the non-finite output becomes 0");
        assert!(
            block[202..].iter().step_by(2).all(|&l| l == 0.0),
            "silence after it stays exactly 0"
        );
        let right: Vec<f32> = block.iter().skip(1).step_by(2).copied().collect();
        let right_ref: Vec<f32> = expected.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(right, right_ref, "the other channel is unaffected");
        assert_eq!(control.nan_resets(), 1);

        let mut more_silence = vec![0.0f32; BLOCK * 2];
        for frame in 0..BLOCK {
            more_silence[2 * frame + 1] = block[2 * BLOCK - 1];
        }
        processor.process(&mut more_silence);
        assert!(more_silence.iter().step_by(2).all(|&l| l == 0.0));
    }
}

// covers: S125
#[test]
fn finite_input_after_a_non_finite_one_is_finite_within_one_block() {
    let (_control, mut processor) = configured(48_000.0, enabled([12.0; BAND_COUNT], 12.0));
    let mut poisoned = vec![f32::NAN; BLOCK * 2];
    processor.process(&mut poisoned);
    assert!(poisoned.iter().all(|&x| x == 0.0));
    let mut next = sine(0, BLOCK, 1000.0, 48_000.0, 0.5);
    processor.process(&mut next);
    assert!(next.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
}

// covers: S30, S140
#[test]
fn the_rate_defaults_to_48_khz_and_a_change_recomputes_and_zeroes_state() {
    let (mut control, mut processor) = equalizer();
    assert_eq!(control.sample_rate(), DEFAULT_SAMPLE_RATE);
    assert_eq!(DEFAULT_SAMPLE_RATE, 48_000.0);
    let settings = enabled(with_band(5, 6.0), 0.0);
    control.set_settings(settings);
    assert_eq!(control.bands(), design_bands(&settings.gains_db, 48_000.0));

    // The same rate again is a no-op: the filters keep ringing into silence.
    processor.process(&mut sine(0, BLOCK, 1000.0, 48_000.0, 0.5));
    assert!(control.set_sample_rate(48_000.0));
    let mut silence = vec![0.0f32; BLOCK * 2];
    processor.process(&mut silence);
    assert!(silence.iter().any(|&x| x != 0.0));

    // A new rate recomputes the coefficients and zeroes all state.
    processor.process(&mut sine(0, BLOCK, 1000.0, 48_000.0, 0.5));
    assert!(control.set_sample_rate(44_100.0));
    assert_eq!(control.sample_rate(), 44_100.0);
    assert_eq!(control.bands(), design_bands(&settings.gains_db, 44_100.0));
    let mut silence = vec![0.0f32; BLOCK * 2];
    processor.process(&mut silence);
    assert!(silence.iter().all(|&x| x == 0.0));
    let measured = measure_db(&mut processor, 44_100.0, 1000.0, 0.25);
    assert!(
        (measured - 6.0).abs() < 0.2,
        "measured {measured:.2} dB at 44.1 kHz"
    );

    for bad in [0.0, -48_000.0, f64::NAN, f64::INFINITY] {
        assert!(!control.set_sample_rate(bad));
        assert_eq!(control.sample_rate(), 44_100.0);
    }
}

// covers: S16
#[test]
fn every_capture_rate_from_8_to_384_khz_gives_stable_filters() {
    let rates = [
        8_000.0, 11_025.0, 16_000.0, 22_050.0, 32_000.0, 44_100.0, 48_000.0, 88_200.0, 96_000.0,
        176_400.0, 192_000.0, 352_800.0, 384_000.0,
    ];
    for fs in rates {
        for gains in [
            [12.0; BAND_COUNT],
            [-12.0; BAND_COUNT],
            PRESETS[10].gains_db,
        ] {
            for band in design_bands(&gains, fs) {
                // Both poles inside the unit circle.
                assert!(
                    band.a2.abs() < 1.0 && band.a1.abs() < 1.0 + band.a2,
                    "{fs} Hz: {band:?}"
                );
            }
        }
        let (_control, mut processor) = configured(fs, enabled(PRESETS[10].gains_db, 0.0));
        let mut block = sine(0, BLOCK, 440.0, fs, 0.5);
        processor.process(&mut block);
        assert!(block.iter().all(|x| x.is_finite()));
    }
}

// covers: S31
#[test]
fn a_change_takes_effect_at_the_next_block_and_keeps_filter_state() {
    let first = enabled(PRESETS[1].gains_db, 0.0);
    let second = enabled(PRESETS[3].gains_db, -4.0);
    let (mut control, mut processor) = configured(48_000.0, first);
    let mut reference = Reference::new(&first, 48_000.0);
    let mut block = sine(0, BLOCK, 200.0, 48_000.0, 0.4);
    let mut expected = block.clone();
    processor.process(&mut block);
    reference.process(&mut expected);
    assert_eq!(block, expected);

    control.set_settings(second);
    reference.retune(&second, 48_000.0);
    for start in (BLOCK..BLOCK * 8).step_by(BLOCK) {
        let mut block = sine(start, BLOCK, 200.0, 48_000.0, 0.4);
        let mut expected = block.clone();
        processor.process(&mut block);
        reference.process(&mut expected);
        assert_eq!(
            block, expected,
            "new coefficients, same state, from the block boundary on"
        );
    }
    // Toggling the flag together with new gains also applies as one change.
    control.set_settings(EqSettings {
        enabled: false,
        ..first
    });
    let mut untouched = sine(0, BLOCK, 200.0, 48_000.0, 0.4);
    let copy = untouched.clone();
    processor.process(&mut untouched);
    assert_eq!(untouched, copy);
}

// covers: S32
#[test]
fn measured_response_matches_the_reference_measurements() {
    let (_c, mut flat) = configured(48_000.0, enabled([0.0; BAND_COUNT], 0.0));
    let db = measure_db(&mut flat, 48_000.0, 1000.0, 0.25);
    assert!(db.abs() <= 0.05, "flat: {db:.3} dB");

    let (_c, mut band5) = configured(48_000.0, enabled(with_band(5, 6.0), 0.0));
    let db = measure_db(&mut band5, 48_000.0, 1000.0, 0.25);
    assert!((db - 6.0).abs() <= 0.2, "+6 dB band at 1 kHz: {db:.3} dB");

    let (_c, mut band5) = configured(44_100.0, enabled(with_band(5, 6.0), 0.0));
    let db = measure_db(&mut band5, 44_100.0, 1000.0, 0.25);
    assert!(
        (db - 6.0).abs() <= 0.2,
        "+6 dB band at 1 kHz, 44.1 kHz: {db:.3} dB"
    );

    let (_c, mut band5) = configured(48_000.0, enabled(with_band(5, 6.0), 0.0));
    let db = measure_db(&mut band5, 48_000.0, 8000.0, 0.25);
    assert!(db.abs() <= 0.5, "+6 dB band at 8 kHz: {db:.3} dB");

    let bass = preset("bassBoost").unwrap();
    let (_c, mut boost) = configured(48_000.0, enabled(bass.gains_db, 0.0));
    let db = measure_db(&mut boost, 48_000.0, 40.0, 0.25);
    assert!(db > 5.0, "Bass Boost at 40 Hz: {db:.3} dB");

    let (_c, mut preamp) = configured(48_000.0, enabled([0.0; BAND_COUNT], -6.0));
    let db = measure_db(&mut preamp, 48_000.0, 1000.0, 0.25);
    assert!((db + 6.0).abs() <= 0.1, "preamp −6 dB: {db:.3} dB");

    let (_c, mut off) = configured(
        48_000.0,
        EqSettings {
            enabled: false,
            ..enabled(with_band(5, 6.0), -6.0)
        },
    );
    let db = measure_db(&mut off, 48_000.0, 1000.0, 0.25);
    assert!(db.abs() <= 0.01, "disabled: {db:.3} dB");
}

// covers: S33
#[test]
fn twelve_presets_with_unique_ids_in_menu_order() {
    let ids: Vec<&str> = PRESETS.iter().map(|preset| preset.id).collect();
    assert_eq!(
        ids,
        [
            "flat",
            "bassBoost",
            "bassCut",
            "trebleBoost",
            "vocal",
            "rock",
            "pop",
            "jazz",
            "classical",
            "electronic",
            "loudness",
            "smallSpeakers"
        ]
    );
    for preset_entry in &PRESETS {
        assert_eq!(preset_entry.gains_db.len(), BAND_COUNT);
        assert_eq!(preset(preset_entry.id), Some(preset_entry));
    }
    assert_eq!(preset("missing"), None);
}

// covers: S33
#[test]
fn a_preset_sets_the_gains_only_and_matching_is_exact() {
    let mut settings = enabled([1.0; BAND_COUNT], -3.0);
    assert_eq!(settings.matching_preset(), None);
    settings.apply_preset(preset("rock").unwrap());
    assert!(settings.enabled);
    assert_eq!(settings.preamp_db, -3.0);
    assert_eq!(settings.matching_preset().map(|p| p.id), Some("rock"));
    settings.gains_db[4] += 0.5;
    assert_eq!(settings.matching_preset(), None);
    assert_eq!(
        EqSettings::default().matching_preset().map(|p| p.id),
        Some("flat")
    );
}

// covers: S33
#[test]
fn reset_reads_flat_and_zeroes_the_preamp() {
    let mut settings = enabled(preset("rock").unwrap().gains_db, -4.5);
    settings.reset();
    assert!(settings.enabled, "Reset leaves the switch alone");
    assert_eq!(settings.preamp_db, 0.0);
    assert_eq!(settings.matching_preset().map(|p| p.id), Some("flat"));
    let mut flat = enabled([2.0; BAND_COUNT], 0.0);
    flat.apply_preset(preset("flat").unwrap());
    assert_eq!(
        flat, settings,
        "Flat and Reset agree once the preamp is 0 dB"
    );
}

// covers: S270
#[test]
fn a_saved_curve_keeps_its_gains_and_matches_after_the_presets() {
    let mut curves = Vec::new();
    let mine = [1.0, 1.5, 2.0, 0.0, -0.5, 0.0, 0.0, 1.0, 2.0, 3.0];
    assert_eq!(save_curve(&mut curves, "  Desk  ", &mine), Some(0));
    assert_eq!(curves[0].name, "Desk");
    assert_eq!(matching_curve(&mine, &curves), Some(CurveMatch::User(0)));
    let rock = preset("rock").unwrap().gains_db;
    assert_eq!(save_curve(&mut curves, "Rocky", &rock), Some(1));
    assert_eq!(
        matching_curve(&rock, &curves),
        Some(CurveMatch::Preset(5)),
        "the presets come first"
    );
    assert_eq!(matching_curve(&[0.5; BAND_COUNT], &curves), None);

    let mut settings = enabled([0.0; BAND_COUNT], -6.0);
    settings.gains_db = curves[0].gains_db;
    assert_eq!(settings.preamp_db, -6.0, "a curve never carries a preamp");
    assert_eq!(
        matching_curve(&settings.gains_db, &curves),
        Some(CurveMatch::User(0))
    );
}

// covers: S270
#[test]
fn saving_under_a_taken_name_replaces_that_curve_and_names_are_checked() {
    let mut curves = Vec::new();
    assert_eq!(save_curve(&mut curves, "Desk", &[1.0; BAND_COUNT]), Some(0));
    assert_eq!(
        save_curve(&mut curves, "Couch", &[2.0; BAND_COUNT]),
        Some(1)
    );
    assert_eq!(save_curve(&mut curves, "desk", &[3.3; BAND_COUNT]), Some(0));
    assert_eq!(curves.len(), 2);
    assert_eq!(curves[0].name, "desk");
    assert_eq!(
        curves[0].gains_db, [3.5; BAND_COUNT],
        "on the controls' steps"
    );
    assert_eq!(save_curve(&mut curves, "   ", &[0.0; BAND_COUNT]), None);
    let long = "x".repeat(MAX_CURVE_NAME_CHARS + 5);
    let index = save_curve(&mut curves, &long, &[0.0; BAND_COUNT]).unwrap();
    assert_eq!(curves[index].name.chars().count(), MAX_CURVE_NAME_CHARS);

    assert!(
        !rename_curve(&mut curves, 1, "DESK"),
        "another curve has it"
    );
    assert!(!rename_curve(&mut curves, 1, " "));
    assert!(!rename_curve(&mut curves, 9, "Attic"));
    assert!(
        rename_curve(&mut curves, 1, "COUCH"),
        "its own name in other case"
    );
    assert!(rename_curve(&mut curves, 1, "Attic"));
    assert_eq!(curves[1].name, "Attic");

    assert!(delete_curve(&mut curves, 0));
    assert_eq!(curves[0].name, "Attic");
    assert!(!delete_curve(&mut curves, 5));
}

// covers: S270
#[test]
fn the_list_is_capped_and_stored_curves_are_cleaned_on_reading() {
    let mut curves = Vec::new();
    for n in 0..MAX_USER_CURVES {
        assert!(save_curve(&mut curves, &format!("c{n}"), &[0.0; BAND_COUNT]).is_some());
    }
    assert_eq!(
        save_curve(&mut curves, "one more", &[0.0; BAND_COUNT]),
        None
    );
    assert_eq!(
        save_curve(&mut curves, "C3", &[1.0; BAND_COUNT]),
        Some(3),
        "a full list still takes new gains for a name it has"
    );

    let mut odd = [0.0; BAND_COUNT];
    odd[0] = f64::NAN;
    odd[1] = 30.0;
    let read = sanitized_curves([
        UserCurve {
            name: " A ".into(),
            gains_db: odd,
        },
        UserCurve {
            name: "".into(),
            gains_db: [1.0; BAND_COUNT],
        },
        UserCurve {
            name: "a".into(),
            gains_db: [2.0; BAND_COUNT],
        },
        UserCurve {
            name: "B".into(),
            gains_db: [0.25; BAND_COUNT],
        },
    ]);
    assert_eq!(read.len(), 2);
    assert_eq!(read[0].name, "A");
    assert_eq!(read[0].gains_db[0], 0.0);
    assert_eq!(read[0].gains_db[1], 12.0);
    assert_eq!(read[1].gains_db[0], quantize_gain_db(0.25));
}

// covers: S7, S34
#[test]
fn a_block_is_equalized_once_in_place_and_both_consumers_get_the_same_samples() {
    let settings = enabled(with_band(5, 6.0), 0.0);
    let (_c, mut processor) = configured(48_000.0, settings);
    let mut reference = Reference::new(&settings, 48_000.0);
    let mut block = sine(0, BLOCK, 1000.0, 48_000.0, 0.25);
    let mut expected = block.clone();
    reference.process(&mut expected);
    let mut to_ring = Vec::new();
    let mut to_airplay = Vec::new();
    processor.equalize_and_deliver(
        &mut block,
        |samples| to_ring.extend_from_slice(samples),
        |samples| to_airplay.extend_from_slice(samples),
    );
    assert_eq!(block, expected, "equalized once, in place");
    let bits = |samples: &[f32]| samples.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&to_ring), bits(&block));
    assert_eq!(
        bits(&to_airplay),
        bits(&block),
        "bit-identical for the ring and the AirPlay leg"
    );
}

proptest! {
    // covers: S123, S125
    #[test]
    fn output_is_always_finite_and_within_full_scale(
        gains in proptest::array::uniform10(-12.0f64..=12.0),
        preamp in -12.0f64..=12.0,
        samples in proptest::collection::vec(
            prop_oneof![
                8 => -1.0f32..=1.0,
                1 => Just(f32::NAN),
                1 => Just(f32::INFINITY),
                1 => Just(f32::NEG_INFINITY),
                1 => -1e30f32..=1e30,
            ],
            2..2048,
        ),
    ) {
        let (_c, mut processor) = configured(48_000.0, enabled(gains, preamp));
        let mut block = samples;
        processor.process(&mut block);
        let frames = block.len() / 2 * 2;
        prop_assert!(block[..frames].iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    }
}
