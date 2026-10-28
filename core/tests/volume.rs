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

mod airplay_device_level {
    use audiosync_core::volume::{
        ReadBack, device_level, device_level_for_db, level_set_on_speaker, own_for_device_level,
        row_level, row_move,
    };

    /// The row a speaker at own volume `own` shows through `zone` and `master`.
    fn row(own: f64, zone: f64, master: f64) -> f64 {
        row_level(device_level(own, zone, master))
    }

    // covers: S289
    #[test]
    fn a_reading_maps_linearly_onto_0_to_100() {
        assert_eq!(device_level_for_db(-24.0).map(row_level), Some(20.0));
        assert_eq!(device_level_for_db(-15.0), Some(50.0));
        assert_eq!(device_level_for_db(0.0), Some(100.0));
        assert_eq!(device_level_for_db(-30.0), Some(0.0));
        assert_eq!(device_level_for_db(-144.0), Some(0.0));
        assert_eq!(device_level_for_db(f64::NAN), None);
    }

    // covers: S290
    #[test]
    fn the_row_shows_the_device_level_rounded_down() {
        assert_eq!(row(50.0, 100.0, 63.0), 31.0);
        assert_eq!(row(90.0, 100.0, 63.0), 56.0);
        assert_eq!(row(100.0, 100.0, 63.0), 63.0);
        assert_eq!(row(100.0, 100.0, 100.0), 100.0);
        assert_eq!(row(0.0, 100.0, 63.0), 0.0);
        // Float error just under a whole number still shows it.
        assert_eq!(row_level(19.999_999_999), 20.0);
        assert_eq!(row_level(f64::NAN), 0.0);
    }

    /// Whether `a` and `b` agree within float error.
    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // covers: S290
    #[test]
    fn a_row_move_sends_the_row_and_stores_the_row_over_k() {
        let moved = row_move(20.0, 100.0, 63.0).expect("k above 0");
        assert_eq!(moved.device_level, 20.0);
        assert!(near(moved.own_position, 20.0 / 0.63), "{moved:?}");
        assert_eq!(row_level(moved.device_level), 20.0);
        // The row never jumps on release, at any row and master.
        for master in [1.0, 7.0, 33.0, 63.0, 99.0, 100.0] {
            for target in 0..=100 {
                let r = f64::from(target);
                let moved = row_move(r, 100.0, master).expect("k above 0");
                assert_eq!(moved.device_level, r);
                let shown = row(moved.own_position, 100.0, master);
                assert_eq!(shown, r, "master {master} row {r}");
            }
        }
    }

    // covers: S290
    #[test]
    fn the_master_and_the_zone_are_no_ceiling_for_a_row_move() {
        let moved = row_move(80.0, 100.0, 63.0).expect("k above 0");
        assert!(near(moved.own_position, 80.0 / 0.63), "{moved:?}");
        assert_eq!(moved.device_level, 80.0);
        let moved = row_move(100.0, 100.0, 63.0).expect("k above 0");
        assert_eq!(moved.device_level, 100.0);
        assert_eq!(row(moved.own_position, 100.0, 63.0), 100.0);
        // Through a zone at 50 and the master at 63, row 40 still sends 40.
        let moved = row_move(40.0, 50.0, 63.0).expect("k above 0");
        assert_eq!(moved.device_level, 40.0);
        assert_eq!(row(moved.own_position, 50.0, 63.0), 40.0);
    }

    // covers: S290
    #[test]
    fn with_k_at_0_a_row_move_sends_nothing() {
        assert_eq!(row_move(40.0, 100.0, 0.0), None);
        assert_eq!(row_move(40.0, 0.0, 100.0), None);
        assert_eq!(row_move(f64::NAN, 100.0, 100.0), None);
        assert_eq!(own_for_device_level(20.0, 100.0, 0.0), None);
    }

    // covers: S290
    #[test]
    fn a_master_move_keeps_the_own_volume_and_caps_the_level_at_100() {
        let moved = row_move(20.0, 100.0, 63.0).expect("k above 0");
        assert_eq!(row(moved.own_position, 100.0, 63.0), 20.0);
        assert_eq!(row(moved.own_position, 100.0, 100.0), 31.0);
        assert_eq!(row(moved.own_position, 100.0, 50.0), 15.0);
        // Past the cap and back loses nothing.
        let own = own_for_device_level(80.0, 100.0, 63.0).expect("k above 0");
        assert_eq!(device_level(own, 100.0, 100.0), 100.0);
        assert!(near(device_level(own, 100.0, 63.0), 80.0));
        assert_eq!(device_level(f64::NAN, 100.0, 63.0), 0.0);
        assert_eq!(device_level(-5.0, 100.0, 63.0), 0.0);
    }

    // covers: S290
    #[test]
    fn a_level_set_on_the_speaker_sets_the_row_and_the_own_volume() {
        let level = device_level_for_db(-24.0).expect("finite");
        assert_eq!(row_level(level), 20.0);
        let own = own_for_device_level(level, 100.0, 63.0).expect("k above 0");
        assert!(near(own, 20.0 / 0.63));
        let own = own_for_device_level(80.0, 100.0, 63.0).expect("k above 0");
        assert!(near(own, 80.0 / 0.63), "no clamp at 100: {own}");
        let moved = level_set_on_speaker(-24.0, 50.0, 31.5, 100.0, 63.0).expect("changed");
        assert!(near(moved.device_level, 20.0));
        assert!(near(moved.own_position, 20.0 / 0.63));
        // Zone × master at 0 keeps the own volume and still shows the level.
        assert_eq!(
            level_set_on_speaker(-24.0, 50.0, 0.0, 100.0, 0.0).map(|moved| moved.own_position),
            Some(50.0)
        );
        // The same reading again changes nothing.
        assert_eq!(
            level_set_on_speaker(-24.0, moved.own_position, moved.device_level, 100.0, 63.0),
            None
        );
        assert_eq!(
            level_set_on_speaker(f64::NAN, 32.0, 20.0, 100.0, 63.0),
            None
        );
        // A row showing 20.6 moves to 21.
        let moved = level_set_on_speaker(-23.7, 20.6, 20.6, 100.0, 100.0).expect("changed");
        assert!(near(moved.own_position, 21.0), "{moved:?}");
        assert_eq!(row_level(moved.device_level), 21.0);
    }

    // covers: S290, S275, S276
    #[test]
    fn the_shared_s290_vectors_hold_through_the_c_interface() {
        use audiosync_core::ffi::{
            asc_airplay_device_level, asc_airplay_row_level, asc_airplay_row_move,
        };
        use audiosync_core::policy::presets::{Preset, PresetMember};
        use audiosync_core::policy::remote::{Action, Command, Event, OwnVolume, Remote};
        use audiosync_core::volume::{level_on_speaker, sanitize_own};
        use serde_json::Value;

        let text = include_str!("vectors/s290_volume.json");
        let doc: Value = serde_json::from_str(text).expect("valid JSON");
        let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_f64);
        let cases = doc["cases"].as_array().expect("cases");
        assert!(cases.len() >= 30, "the cases and the sweep");
        let row_move = |row: f64, zone: f64, master: f64| {
            let (mut stored, mut sent) = (f64::NAN, f64::NAN);
            // SAFETY: both point to live doubles.
            unsafe { asc_airplay_row_move(row, zone, master, &mut stored, &mut sent) }
                .then_some((stored, sent))
        };
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let zone = number(case, "zone").expect("zone");
            let mut master = number(case, "master").expect("master");
            let mut own = 0.0;
            let mut level = 0.0;
            let mut remote = Remote::default();
            let mut at = 0.0;
            // A remote command as both hosts hand it to the core (S273).
            let mut decide = |command: Command, level: f64, reported: Option<f64>| {
                at += 1.0;
                let event = Event {
                    speaker: "Kitchen",
                    row: Some("kitchen"),
                    member: false,
                    command,
                    app: None,
                    volume: OwnVolume {
                        level,
                        dragging: false,
                        echo: false,
                        reported,
                    },
                    at,
                };
                match remote.decide(&event).action {
                    Action::Move { to, .. } => Some(to),
                    _ => None,
                }
            };
            for step in case["steps"].as_array().expect("steps") {
                let sent_expected = step.get("sent").and_then(Value::as_bool).unwrap_or(true);
                let mut sent = true;
                if let Some(value) = number(step, "own") {
                    own = value;
                    level = asc_airplay_device_level(own, zone, master);
                } else if let Some(value) = number(step, "stored_own") {
                    // As a preset keeps an AirPlay member's own volume (S251).
                    let preset = Preset {
                        id: "p".to_owned(),
                        name: "p".to_owned(),
                        members: vec![PresetMember {
                            key: "ap:kitchen".to_owned(),
                            volume: Some(value),
                        }],
                        zones: Vec::new(),
                        master,
                        gains_db: [0.0; 10],
                    }
                    .sanitized();
                    own = sanitize_own(value);
                    assert_eq!(preset.members[0].volume, Some(own), "{name}: preset");
                    level = asc_airplay_device_level(own, zone, master);
                } else if let Some(value) = number(step, "master") {
                    master = value;
                    level = asc_airplay_device_level(own, zone, master);
                } else if let Some(row) = number(step, "row_move") {
                    match row_move(row, zone, master) {
                        Some((stored, to)) => (own, level) = (stored, to),
                        None => sent = false,
                    }
                } else if let Some(by) = number(step, "step") {
                    let command = if by > 0.0 {
                        Command::VolumeUp
                    } else {
                        Command::VolumeDown
                    };
                    match decide(command, level, None).and_then(|to| row_move(to, zone, master)) {
                        Some((stored, to)) => (own, level) = (stored, to),
                        None => sent = false,
                    }
                } else if let Some(value) = number(step, "device_volume") {
                    let db = -30.0 + 0.3 * value;
                    match decide(Command::DeviceVolume(db), level, Some(value)) {
                        Some(to) => {
                            let moved = level_on_speaker(to, own, zone, master);
                            (own, level) = (moved.own_position, moved.device_level);
                        }
                        None => sent = false,
                    }
                } else if let Some(value) = number(step, "tv_level") {
                    let db = -30.0 + 0.3 * value;
                    if let Some(moved) = level_set_on_speaker(db, own, level, zone, master) {
                        own = moved.own_position;
                        level = moved.device_level;
                    }
                } else {
                    panic!("{name}: unknown step {step}");
                }
                assert_eq!(sent, sent_expected, "{name}: {step} sent");
                if let Some(expected) = number(step, "level") {
                    assert!(
                        (level - expected).abs() < 1e-6,
                        "{name}: {step} gave level {level}"
                    );
                }
                if let Some(expected) = number(step, "own_after") {
                    assert!(
                        (own - expected).abs() < 1e-6,
                        "{name}: {step} gave own {own}"
                    );
                }
                if let Some(expected) = number(step, "row") {
                    assert_eq!(asc_airplay_row_level(level), expected, "{name}: {step}");
                }
            }
        }
    }

    // covers: S291
    #[test]
    fn the_first_reading_near_a_level_sent_is_its_echo_and_the_baseline() {
        let mut read = ReadBack::default();
        read.sent(-23.7);
        assert_eq!(read.echo_of(), Some(-23.7));
        // The 0.6 dB grid keeps level 21 (−23.7) as −24: the baseline, no change.
        assert!(!read.set_on_speaker(-24.0));
        assert_eq!(read.echo_of(), None);
        assert!(!read.set_on_speaker(-24.03));
        assert!(!read.set_on_speaker(f64::NAN));
        // One remote press away from the baseline is a change and the new baseline.
        assert!(read.set_on_speaker(-23.7));
        assert!(!read.set_on_speaker(-23.7));
        // More than 0.5 dB from a level just sent is a change at once.
        read.sent(-24.0);
        assert!(read.set_on_speaker(-23.4));
    }

    // covers: S291
    #[test]
    fn one_step_inside_the_echo_window_is_a_change_on_the_speaker() {
        let mut read = ReadBack::default();
        // The row at 50 sent −15 dB; the remote pressed down once before the
        // first read holds level 49.
        read.sent(-15.0);
        assert!(read.set_on_speaker(-15.3));
        assert!(!read.set_on_speaker(-15.3));
        // A reading on the row sent is still its echo.
        read.sent(-15.0);
        assert!(!read.set_on_speaker(-14.98));
    }
}

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
