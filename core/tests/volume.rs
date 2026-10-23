//! Volume curve, host-side volume rules, gain application and input channel
//! mapping (spec sections 2, 6 and 7).

use audiosync_cast::status::{NO_ZONE_LEVEL, device_level, own_level, start_own_level};
use audiosync_core::channels::input_to_stereo;
use audiosync_core::volume::{
    DEFAULT_LOCAL_POSITION, DEFAULT_MASTER_POSITION, GainStage, NO_ZONE_POSITION, SharedGain,
    apply_gain, clamp_position, gain_for_position, gain_to_db, migrate_legacy_percent, output_gain,
    own_position_for_level, position_for_gain, sanitize_position, stage_level, zoned_output_gain,
};
use proptest::prelude::*;

// covers: S70
#[test]
fn zero_is_silent_and_100_is_unity() {
    assert_eq!(gain_for_position(0.0), 0.0);
    assert_eq!(gain_for_position(100.0), 1.0);
    assert_eq!(gain_to_db(gain_for_position(0.0)), f64::NEG_INFINITY);
}

// covers: S70
#[test]
fn fifty_is_about_minus_18_db_and_ten_about_minus_60_db() {
    assert_eq!(gain_for_position(50.0), 0.125);
    assert!((gain_to_db(gain_for_position(50.0)) + 18.06).abs() < 0.005);
    assert!((gain_for_position(10.0) - 0.001).abs() < 1e-15);
    assert!((gain_to_db(gain_for_position(10.0)) + 60.0).abs() < 1e-9);
    // The ends clamp.
    assert_eq!(gain_for_position(-5.0), 0.0);
    assert_eq!(gain_for_position(250.0), 1.0);
    assert_eq!(gain_for_position(f64::NAN), 0.0);
}

// covers: S70
#[test]
fn the_curve_is_strictly_increasing_over_integer_positions() {
    let gains: Vec<f64> = (0..=100).map(|p| gain_for_position(f64::from(p))).collect();
    assert!(gains.windows(2).all(|pair| pair[0] < pair[1]));
}

// covers: S70
#[test]
fn position_and_gain_round_trip() {
    for p in 0..=100 {
        let p = f64::from(p);
        assert!(
            (position_for_gain(gain_for_position(p)) - p).abs() < 1e-9,
            "{p}"
        );
    }
    assert_eq!(position_for_gain(2.0), 100.0);
    assert_eq!(position_for_gain(-1.0), 0.0);
    assert_eq!(position_for_gain(f64::NAN), 0.0);
}

// covers: S71
#[test]
fn a_local_output_plays_at_its_gain_times_the_master_gain() {
    assert_eq!(DEFAULT_LOCAL_POSITION, 60.0);
    assert_eq!(DEFAULT_MASTER_POSITION, 100.0);
    let fresh = output_gain(DEFAULT_LOCAL_POSITION, DEFAULT_MASTER_POSITION);
    assert!(
        (gain_to_db(fresh) + 13.31).abs() < 0.005,
        "a new device starts near −13.3 dB"
    );
    assert_eq!(output_gain(50.0, 50.0), 0.125 * 0.125);
    assert_eq!(output_gain(100.0, 0.0), 0.0);
}

// covers: S71, S249
#[test]
fn the_local_gain_is_own_times_zone_times_master() {
    assert_eq!(zoned_output_gain(50.0, 50.0, 100.0), 0.125 * 0.125);
    assert_eq!(zoned_output_gain(100.0, 50.0, 50.0), 0.125 * 0.125);
    assert_eq!(zoned_output_gain(60.0, 0.0, 100.0), 0.0, "zone 0 is silent");
    assert_eq!(zoned_output_gain(100.0, 150.0, 100.0), 1.0, "clamped");
    let half = zoned_output_gain(DEFAULT_LOCAL_POSITION, 50.0, DEFAULT_MASTER_POSITION);
    assert!(
        (gain_to_db(half) - (gain_to_db(output_gain(60.0, 100.0)) - 18.06)).abs() < 0.005,
        "a zone at 50 takes about 18 dB more"
    );
}

/// The stages as they were before zones: local `g(own) × g(master)`;
/// AirPlay `(own / 100)(master / 100)` and its read-back `level ÷ master`;
/// Cast `master × own` and `level ÷ master`.
fn before_zones_airplay(own: f64, master: f64) -> f64 {
    (own / 100.0).clamp(0.0, 1.0) * (master / 100.0).clamp(0.0, 1.0)
}

// covers: S71, S249, S250
#[test]
fn without_zones_every_gain_and_level_is_bit_identical() {
    let positions = [0.0, 1.0, 13.0, 37.5, 50.0, 60.0, 61.0, 99.0, 100.0];
    for &own in &positions {
        for &master in &positions {
            assert_eq!(
                zoned_output_gain(own, NO_ZONE_POSITION, master).to_bits(),
                output_gain(own, master).to_bits(),
                "local {own} at master {master}"
            );
            assert_eq!(
                stage_level(own, NO_ZONE_POSITION, master).to_bits(),
                before_zones_airplay(own, master).to_bits(),
                "AirPlay {own} at master {master}"
            );
            let (own_level_kept, master_level) = (own / 100.0, master / 100.0);
            assert_eq!(
                device_level(master_level, NO_ZONE_LEVEL, own_level_kept).to_bits(),
                (master_level * own_level_kept).to_bits(),
                "Cast {own} at master {master}"
            );
            for level in [0.0, 0.123, 0.4, 0.777, 1.0] {
                let before = (master > 0.0).then(|| {
                    (level / (master / 100.0) * 100.0_f64)
                        .clamp(0.0, 100.0)
                        .round()
                });
                assert_eq!(
                    own_position_for_level(level, NO_ZONE_POSITION, master).map(f64::to_bits),
                    before.map(f64::to_bits),
                    "AirPlay read-back {level} at master {master}"
                );
                let before = if master > 0.0 {
                    (level / master_level).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                assert_eq!(
                    own_level(level, master_level, NO_ZONE_LEVEL, 0.5).to_bits(),
                    before.to_bits(),
                    "Cast read-back {level} at master {master}"
                );
                assert_eq!(
                    start_own_level(master_level, NO_ZONE_LEVEL, None, level).to_bits(),
                    if master > 0.0 { before } else { level }.to_bits(),
                    "Cast first status {level} at master {master}"
                );
            }
        }
    }
}

// covers: S250
#[test]
fn a_read_back_through_the_zone_divides_by_zone_times_master() {
    assert_eq!(own_position_for_level(0.4, 50.0, 100.0), Some(80.0));
    assert_eq!(own_position_for_level(0.2, 50.0, 50.0), Some(80.0));
    assert_eq!(
        own_position_for_level(0.9, 50.0, 100.0),
        Some(100.0),
        "clamped"
    );
    assert_eq!(
        own_position_for_level(0.333, 100.0, 100.0),
        Some(33.0),
        "a whole step"
    );
    assert_eq!(own_position_for_level(0.4, 0.0, 100.0), None, "zone 0");
    assert_eq!(own_position_for_level(0.4, 50.0, 0.0), None, "master 0");
    assert_eq!(own_position_for_level(f64::NAN, 50.0, 100.0), None);
    assert_eq!(own_position_for_level(0.4, f64::NAN, 100.0), None);
}

// covers: S70, S76
#[test]
fn negative_zero_counts_as_zero() {
    let positive_zero = 0.0f64.to_bits();
    for value in [
        gain_for_position(-0.0),
        position_for_gain(-0.0),
        clamp_position(-0.0),
        sanitize_position(-0.0).unwrap(),
        migrate_legacy_percent(-0.0),
        output_gain(-0.0, 100.0),
    ] {
        assert_eq!(value.to_bits(), positive_zero);
    }
}

// covers: S75
#[test]
fn legacy_percentages_migrate_through_the_inverse_curve() {
    assert!((migrate_legacy_percent(3.49) - 32.68).abs() < 0.01);
    assert_eq!(migrate_legacy_percent(3.49).round(), 33.0);
    assert_eq!(migrate_legacy_percent(100.0), 100.0);
    assert_eq!(migrate_legacy_percent(150.0), 100.0);
    assert_eq!(migrate_legacy_percent(0.0), 0.0);
    assert!((migrate_legacy_percent(12.5) - 50.0).abs() < 1e-9);
}

// covers: S76, S124
#[test]
fn host_volume_inputs_are_clamped_and_non_finite_ones_ignored() {
    assert_eq!(clamp_position(-1.0), 0.0);
    assert_eq!(clamp_position(101.0), 100.0);
    assert_eq!(clamp_position(42.5), 42.5);
    assert_eq!(sanitize_position(140.0), Some(100.0));
    assert_eq!(sanitize_position(-0.5), Some(0.0));
    assert_eq!(sanitize_position(f64::NAN), None);
    assert_eq!(sanitize_position(f64::INFINITY), None);
}

// covers: S14
#[test]
fn an_input_device_contributes_its_first_two_channels() {
    let mut stereo = vec![9.0f32; 8];
    // Mono: copied to both sides.
    assert_eq!(input_to_stereo(&[0.1, 0.2, 0.3, 0.4], 1, &mut stereo), 4);
    assert_eq!(stereo, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3, 0.4, 0.4]);
    // Four channels: the third and fourth are ignored.
    let quad = [0.1, 0.2, 0.7, 0.8, 0.3, 0.4, 0.7, 0.8];
    assert_eq!(input_to_stereo(&quad, 4, &mut stereo), 2);
    assert_eq!(stereo[..4], [0.1, 0.2, 0.3, 0.4]);
    // No channels: nothing.
    assert_eq!(input_to_stereo(&quad, 0, &mut stereo), 0);
}

// covers: S64
#[test]
fn every_output_frame_is_multiplied_by_the_gain_and_gain_zero_is_exact_zeros() {
    let shared = SharedGain::new(0.5);
    let mut stage = GainStage::new();
    let mut buffer = vec![0.8f32, -0.4, 0.2, 0.0];
    // A gain set before the first callback applies from the first callback.
    stage.apply(&mut buffer, 2, shared.get());
    assert_eq!(buffer, [0.4, -0.2, 0.1, 0.0]);
    shared.set(0.0);
    let mut buffer = vec![-0.8f32; 512];
    stage.apply(&mut buffer, 2, shared.get());
    // The change applies from the next callback, ramping across it at most...
    assert!(buffer[0] < 0.0 && buffer[..510].iter().all(|x| *x <= 0.0));
    assert_eq!(buffer[510], 0.0);
    // ...and from then on gain 0 is exact (positive) zeros.
    let mut buffer = vec![-0.8f32; 512];
    stage.apply(&mut buffer, 2, shared.get());
    assert!(buffer.iter().all(|x| x.to_bits() == 0));
    let mut stateless = vec![-0.8f32, 0.5];
    apply_gain(&mut stateless, 0.0);
    assert!(stateless.iter().all(|x| x.to_bits() == 0));
    // Negative and non-finite gains are stored as 0.
    shared.set(-1.0);
    assert_eq!(shared.get(), 0.0);
    shared.set(f32::NAN);
    assert_eq!(shared.get(), 0.0);
}

// covers: S64
#[test]
fn a_gain_change_ramps_over_one_callback_then_holds() {
    let mut stage = GainStage::new();
    let mut ones = vec![1.0f32; 8];
    stage.apply(&mut ones, 2, 1.0);
    let mut ramp = vec![1.0f32; 8];
    stage.apply(&mut ramp, 2, 0.5);
    assert_eq!(ramp, [0.875, 0.875, 0.75, 0.75, 0.625, 0.625, 0.5, 0.5]);
    let mut held = vec![1.0f32; 8];
    stage.apply(&mut held, 2, 0.5);
    assert!(held.iter().all(|&x| x == 0.5));
    assert_eq!(stage.current(), 0.5);
}

// covers: S65, S123
#[test]
fn non_finite_values_never_reach_the_device() {
    let mut stage = GainStage::new();
    let mut buffer = vec![
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        0.5,
        f32::MAX,
        0.25,
    ];
    stage.apply(&mut buffer, 2, 4.0);
    assert_eq!(buffer, [0.0, 0.0, 0.0, 2.0, 0.0, 1.0]);
    let mut mono = vec![f32::NAN];
    stage.apply(&mut mono, 1, 1.0);
    assert_eq!(mono, [0.0]);
    let mut stateless = vec![f32::NAN, 0.5];
    apply_gain(&mut stateless, 1.0);
    assert_eq!(stateless, [0.0, 0.5]);
}

proptest! {
    // covers: S70
    #[test]
    fn the_curve_is_monotonic_and_inverts(a in 0.0f64..=100.0, b in 0.0f64..=100.0) {
        let (low, high) = if a <= b { (a, b) } else { (b, a) };
        prop_assert!(gain_for_position(low) <= gain_for_position(high));
        prop_assert!((position_for_gain(gain_for_position(a)) - a).abs() < 1e-9);
        prop_assert!((0.0..=1.0).contains(&gain_for_position(a)));
    }

    // covers: S64, S65
    #[test]
    fn the_gain_stage_output_is_finite_and_bounded_by_the_gains(
        samples in proptest::collection::vec(prop_oneof![8 => -1.0f32..=1.0, 1 => Just(f32::NAN), 1 => Just(f32::INFINITY)], 0..256),
        first in 0.0f32..=1.0,
        second in 0.0f32..=1.0,
    ) {
        let mut stage = GainStage::new();
        let mut a = samples.clone();
        stage.apply(&mut a, 2, first);
        let mut b = samples;
        stage.apply(&mut b, 2, second);
        let bound = first.max(second);
        prop_assert!(a.iter().chain(&b).all(|x| x.is_finite() && x.abs() <= bound));
    }
}
