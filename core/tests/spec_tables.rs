//! The data tables in the code match the tables in `docs/ENGINE_SPEC.md`: the
//! equalizer presets (S33), the volume reference values (S70) and the
//! hold-backs of the local delay model (S176).

use audiosync_core::eq::{BAND_CENTERS_HZ, PRESETS};
use audiosync_core::policy::delay::{Participant, hold_backs};
use audiosync_core::volume::{gain_for_position, gain_to_db};

fn spec() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("docs")
        .join("ENGINE_SPEC.md");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The cells of every row of the Markdown table whose header row starts with
/// `first_header`, header row included, separator row left out.
fn table(spec: &str, first_header: &str) -> Vec<Vec<String>> {
    let cells = |line: &str| -> Vec<String> {
        line.trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().replace('\u{2212}', "-"))
            .collect()
    };
    let mut lines = spec.lines().skip_while(|line| {
        let row = cells(line);
        !(line.trim_start().starts_with('|')
            && row.first().map(String::as_str) == Some(first_header))
    });
    let mut rows = Vec::new();
    if let Some(header) = lines.next() {
        rows.push(cells(header));
    }
    for line in lines {
        if !line.trim_start().starts_with('|') {
            break;
        }
        if line.contains("---") {
            continue;
        }
        rows.push(cells(line));
    }
    rows
}

fn number(cell: &str) -> f64 {
    match cell {
        "-∞" => f64::NEG_INFINITY,
        _ => cell
            .parse()
            .unwrap_or_else(|_| panic!("not a number in the spec table: {cell:?}")),
    }
}

fn frequency(cell: &str) -> f64 {
    match cell.strip_suffix('k') {
        Some(kilo) => number(kilo) * 1000.0,
        None => number(cell),
    }
}

// covers: S22, S33
#[test]
fn the_preset_table_matches_the_spec() {
    let spec = spec();
    let rows = table(&spec, "Preset");
    assert_eq!(rows.len(), 1 + PRESETS.len(), "one row per preset");
    let centers: Vec<f64> = rows[0][1..].iter().map(|cell| frequency(cell)).collect();
    assert_eq!(
        centers, BAND_CENTERS_HZ,
        "the table's columns are the band centers"
    );
    for (row, preset) in rows[1..].iter().zip(PRESETS.iter()) {
        assert_eq!(row[0], preset.id, "menu order");
        let gains: Vec<f64> = row[1..].iter().map(|cell| number(cell)).collect();
        assert_eq!(gains, preset.gains_db, "{}", preset.id);
    }
}

// covers: S70
#[test]
fn the_volume_reference_values_match_the_spec() {
    let spec = spec();
    let rows = table(&spec, "p");
    assert_eq!(rows.len(), 3, "p, g and dB rows");
    assert_eq!(rows[1][0], "g");
    assert_eq!(rows[2][0], "dB");
    let columns = rows[0][1..].iter().zip(&rows[1][1..]).zip(&rows[2][1..]);
    assert_eq!(columns.len(), 9, "nine reference positions");
    for ((position, gain), db) in columns {
        let (position, gain, db) = (number(position), number(gain), number(db));
        let computed = gain_for_position(position);
        assert!(
            (computed - gain).abs() <= gain * 1e-9,
            "g({position}) = {computed}, spec {gain}"
        );
        let computed_db = gain_to_db(computed);
        if db.is_infinite() {
            assert_eq!(computed_db, db);
        } else {
            assert!(
                (computed_db - db).abs() <= 0.005,
                "dB at {position}: {computed_db}, spec {db}"
            );
        }
    }
}

// covers: S176
#[test]
fn the_hold_back_table_matches_the_spec() {
    // The table's outputs: the reference, a USB speaker at +150 ms and a
    // Bluetooth speaker at −250 ms, with L = 2000 ms while AirPlay is chosen.
    let outputs = [
        ("R", 0.0, true),
        ("USB", 150.0, false),
        ("Bluetooth", -250.0, false),
    ];
    let spec = spec();
    let rows = table(&spec, "Taking part");
    assert_eq!(
        rows[0][3..],
        ["h(R)", "h(USB)", "h(Bluetooth)"],
        "one column per output"
    );
    assert_eq!(rows.len(), 8, "seven cases");
    for row in &rows[1..] {
        let airplay = match row[1].as_str() {
            "yes" => Some(2000.0),
            "no" => None,
            other => panic!("AirPlay chosen is yes or no, not {other:?}"),
        };
        let taking_part: Vec<Participant> = outputs
            .iter()
            .zip(&row[3..])
            .filter(|(_, cell)| cell.as_str() != "–")
            .map(|(&(id, offset_ms, is_reference), _)| Participant {
                id,
                offset_ms,
                is_reference,
            })
            .collect();
        let expected: Vec<f64> = row[3..]
            .iter()
            .filter(|cell| cell.as_str() != "–")
            .map(|cell| number(cell))
            .collect();
        let held: Vec<f64> = hold_backs(&taking_part, airplay)
            .iter()
            .map(|(_, held)| *held)
            .collect();
        assert_eq!(held, expected, "{}", row[0]);
        // h(x) = o′(x) − m, and every o′ in the table is its stored offset.
        assert_eq!(
            taking_part[0].offset_ms - held[0],
            number(&row[2]),
            "m for {}",
            row[0]
        );
    }
}
