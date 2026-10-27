//! The verdicts `fake_tv_cells` prints: one line per dry-run cell,
//! `<cell> PASS|FAIL|MANUAL <measured value>`, from what the run measured.
//! Shared with `tests/fake_tv_cells.rs`.

use std::time::Duration;

use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::volume::{MIN_DB, MUTED_DB};

/// A level set on the television shows on the row within this long, with
/// the host asking once a second (S291 of docs/ENGINE_SPEC.md).
pub const READ_WITHIN: Duration = Duration::from_millis(1600);
/// A remote press is reported when the read lies this close to it.
pub const PRESS_DB: f64 = 0.01;
/// How long the paused-cover cell asks the pause to last.
pub const COVER_PAUSE: Duration = Duration::from_secs(11 * 60);
/// Slack against arithmetic error before the row rounds down (S290).
const ROW_SLACK: f64 = 0.01;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Manual,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Manual => "MANUAL",
        }
    }
}

pub struct Cell {
    pub id: &'static str,
    pub status: Status,
    pub value: String,
}

impl Cell {
    pub fn new(id: &'static str, status: Status, value: String) -> Cell {
        Cell { id, status, value }
    }

    pub fn line(&self) -> String {
        format!("{:<9} {:<7} {}", self.id, self.status.word(), self.value)
    }

    fn judged(id: &'static str, problems: &[String], value: String) -> Cell {
        if problems.is_empty() {
            Cell::new(id, Status::Pass, value)
        } else {
            Cell::new(
                id,
                Status::Fail,
                format!("{value}; {}", problems.join(", ")),
            )
        }
    }
}

/// What one pause and its resume sent, counted from the pause on.
#[derive(Clone, Copy, Debug)]
pub struct Pause {
    pub flushes: usize,
    pub teardowns: usize,
    pub setups: usize,
    pub records: usize,
    /// Audio reached the television again after the resume.
    pub resumed: bool,
}

/// The dB the host sends for an AirPlay row (S289's default line); the
/// airplay crate can't reach the core's copy, which depends on it.
pub fn db_for_row(row: u32) -> f64 {
    if row == 0 {
        MUTED_DB
    } else {
        MIN_DB + 0.3 * f64::from(row.min(100))
    }
}

/// The number a row shows for a level read as `db` (S289, S290).
pub fn row_for_db(db: f64) -> u32 {
    if !db.is_finite() || db <= MIN_DB {
        return 0;
    }
    (100.0 * (db - MIN_DB) / -MIN_DB + ROW_SLACK)
        .floor()
        .clamp(0.0, 100.0) as u32
}

fn joined<T>(items: &[T], show: impl Fn(&T) -> String) -> String {
    items.iter().map(show).collect::<Vec<_>>().join("/")
}

fn slowest(times: impl Iterator<Item = Duration>) -> Duration {
    times.max().unwrap_or_default()
}

/// K1, app to television: each row the app set, with the level the fake
/// television took from it.
pub fn k1_send(landed: &[(u32, Option<f64>)]) -> Cell {
    let mut problems = Vec::new();
    for &(row, level) in landed {
        match level {
            None => problems.push(format!("row {row} never reached the fake")),
            Some(db) if row_for_db(db) != row => {
                problems.push(format!("row {row} shows {}", row_for_db(db)))
            }
            Some(_) => {}
        }
    }
    let value = format!(
        "rows {} -> fake at {} dB",
        joined(landed, |(row, _)| row.to_string()),
        joined(landed, |(_, level)| level
            .map_or("none".to_owned(), |db| format!("{db:.1}")))
    );
    Cell::judged("K1.send", &problems, value)
}

/// K1, television to app: each row set with `vol`, with the row the
/// sender's first matching report shows and how long after `vol` it came.
pub fn k1_read(seen: &[(u32, Option<(u32, Duration)>)]) -> Cell {
    let mut problems = Vec::new();
    for &(row, report) in seen {
        match report {
            None => problems.push(format!("row {row} never reported")),
            Some((shown, _)) if shown != row => {
                problems.push(format!("row {row} reported as {shown}"))
            }
            Some(_) => {}
        }
    }
    let slow = slowest(
        seen.iter()
            .filter_map(|(_, report)| report.map(|(_, at)| at)),
    );
    if slow > READ_WITHIN {
        problems.push(format!("over {:.1} s", READ_WITHIN.as_secs_f64()));
    }
    let value = format!(
        "vol rows {} -> reported {}, slowest {:.1} s",
        joined(seen, |(row, _)| row.to_string()),
        joined(seen, |(_, report)| report
            .map_or("none".to_owned(), |(shown, _)| shown.to_string())),
        slow.as_secs_f64()
    );
    Cell::judged("K1.read", &problems, value)
}

/// K1, remote presses: each level pressed to, with the matching report and
/// its delay, and the volume sets the sender sent after the first press.
pub fn k1_press(presses: &[(f64, Option<(f64, Duration)>)], sets_back: usize) -> Cell {
    let mut problems = Vec::new();
    for &(pressed, report) in presses {
        let matched =
            report.is_some_and(|(db, at)| (db - pressed).abs() <= PRESS_DB && at <= READ_WITHIN);
        if !matched {
            problems.push(format!("{pressed:.1} dB never reported"));
        }
    }
    let slow = slowest(
        presses
            .iter()
            .filter_map(|(_, report)| report.map(|(_, at)| at)),
    );
    let value = format!(
        "{} remote presses {} dB reported, slowest {:.1} s; {sets_back} volume sets back",
        presses.len(),
        joined(presses, |(db, _)| format!("{db:.1}")),
        slow.as_secs_f64()
    );
    if sets_back > 0 {
        return Cell::new("K1.press", Status::Fail, value);
    }
    Cell::judged("K1.press", &problems, value)
}

/// D1, a PIN every time: per switch-on, the sender asks for the PIN, the
/// fake sees a PIN pairing start and finish, and the speaker plays.
pub fn d1(switch_ons: usize, prompts: usize, started: usize, done: usize, playing: usize) -> Cell {
    let value = format!(
        "{switch_ons} switch-ons: {prompts} PIN prompts, {started} PIN pairings started, {done} done, {playing} playing"
    );
    let every = [prompts, started, done, playing]
        .iter()
        .all(|&count| count == switch_ons);
    let status = if switch_ons >= 2 && every {
        Status::Pass
    } else {
        Status::Fail
    };
    Cell::new("D1", status, value)
}

/// What one add of the password television led to, counted from the add on.
#[derive(Clone, Copy, Debug, Default)]
pub struct PasswordStep {
    /// The first error the speaker reported.
    pub error: Option<ErrorKind>,
    pub playing: bool,
    /// The host got HomeKit credentials.
    pub paired: bool,
    pub pair_setups: usize,
    /// Pair-setup M1 without the transient flag, and M5.
    pub started: usize,
    pub done: usize,
    pub pin_starts: usize,
    pub verifies: usize,
}

fn password_value(step: PasswordStep) -> String {
    let outcome = match (step.error, step.playing) {
        (Some(kind), _) => format!("error {kind:?}"),
        (None, true) => "playing".to_owned(),
        (None, false) => "no outcome".to_owned(),
    };
    format!(
        "{outcome}, {} pair-setup ({} M1, {} M5), {} pin-start, {} pair-verify",
        step.pair_setups, step.started, step.done, step.pin_starts, step.verifies
    )
}

/// The password cells, from four adds in order: with no password, a wrong
/// one, the right one, and again with the credentials the right one gave.
pub fn pw(
    none: PasswordStep,
    wrong: PasswordStep,
    right: PasswordStep,
    again: PasswordStep,
) -> [Cell; 4] {
    [
        ("PW.none", none, pw_none(none)),
        ("PW.wrong", wrong, pw_wrong(wrong)),
        ("PW.right", right, pw_right(right)),
        ("PW.again", again, pw_again(again)),
    ]
    .map(|(id, step, problems)| Cell::judged(id, &problems, password_value(step)))
}

fn no_pin_start(step: PasswordStep, problems: &mut Vec<String>) {
    if step.pin_starts > 0 {
        problems.push("a PIN pairing was started".to_owned());
    }
}

fn pw_none(step: PasswordStep) -> Vec<String> {
    let mut problems = Vec::new();
    if step.error != Some(ErrorKind::PasswordRequired) {
        problems.push("PasswordRequired expected".to_owned());
    }
    if step.pair_setups > 0 {
        problems.push("pair-setup without a password".to_owned());
    }
    no_pin_start(step, &mut problems);
    problems
}

fn pw_wrong(step: PasswordStep) -> Vec<String> {
    let mut problems = Vec::new();
    if step.error != Some(ErrorKind::WrongPassword) {
        problems.push("WrongPassword expected".to_owned());
    }
    // P151 retries M1 once when B leads with a zero byte or S is short.
    if !(1..=2).contains(&step.started) {
        problems.push("1 or 2 M1 expected".to_owned());
    }
    if step.done > 0 {
        problems.push("M5 after a wrong password".to_owned());
    }
    no_pin_start(step, &mut problems);
    problems
}

fn pw_right(step: PasswordStep) -> Vec<String> {
    let mut problems = Vec::new();
    if !step.playing || step.error.is_some() {
        problems.push("never played".to_owned());
    }
    if step.started != 1 || step.done != 1 {
        problems.push("one password pairing expected".to_owned());
    }
    if step.verifies == 0 {
        problems.push("no pair-verify".to_owned());
    }
    if !step.paired {
        problems.push("no credentials reached the host".to_owned());
    }
    no_pin_start(step, &mut problems);
    problems
}

fn pw_again(step: PasswordStep) -> Vec<String> {
    let mut problems = Vec::new();
    if !step.playing || step.error.is_some() {
        problems.push("never played".to_owned());
    }
    if step.pair_setups > 0 {
        problems.push("paired again".to_owned());
    }
    if step.verifies == 0 {
        problems.push("no pair-verify".to_owned());
    }
    no_pin_start(step, &mut problems);
    problems
}

fn pause_counts(pause: Pause) -> String {
    format!(
        "{} FLUSH, {} TEARDOWN, {} SETUP, {} RECORD, {}",
        pause.flushes,
        pause.teardowns,
        pause.setups,
        pause.records,
        if pause.resumed {
            "audio again"
        } else {
            "no audio"
        }
    )
}

fn pause_clean(pause: Pause) -> bool {
    pause.flushes == 1
        && pause.teardowns == 0
        && pause.setups == 0
        && pause.records == 0
        && pause.resumed
}

/// D2, the app's pause and play: one FLUSH, no TEARDOWN, and a resume with
/// no new SETUP or RECORD.
pub fn d2(pause: Pause) -> Cell {
    let value = format!(
        "pause: {} FLUSH, {} TEARDOWN; resume: {} SETUP, {} RECORD, {}",
        pause.flushes,
        pause.teardowns,
        pause.setups,
        pause.records,
        if pause.resumed {
            "audio again"
        } else {
            "no audio"
        }
    );
    let status = if pause_clean(pause) {
        Status::Pass
    } else {
        Status::Fail
    };
    Cell::new("D2", status, value)
}

/// D3, the sender's part: new track info, one of them a seek, reaches the
/// television (`seen` metadata SET_PARAMETERs, at least one per change, and
/// `track_text` of them DMAP track text, also one per change) and sends no
/// FLUSH. Progress alone must not pass for track info.
pub fn d3_track(changes: usize, seen: usize, track_text: usize, flushes: usize) -> Cell {
    let value = format!(
        "{changes} track info changes ({} tracks, 1 seek): {seen} metadata sets, {track_text} track text, {flushes} FLUSH",
        changes.saturating_sub(1)
    );
    let short = if seen < changes {
        Some("fewer sets than changes")
    } else if track_text < changes {
        Some("fewer track text sets than changes")
    } else {
        None
    };
    if let Some(short) = short {
        return Cell::new("D3.track", Status::Fail, format!("{value}; {short}"));
    }
    let status = if flushes == 0 {
        Status::Pass
    } else {
        Status::Fail
    };
    Cell::new("D3.track", status, value)
}

/// D4, the sender's part: a long pause keeps the session, and play goes
/// on after it. A pause shorter than the cell asks is said in the value.
pub fn d4(paused: Duration, pause: Pause) -> Cell {
    let minutes = paused.as_secs_f64() / 60.0;
    let short = if paused < COVER_PAUSE {
        format!(" (short run, the cell asks {})", COVER_PAUSE.as_secs() / 60)
    } else {
        String::new()
    };
    let value = format!("paused {minutes:.1} min{short}: {}", pause_counts(pause));
    let status = if pause_clean(pause) {
        Status::Pass
    } else {
        Status::Fail
    };
    Cell::new("D4", status, value)
}

/// The cells only a person with the app, a player or the real television
/// can judge, each with its step.
pub fn manual() -> Vec<Cell> {
    [
        (
            "K1.row",
            "Mac app with fake_tv --summary: row 20 shows 20; vol -30/-24/-15/-6/0 shows 0/20/50/80/100 within about 1 s; no ECHO or OVERWRITE line",
        ),
        (
            "K2",
            "this run and the Mac cells clean by Friday night; the weekend cells only measure",
        ),
        (
            "K3",
            "PC, outside gaming: cargo test in every crate and bash scripts/pc_test.sh raise no Firewall prompt",
        ),
        (
            "D3",
            "Spotify pause in Spotify (and once under All System Audio): one FLUSH; track change and held seek in Music: no FLUSH",
        ),
        (
            "D4.cover",
            "Mac app: pause 11 min with the debug log on; no \"Now Playing: dropped\" line",
        ),
        (
            "D5",
            "Mac app: a failed switch notice clears when the playing source is picked again",
        ),
        (
            "D6",
            "Mac app: grep -iE \"error|warn\" /tmp/audiosync-debug.log shows only the expected 500 volume lines",
        ),
        (
            "W1",
            "real TV at master 63: TV 20 by its remote reads 20 in AudioSync and AudioSync 20 reads 20 on the TV, within about 1 s; remote presses stay put",
        ),
        (
            "W2",
            "real TV: a pause from its remote or the app is quiet within 1 s, a Spotify pause within 2 s; resume clean",
        ),
        ("W3", "real TV: the paused cover stays 11 min"),
        (
            "W4",
            "real TV: TV 80 by its remote at master 63, master to 64: TV and row 81; master 100 then back to 63: 100, then 80 again",
        ),
        (
            "W5",
            "real TV: TV 40 by its remote at master 63, stop and start streaming: TV and row stay 40; same after Calibrate All",
        ),
        (
            "W6",
            "real TV: switch the source while it plays: no PIN prompt, no SETUP or GET timeout, the gap no longer than a track change",
        ),
    ]
    .into_iter()
    .map(|(id, step)| Cell::new(id, Status::Manual, step.to_owned()))
    .collect()
}
