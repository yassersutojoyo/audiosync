//! The verdicts `fake_tv_cells` prints, one line per dry-run cell.

#[path = "../examples/fake_tv/cells.rs"]
mod cells;

use std::time::Duration;

use cells::{Cell, Pause, Status, db_for_row, row_for_db};

#[test]
fn rows_map_to_the_default_line_and_back() {
    assert_eq!(db_for_row(0), -144.0);
    assert_eq!(db_for_row(20), -24.0);
    assert!((db_for_row(80) + 6.0).abs() < 1e-9);
    assert_eq!(db_for_row(100), 0.0);
    // A single-precision read just under the level still shows its row.
    assert_eq!(row_for_db(-24.000_001), 20);
    assert_eq!(row_for_db(-6.000_001), 80);
    assert_eq!(row_for_db(-26.7), 11);
    assert_eq!(row_for_db(-30.0), 0);
    assert_eq!(row_for_db(-144.0), 0);
    assert_eq!(row_for_db(f64::NAN), 0);
    assert_eq!(row_for_db(0.0), 100);
}

#[test]
fn a_line_is_the_cell_its_status_and_the_value() {
    let cell = Cell::new("D2", Status::Pass, "1 FLUSH".to_owned());
    assert_eq!(cell.line(), "D2        PASS    1 FLUSH");
    let manual = Cell::new("W1", Status::Manual, "on the TV".to_owned());
    assert_eq!(manual.line(), "W1        MANUAL  on the TV");
}

#[test]
fn k1_send_passes_when_every_row_lands_on_its_level() {
    let landed = [(20, Some(-24.0)), (50, Some(-15.0)), (0, Some(-144.0))];
    let cell = cells::k1_send(&landed);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(cell.value, "rows 20/50/0 -> fake at -24.0/-15.0/-144.0 dB");

    let off = cells::k1_send(&[(20, Some(-24.0)), (50, Some(-16.2))]);
    assert_eq!(off.status, Status::Fail);
    assert_eq!(
        off.value,
        "rows 20/50 -> fake at -24.0/-16.2 dB; row 50 shows 46"
    );
    let lost = cells::k1_send(&[(20, None)]);
    assert_eq!(lost.status, Status::Fail);
    assert_eq!(
        lost.value,
        "rows 20 -> fake at none dB; row 20 never reached the fake"
    );
}

#[test]
fn k1_read_passes_when_each_vol_shows_its_row_in_time() {
    let ms = Duration::from_millis;
    let seen = [(20, Some((20, ms(700)))), (100, Some((100, ms(1_100))))];
    let cell = cells::k1_read(&seen);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(
        cell.value,
        "vol rows 20/100 -> reported 20/100, slowest 1.1 s"
    );

    let slow = cells::k1_read(&[(20, Some((20, ms(2_400))))]);
    assert_eq!(slow.status, Status::Fail);
    assert_eq!(
        slow.value,
        "vol rows 20 -> reported 20, slowest 2.4 s; over 1.6 s"
    );
    let missing = cells::k1_read(&[(20, Some((20, ms(500)))), (50, None)]);
    assert_eq!(missing.status, Status::Fail);
    assert_eq!(
        missing.value,
        "vol rows 20/50 -> reported 20/none, slowest 0.5 s; row 50 never reported"
    );
}

#[test]
fn k1_press_passes_when_presses_are_reported_and_nothing_goes_back() {
    let ms = Duration::from_millis;
    let presses = [
        (-15.3, Some((-15.300_001, ms(800)))),
        (-15.6, Some((-15.6, ms(900)))),
    ];
    let cell = cells::k1_press(&presses, 0);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(
        cell.value,
        "2 remote presses -15.3/-15.6 dB reported, slowest 0.9 s; 0 volume sets back"
    );

    let echoed = cells::k1_press(&presses, 1);
    assert_eq!(echoed.status, Status::Fail);
    assert!(
        echoed.value.ends_with("1 volume sets back"),
        "{}",
        echoed.value
    );
    let unseen = cells::k1_press(&[(-15.3, None)], 0);
    assert_eq!(unseen.status, Status::Fail);
    assert!(
        unseen.value.contains("-15.3 dB never reported"),
        "{}",
        unseen.value
    );
}

#[test]
fn d1_passes_only_with_a_pin_at_every_switch_on() {
    let pass = cells::d1(2, 2, 2, 2, 2);
    assert_eq!(pass.status, Status::Pass, "{}", pass.line());
    assert_eq!(
        pass.value,
        "2 switch-ons: 2 PIN prompts, 2 PIN pairings started, 2 done, 2 playing"
    );
    assert_eq!(cells::d1(2, 1, 1, 1, 2).status, Status::Fail);
    assert_eq!(cells::d1(2, 2, 2, 2, 1).status, Status::Fail);
    assert_eq!(
        cells::d1(1, 1, 1, 1, 1).status,
        Status::Fail,
        "one switch-on proves nothing"
    );
}

#[test]
fn d2_passes_with_one_flush_and_nothing_else() {
    let clean = Pause {
        flushes: 1,
        teardowns: 0,
        setups: 0,
        records: 0,
        resumed: true,
    };
    let cell = cells::d2(clean);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(
        cell.value,
        "pause: 1 FLUSH, 0 TEARDOWN; resume: 0 SETUP, 0 RECORD, audio again"
    );
    for broken in [
        Pause {
            flushes: 2,
            ..clean
        },
        Pause {
            flushes: 0,
            ..clean
        },
        Pause {
            teardowns: 1,
            ..clean
        },
        Pause { setups: 1, ..clean },
        Pause {
            records: 1,
            ..clean
        },
        Pause {
            resumed: false,
            ..clean
        },
    ] {
        assert_eq!(
            cells::d2(broken).status,
            Status::Fail,
            "{}",
            cells::d2(broken).value
        );
    }
    assert!(
        cells::d2(Pause {
            resumed: false,
            ..clean
        })
        .value
        .ends_with("no audio")
    );
}

#[test]
fn d3_passes_when_track_changes_and_a_seek_reach_the_tv_without_a_flush() {
    let cell = cells::d3_track(3, 6, 0);
    assert_eq!(cell.status, Status::Pass, "{}", cell.line());
    assert_eq!(
        cell.value,
        "3 track info changes (2 tracks, 1 seek): 6 metadata sets, 0 FLUSH"
    );
    assert_eq!(cells::d3_track(3, 6, 1).status, Status::Fail);
}

#[test]
fn d3_fails_when_the_track_info_never_reaches_the_tv() {
    let none = cells::d3_track(3, 0, 0);
    assert_eq!(none.status, Status::Fail, "{}", none.line());
    assert_eq!(
        none.value,
        "3 track info changes (2 tracks, 1 seek): 0 metadata sets, 0 FLUSH; fewer sets than changes"
    );
    assert_eq!(cells::d3_track(3, 2, 0).status, Status::Fail);
}

#[test]
fn d4_says_how_long_it_paused_against_the_eleven_minutes() {
    let clean = Pause {
        flushes: 1,
        teardowns: 0,
        setups: 0,
        records: 0,
        resumed: true,
    };
    let full = cells::d4(Duration::from_secs(660), clean);
    assert_eq!(full.status, Status::Pass, "{}", full.line());
    assert_eq!(
        full.value,
        "paused 11.0 min: 1 FLUSH, 0 TEARDOWN, 0 SETUP, 0 RECORD, audio again"
    );
    let short = cells::d4(Duration::from_secs(60), clean);
    assert_eq!(short.status, Status::Pass);
    assert_eq!(
        short.value,
        "paused 1.0 min (short run, the cell asks 11): 1 FLUSH, 0 TEARDOWN, 0 SETUP, 0 RECORD, audio again"
    );
    let dropped = cells::d4(
        Duration::from_secs(660),
        Pause {
            teardowns: 1,
            ..clean
        },
    );
    assert_eq!(dropped.status, Status::Fail);
}

#[test]
fn manual_cells_name_their_step() {
    let manual = cells::manual();
    let ids: Vec<&str> = manual.iter().map(|cell| cell.id).collect();
    assert_eq!(
        ids,
        [
            "K1.row", "K2", "K3", "D3", "D4.cover", "D5", "D6", "W1", "W2", "W3"
        ]
    );
    assert!(manual.iter().all(|cell| cell.status == Status::Manual));
    assert!(manual.iter().all(|cell| !cell.value.is_empty()));
}
