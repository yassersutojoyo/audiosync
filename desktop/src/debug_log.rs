//! The debug log, the Windows counterpart of the macOS app's
//! `AUDIOSYNC_DEBUG=1` log: a plain-text record of what the app did, for
//! whoever debugs it on a real PC.
//!
//! It is off unless the environment sets `AUDIOSYNC_DEBUG` to exactly `1`.
//! Then the app empties `audiosync-debug.log` in the temporary folder
//! (`%TEMP%\audiosync-debug.log`; on Linux `$XDG_STATE_HOME/audiosync/debug.log`)
//! at launch and appends one line per event,
//! each starting with the time in UTC, to the millisecond
//! (`2026-10-18T17:52:13.456Z`), up to [`MAX_BYTES`].
//!
//! A message is built only while the log is on: [`DebugLog::log`] takes a
//! closure. The thread that logs never touches the file: lines go over a
//! channel to one writer thread, which appends them and flushes whenever the
//! channel runs empty, so the file stays complete when the app is killed.
//! Only the UI thread logs. The capture and render threads never do (spec
//! S131): the UI thread logs what they report, and reads the counters they
//! keep for a status line every few seconds while streaming.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The environment variable that turns the log on when it is `1`.
pub const ENV_VAR: &str = "AUDIOSYNC_DEBUG";

/// The log's file name, in the temporary folder.
pub const FILE_NAME: &str = "audiosync-debug.log";

/// The most the log grows to, so a PC that streams for days doesn't fill
/// its disk (the status line alone adds a few MB a day).
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// What the writer thread is asked to do.
enum Request {
    /// Append a message, logged at this time.
    Line(SystemTime, String),
    /// Flush the file, then answer.
    Flush(Sender<()>),
}

/// The debug log, or a log that writes nothing (the default). Clones write
/// to the same file, in the order they log.
#[derive(Clone, Default)]
pub struct DebugLog {
    writer: Option<Sender<Request>>,
}

impl DebugLog {
    /// The log the environment asks for: `audiosync-debug.log` in the
    /// temporary folder when `AUDIOSYNC_DEBUG` is `1`, nothing otherwise.
    pub fn from_env() -> DebugLog {
        let (path, warning) = env_path();
        let log = DebugLog::new(path);
        if let Some(warning) = warning {
            log.log(|| warning);
        }
        log
    }

    /// [`DebugLog::from_env`] that keeps what the file holds: for a second
    /// launch, which adds its line to the running copy's log.
    #[cfg_attr(
        not(any(windows, target_os = "linux")),
        expect(dead_code, reason = "only Windows and Linux hand over")
    )]
    pub fn appending_from_env() -> DebugLog {
        DebugLog::open(env_path().0, MAX_BYTES, false)
    }

    /// A log written to `path`, emptied first, or one that writes nothing
    /// for `None` and when the file can't be created.
    pub fn new(path: Option<PathBuf>) -> DebugLog {
        DebugLog::with_limit(path, MAX_BYTES)
    }

    /// [`DebugLog::new`] that stops growing at `max_bytes` instead of
    /// [`MAX_BYTES`].
    fn with_limit(path: Option<PathBuf>, max_bytes: u64) -> DebugLog {
        DebugLog::open(path, max_bytes, true)
    }

    /// A log written to `path`, emptied first when `empty` is set.
    fn open(path: Option<PathBuf>, max_bytes: u64, empty: bool) -> DebugLog {
        // Emptied, then reopened for appending: every write lands at the
        // end even if something else truncates or writes the file meanwhile.
        let file = path.and_then(|path| {
            if empty {
                File::create(&path).ok()?;
            }
            OpenOptions::new().append(true).create(true).open(path).ok()
        });
        let Some(file) = file else {
            return DebugLog::default();
        };
        let (writer, requests) = mpsc::channel();
        let started = thread::Builder::new()
            .name("audiosync-debug-log".to_owned())
            .spawn(move || write_requests(&requests, file, max_bytes));
        DebugLog {
            writer: started.ok().map(|_| writer),
        }
    }

    /// Whether lines reach a file.
    pub fn enabled(&self) -> bool {
        self.writer.is_some()
    }

    /// Appends the line `message` builds, stamped with the time now. While
    /// the log is off, `message` isn't called. Never waits for the file.
    pub fn log(&self, message: impl FnOnce() -> String) {
        if let Some(writer) = &self.writer {
            let time = SystemTime::now();
            let _ = writer.send(Request::Line(time, message()));
        }
    }

    /// Waits at most `timeout` for every line logged so far to reach the
    /// file, for the end of the app and for tests. Returns whether they did;
    /// always true for a log that writes nothing.
    pub fn flush(&self, timeout: Duration) -> bool {
        let Some(writer) = &self.writer else {
            return true;
        };
        let (done, answer) = mpsc::channel();
        writer.send(Request::Flush(done)).is_ok() && answer.recv_timeout(timeout).is_ok()
    }
}

/// Where the log goes for the value of `AUDIOSYNC_DEBUG`: [`FILE_NAME`] in
/// `dir` when the value is exactly `1`, nowhere for any other value or none.
pub fn log_path(setting: Option<&OsStr>, dir: &Path) -> Option<PathBuf> {
    (setting == Some(OsStr::new("1"))).then(|| dir.join(FILE_NAME))
}

/// Where the environment asks for the log, and a line to start it with
/// when it can't go where it should.
#[cfg(not(target_os = "linux"))]
fn env_path() -> (Option<PathBuf>, Option<String>) {
    let setting = std::env::var_os(ENV_VAR);
    (log_path(setting.as_deref(), &std::env::temp_dir()), None)
}

/// On Linux the log lives in the state folder, which outlasts a sign-out
/// (`crate::os::xdg::debug_log_file`); the temporary folder only when that
/// can't be made.
#[cfg(target_os = "linux")]
fn env_path() -> (Option<PathBuf>, Option<String>) {
    let setting = std::env::var_os(ENV_VAR);
    let Some(fallback) = log_path(setting.as_deref(), &std::env::temp_dir()) else {
        return (None, None);
    };
    match crate::os::xdg::debug_log_file(crate::os::xdg::state_base()) {
        Ok(path) => (Some(path), None),
        Err(error) => (
            Some(fallback),
            Some(format!(
                "debug log: the state folder can't be used ({error}); writing to the temporary folder"
            )),
        ),
    }
}

/// A writer that counts the bytes it passes on.
struct Counted<W> {
    inner: W,
    written: u64,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The writer thread: appends each line and flushes whenever no request
/// waits, until every clone of the log is gone. A line that can't be
/// written is dropped. Once the file holds `max_bytes`, one last line says
/// so and later lines are dropped.
fn write_requests(requests: &Receiver<Request>, file: File, max_bytes: u64) {
    let mut out = Counted {
        inner: BufWriter::new(file),
        written: 0,
    };
    while let Ok(request) = requests.recv() {
        let mut next = Some(request);
        while let Some(request) = next {
            match request {
                Request::Line(time, text) if out.written < max_bytes => {
                    let _ = write_line(&mut out, time, &text);
                    if out.written >= max_bytes {
                        let full = format!(
                            "the debug log is full ({max_bytes} bytes); later lines are dropped"
                        );
                        let _ = write_line(&mut out, time, &full);
                    }
                }
                Request::Line(..) => {}
                Request::Flush(done) => {
                    let _ = out.flush();
                    let _ = done.send(());
                }
            }
            next = requests.try_recv().ok();
        }
        let _ = out.flush();
    }
}

/// Writes `text` stamped with `time`. Each line of a message that has
/// several gets the stamp, so every line of the file starts with one.
fn write_line(out: &mut impl Write, time: SystemTime, text: &str) -> io::Result<()> {
    let stamp = utc_timestamp(time);
    if text.is_empty() {
        return writeln!(out, "{stamp}");
    }
    for line in text.lines() {
        writeln!(out, "{stamp} {line}")?;
    }
    Ok(())
}

/// `time` in UTC, to the millisecond, as `2026-10-18T17:52:13.456Z`. A time
/// before 1970 shows as the start of 1970.
pub fn utc_timestamp(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let (year, month, day) = civil_date(seconds / 86_400);
    let second_of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
        since_epoch.subsec_millis(),
    )
}

/// The date (year, month 1 to 12, day 1 to 31) of the day `days` after
/// 1970-01-01, in the Gregorian calendar.
///
/// It counts in years that start on 1 March, so a leap day is the last day
/// of its year, and in eras of 400 years (146 097 days), after which the
/// calendar repeats.
fn civil_date(days: u64) -> (u64, u64, u64) {
    // Days since 0000-03-01, which is 719 468 days before 1970-01-01.
    let days = days + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    // Each 4-year cycle has a leap day (1 461 days), except the last of each
    // 100 years (36 524 days); the era's last day ends its 400th year.
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // From March, five months always span 153 days (31 30 31 30 31).
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = era * 400 + year_of_era + u64::from(month <= 2);
    (year, month, day)
}

/// How long a test waits for the writer thread.
#[cfg(test)]
pub const TEST_FLUSH: Duration = Duration::from_secs(10);

/// A new, empty folder for the test `name`, in the build's target folder
/// next to the test executable, never in a folder anything else shares.
#[cfg(test)]
pub fn test_dir(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable has a path");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("the test executable is in the target folder")
        .join("debug-log-tests")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the test folder can be made");
    dir
}

/// The messages of the log at `path`, in order, without their time stamps.
/// Panics on a line that doesn't start with one.
#[cfg(test)]
pub fn read_messages(path: &Path) -> Vec<String> {
    const PATTERN: &str = "dddd-dd-ddTdd:dd:dd.dddZ";
    let text = std::fs::read_to_string(path).expect("the log file is there");
    text.lines()
        .map(|line| {
            let stamp = line.get(..PATTERN.len()).unwrap_or(line);
            let stamped = stamp.len() == PATTERN.len()
                && stamp
                    .chars()
                    .zip(PATTERN.chars())
                    .all(|(c, p)| if p == 'd' { c.is_ascii_digit() } else { c == p });
            assert!(stamped, "{line:?} starts with a time stamp");
            let message = &line[PATTERN.len()..];
            message.strip_prefix(' ').unwrap_or(message).to_owned()
        })
        .collect()
}

/// Asserts that `expected` appear in `messages` in this order, each within
/// one message, with any other messages between them.
#[cfg(test)]
pub fn assert_in_order(messages: &[String], expected: &[&str]) {
    let mut rest = messages.iter();
    for part in expected {
        assert!(
            rest.any(|message| message.contains(part)),
            "{part:?} follows in the log:\n{}",
            messages.join("\n")
        );
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;

    use super::*;

    #[test]
    fn the_log_is_on_only_when_audiosync_debug_is_exactly_1() {
        let dir = Path::new("logs");
        assert_eq!(
            log_path(Some(OsStr::new("1")), dir),
            Some(dir.join("audiosync-debug.log"))
        );
        for value in ["0", "", "true", "yes", " 1", "1 ", "01", "11"] {
            assert_eq!(log_path(Some(OsStr::new(value)), dir), None, "{value:?}");
        }
        assert_eq!(log_path(None, dir), None);
    }

    #[test]
    fn a_log_that_is_off_creates_no_file_and_builds_no_message() {
        let dir = test_dir("log-off");
        for setting in [None, Some(OsStr::new("0")), Some(OsStr::new("true"))] {
            let log = DebugLog::new(log_path(setting, &dir));
            assert!(!log.enabled());
            let built = Cell::new(false);
            log.log(|| {
                built.set(true);
                "built".to_owned()
            });
            assert!(!built.get(), "the message isn't built");
            assert!(log.flush(TEST_FLUSH));
            assert!(!dir.join(FILE_NAME).exists(), "{setting:?} writes no file");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lines_are_appended_in_order_each_with_a_time_stamp() {
        let dir = test_dir("log-order");
        let log = DebugLog::new(log_path(Some(OsStr::new("1")), &dir));
        assert!(log.enabled());
        let built = Cell::new(false);
        log.log(|| {
            built.set(true);
            "first".to_owned()
        });
        assert!(built.get(), "the message is built while the log is on");
        let clone = log.clone();
        clone.log(|| "second".to_owned());
        log.log(|| "third, with two\nlines".to_owned());
        log.log(String::new);
        assert!(log.flush(TEST_FLUSH));
        assert_eq!(
            read_messages(&dir.join(FILE_NAME)),
            ["first", "second", "third, with two", "lines", ""]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn each_launch_empties_the_last_log() {
        let dir = test_dir("log-truncate");
        let path = dir.join(FILE_NAME);
        fs::write(&path, "2026-10-18T17:52:13.456Z the last launch\n").unwrap();
        let log = DebugLog::new(Some(path.clone()));
        assert!(log.flush(TEST_FLUSH));
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        log.log(|| "this launch".to_owned());
        assert!(log.flush(TEST_FLUSH));
        assert_eq!(read_messages(&path), ["this launch"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_launch_adds_to_the_running_copys_log() {
        let dir = test_dir("log-second-launch");
        let path = dir.join(FILE_NAME);
        let running = DebugLog::new(Some(path.clone()));
        running.log(|| "the running copy".to_owned());
        assert!(running.flush(TEST_FLUSH));
        let second = DebugLog::open(Some(path.clone()), MAX_BYTES, false);
        second.log(|| "the second launch".to_owned());
        assert!(second.flush(TEST_FLUSH));
        assert_eq!(
            read_messages(&path),
            ["the running copy", "the second launch"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_log_stops_growing_at_its_limit() {
        let dir = test_dir("log-limit");
        let path = dir.join(FILE_NAME);
        let log = DebugLog::with_limit(Some(path.clone()), 200);
        for n in 0..20 {
            log.log(|| format!("line {n:02} of a long run"));
        }
        assert!(log.flush(TEST_FLUSH));
        let messages = read_messages(&path);
        assert!(messages.len() < 20, "{} lines", messages.len());
        assert!(messages[0].starts_with("line 00"));
        let full = "the debug log is full (200 bytes); later lines are dropped";
        assert_eq!(messages.last().map(String::as_str), Some(full));
        assert_eq!(messages.iter().filter(|m| m.as_str() == full).count(), 1);
        let size = fs::metadata(&path).unwrap().len();
        assert!(size < 200 + 2 * 128, "{size} bytes");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lines_land_at_the_end_after_the_file_was_emptied_meanwhile() {
        let dir = test_dir("log-append");
        let path = dir.join(FILE_NAME);
        let log = DebugLog::new(Some(path.clone()));
        log.log(|| "before someone empties the file".to_owned());
        assert!(log.flush(TEST_FLUSH));
        fs::write(&path, "").unwrap();
        log.log(|| "after".to_owned());
        assert!(log.flush(TEST_FLUSH));
        let bytes = fs::read(&path).unwrap();
        assert!(!bytes.contains(&0), "no gap of NUL bytes");
        assert_eq!(read_messages(&path), ["after"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_log_whose_file_cant_be_made_is_off() {
        let dir = test_dir("log-no-folder");
        let log = DebugLog::new(Some(dir.join("missing").join(FILE_NAME)));
        assert!(!log.enabled());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn times_are_utc_to_the_millisecond() {
        let at = |seconds: u64, millis: u64| {
            utc_timestamp(UNIX_EPOCH + Duration::from_millis(seconds * 1_000 + millis))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        // A leap day in a year divisible by 400.
        assert_eq!(at(951_868_799, 999), "2000-02-29T23:59:59.999Z");
        // The last day of a leap year.
        assert_eq!(at(1_735_689_599, 7), "2024-12-31T23:59:59.007Z");
        assert_eq!(at(1_792_345_933, 456), "2026-10-18T17:52:13.456Z");
        // 2100 is no leap year: 28 February is followed by 1 March.
        assert_eq!(at(4_107_542_399, 0), "2100-02-28T23:59:59.000Z");
        assert_eq!(at(4_107_542_400, 0), "2100-03-01T00:00:00.000Z");
        let before = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(utc_timestamp(before), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn every_day_follows_the_one_before() {
        let leap = |year: u64| {
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
        };
        let month_length = |year: u64, month: u64| match month {
            2 if leap(year) => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        let mut last = civil_date(0);
        assert_eq!(last, (1970, 1, 1));
        // Over 400 years, past the century years 2000, 2100, 2200 and 2300.
        for days in 1..=146_097 {
            let date = civil_date(days);
            let (year, month, day) = last;
            let expected = if day < month_length(year, month) {
                (year, month, day + 1)
            } else if month < 12 {
                (year, month + 1, 1)
            } else {
                (year + 1, 1, 1)
            };
            assert_eq!(date, expected, "the day after {last:?}");
            last = date;
        }
        // The calendar repeats every 400 years.
        assert_eq!(last, (2370, 1, 1));
    }
}
