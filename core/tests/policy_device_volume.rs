//! An output device's own volume and mute (S192 to S202): the decision a
//! reading leads to, the fix one click writes, and when a new reading is
//! worth a log line.
//!
//! The first test reproduces the report: a MacBook's speakers at 13 % of
//! their own volume sounded very quiet with AudioSync's slider at 100.

use audiosync_core::policy::device_volume::{
    Decision, DeviceVolume, FIX_VOLUME, Fix, Hint, LOW_BELOW_PERCENT, decision_change,
};

fn at(volume: f64) -> DeviceVolume {
    DeviceVolume {
        volume: Some(volume),
        muted: Some(false),
    }
}

fn muted_at(volume: f64) -> DeviceVolume {
    DeviceVolume {
        volume: Some(volume),
        muted: Some(true),
    }
}

// covers: S196
#[test]
fn a_device_at_13_or_25_percent_is_low_and_one_at_26_percent_is_fine() {
    let quiet = at(0.13);
    assert_eq!(quiet.percent(), Some(13));
    assert_eq!(quiet.decision(), Decision::Low);
    assert_eq!(quiet.hint(), Some(Hint::Low { percent: 13 }));

    assert_eq!(LOW_BELOW_PERCENT, 25);
    // 25 % itself is low: the hint names the percent it shows.
    assert_eq!(at(0.25).decision(), Decision::Low);
    assert_eq!(at(0.25).hint(), Some(Hint::Low { percent: 25 }));
    assert_eq!(at(0.25).fix().volume, Some(FIX_VOLUME));
    assert_eq!(at(0.26).decision(), Decision::Fine);
    assert_eq!(at(0.26).hint(), None);
    // The whole percent decides, rounded half away from zero: 25.4 % shows
    // as 25 % and is low, 25.5 % shows as 26 % and is fine.
    assert_eq!(at(0.254).percent(), Some(25));
    assert_eq!(at(0.254).decision(), Decision::Low);
    assert_eq!(at(0.2551).percent(), Some(26));
    assert_eq!(at(0.2551).decision(), Decision::Fine);
    // 0 % without mute is low; a third of the volume, kept low on purpose,
    // is left alone.
    assert_eq!(at(0.0).hint(), Some(Hint::Low { percent: 0 }));
    assert_eq!(at(0.33).decision(), Decision::Fine);
    assert_eq!(at(1.0).percent(), Some(100));
    // Readings outside 0 to 1 are clamped.
    assert_eq!(at(1.7).percent(), Some(100));
    assert_eq!(at(-0.2).hint(), Some(Hint::Low { percent: 0 }));
    // A volume known without its mute decides on its own.
    let volume_only = DeviceVolume {
        volume: Some(0.13),
        muted: None,
    };
    assert_eq!(volume_only.decision(), Decision::Low);
}

// covers: S196
#[test]
fn a_muted_device_is_muted_whatever_its_volume() {
    assert_eq!(muted_at(0.9).decision(), Decision::Muted);
    assert_eq!(muted_at(0.9).hint(), Some(Hint::Muted));
    assert_eq!(muted_at(0.26).hint(), Some(Hint::Muted));
    assert_eq!(
        muted_at(0.25).hint(),
        Some(Hint::MutedAndLow { percent: 25 })
    );
    assert_eq!(muted_at(0.13).decision(), Decision::MutedAndLow);
    assert_eq!(
        muted_at(0.13).hint(),
        Some(Hint::MutedAndLow { percent: 13 })
    );
    assert_eq!(muted_at(0.0).hint(), Some(Hint::MutedAndLow { percent: 0 }));
    // A mute known without the volume is muted.
    let mute_only = DeviceVolume {
        volume: None,
        muted: Some(true),
    };
    assert_eq!(mute_only.decision(), Decision::Muted);
    assert_eq!(mute_only.hint(), Some(Hint::Muted));
    let unmuted_only = DeviceVolume {
        volume: None,
        muted: Some(false),
    };
    assert_eq!(unmuted_only.decision(), Decision::Fine);
    assert_eq!(unmuted_only.hint(), None);
    for decision in [Decision::Low, Decision::Muted, Decision::MutedAndLow] {
        assert!(decision.shows_hint(), "{decision:?}");
    }
}

// covers: S196
#[test]
fn an_unknown_or_non_finite_volume_shows_nothing() {
    let unknown = DeviceVolume::UNKNOWN;
    assert_eq!(
        unknown,
        DeviceVolume {
            volume: None,
            muted: None
        }
    );
    assert_eq!(unknown.decision(), Decision::Unknown);
    assert_eq!(unknown.hint(), None);
    assert_eq!(unknown.percent(), None);
    assert_eq!(DeviceVolume::default(), unknown);
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(at(bad).percent(), None, "{bad}");
        // The mute is still known, and not set.
        assert_eq!(at(bad).decision(), Decision::Fine, "{bad}");
        assert_eq!(at(bad).hint(), None, "{bad}");
        assert_eq!(muted_at(bad).hint(), Some(Hint::Muted), "{bad}");
        let alone = DeviceVolume {
            volume: Some(bad),
            muted: None,
        };
        assert_eq!(alone.decision(), Decision::Unknown, "{bad}");
    }
    for decision in [Decision::Fine, Decision::Unknown] {
        assert!(!decision.shows_hint(), "{decision:?}");
    }
}

// covers: S197
#[test]
fn the_fix_raises_to_75_percent_only_below_25_and_never_lowers() {
    assert_eq!(FIX_VOLUME, 0.75);
    assert_eq!(
        at(0.13).fix(),
        Fix {
            volume: Some(0.75),
            unmute: false
        }
    );
    assert_eq!(at(0.0).fix().volume, Some(0.75));
    assert_eq!(at(0.244).fix().volume, Some(0.75));
    for fine in [0.25, 0.5, 0.9, 1.0] {
        assert_eq!(at(fine).fix(), Fix::NOTHING, "{fine}");
        assert!(at(fine).fix().is_empty(), "{fine}");
    }
    assert_eq!(DeviceVolume::UNKNOWN.fix(), Fix::NOTHING);
    assert_eq!(at(f64::NAN).fix(), Fix::NOTHING);
    // Whatever it writes is above the volume it fixes.
    for whole in 0..=100 {
        let reading = at(f64::from(whole) / 100.0);
        if let Some(volume) = reading.fix().volume {
            assert!(volume > reading.volume.unwrap(), "{whole} %");
        }
    }
}

// covers: S197
#[test]
fn the_fix_unmutes_without_touching_a_fine_volume() {
    assert_eq!(
        muted_at(0.9).fix(),
        Fix {
            volume: None,
            unmute: true
        }
    );
    assert_eq!(
        muted_at(0.13).fix(),
        Fix {
            volume: Some(0.75),
            unmute: true
        }
    );
    let mute_only = DeviceVolume {
        volume: None,
        muted: Some(true),
    };
    assert_eq!(
        mute_only.fix(),
        Fix {
            volume: None,
            unmute: true
        }
    );
    assert!(!muted_at(0.9).fix().is_empty());
}

// covers: S201
#[test]
fn a_reading_logs_only_when_its_decision_changes() {
    // The first reading is logged on a line of its own, not as a change.
    assert_eq!(decision_change(None, Decision::Fine), None);
    assert_eq!(decision_change(None, Decision::Low), None);
    // Dragging the system slider from 80 % to 30 % logs nothing, on to 20 %
    // one line, and on to 15 % nothing again.
    let mut last = at(0.8).decision();
    let mut lines = Vec::new();
    for reading in [at(0.3), at(0.2), at(0.15), muted_at(0.15), at(0.9)] {
        let now = reading.decision();
        if let Some(change) = decision_change(Some(last), now) {
            lines.push(change);
        }
        last = now;
    }
    assert_eq!(
        lines,
        [
            (Decision::Fine, Decision::Low),
            (Decision::Low, Decision::MutedAndLow),
            (Decision::MutedAndLow, Decision::Fine),
        ]
    );
    assert_eq!(decision_change(Some(Decision::Low), Decision::Low), None);
    // The log names each decision.
    assert_eq!(Decision::Fine.to_string(), "fine");
    assert_eq!(Decision::Low.to_string(), "low");
    assert_eq!(Decision::Muted.to_string(), "muted");
    assert_eq!(Decision::MutedAndLow.to_string(), "muted and low");
    assert_eq!(Decision::Unknown.to_string(), "unknown");
}
