//! What happens when Slint's event loop ends with an error after the app has
//! run, as when a GPU driver reset leaves the window's surface unusable: the
//! same exe starts again in this one's place, and losses that keep coming
//! move Windows to the software renderer instead of starting again forever.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::launch_at_login::BACKGROUND_FLAG;
use crate::updater::relaunch::MARKER;

/// The file in the data folder that keeps the recent losses, in Unix seconds.
pub const RECORD_NAME: &str = "surface-losses.txt";

/// Losses within [`WINDOW`] after which starting again with the same
/// renderer stops.
pub const LOSSES_IN_A_ROW: usize = 3;

pub const WINDOW: Duration = Duration::from_secs(600);

/// How long the event loop runs before an error in it counts as a lost
/// surface rather than a failed start.
pub const START: Duration = Duration::from_secs(1);

/// Marks a copy a lost surface started; the updater's restart passes only
/// [`MARKER`].
pub const AFTER_LOSS: &str = "--after-surface-loss";

/// With `AUDIOSYNC_DEBUG=1`, fakes a lost surface for a test run: `<seconds>`
/// after the start of the first copy only, or `repeat` in every copy still
/// drawing on the GPU, [`FAKE_REPEAT_AFTER`] after its start.
pub const FAKE_LOSS_VAR: &str = "AUDIOSYNC_FAKE_SURFACE_LOSS";
pub const FAKE_REPEAT_AFTER: Duration = Duration::from_secs(5);
pub const FAKE_ERROR: &str = "WGPU surface validation error in get_current_texture (faked)";

/// When this copy fakes a lost surface, from the debug log's switch, the
/// value of [`FAKE_LOSS_VAR`], whether a restart started this copy and
/// whether it draws on the GPU.
pub fn fake_loss_after(
    debug: bool,
    value: Option<&str>,
    relaunched: bool,
    on_gpu: bool,
) -> Option<Duration> {
    if !debug {
        return None;
    }
    match value? {
        "repeat" => on_gpu.then_some(FAKE_REPEAT_AFTER),
        seconds if !relaunched => seconds.parse().ok().map(Duration::from_secs),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The app never ran past its start: the start-failure box, as before.
    StartFailed,
    /// A new copy takes over, with the software renderer when `software`.
    Relaunch { software: bool },
    /// Losses keep coming with no renderer left to fall back to.
    Stop,
}

/// What follows an event loop error, from whether the app ran past its start,
/// the losses in the last [`WINDOW`] with this one, and whether the renderer
/// can still fall back to software.
pub fn after_loop_error(ran: bool, losses: usize, can_fall_back: bool) -> Outcome {
    if !ran {
        Outcome::StartFailed
    } else if losses < LOSSES_IN_A_ROW {
        Outcome::Relaunch { software: false }
    } else if can_fall_back {
        Outcome::Relaunch { software: true }
    } else {
        Outcome::Stop
    }
}

/// The losses in the last [`WINDOW`] with one at `now`, and the record's new
/// text, from the record's `held` text.
pub fn count_loss(held: &str, now: u64) -> (usize, String) {
    let mut kept: Vec<u64> = held
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .filter(|&at| at <= now && now - at < WINDOW.as_secs())
        .collect();
    kept.push(now);
    let text = kept.iter().map(|at| format!("{at}\n")).collect();
    (kept.len(), text)
}

/// Adds a loss at `now` to the record in `folder`; the losses in the last
/// [`WINDOW`] with it. Without a record this one alone counts.
pub fn note_loss(folder: Option<&Path>, now: u64) -> usize {
    let Some(folder) = folder else {
        return 1;
    };
    let record = folder.join(RECORD_NAME);
    let held = std::fs::read_to_string(&record).unwrap_or_default();
    let (losses, text) = count_loss(&held, now);
    let _ = crate::data_folder::write_atomically(&record, text.as_bytes());
    losses
}

/// Whether this launch's `args` (with the exe) are a copy a lost surface
/// started, which counts as running from its start: the driver may still be
/// resetting then.
pub fn after_loss<A: AsRef<std::ffi::OsStr>>(args: impl IntoIterator<Item = A>) -> bool {
    args.into_iter()
        .skip(1)
        .any(|arg| arg.as_ref() == std::ffi::OsStr::new(AFTER_LOSS))
}

/// The new copy's arguments: this launch's `args` (without the exe), less an
/// earlier restart's markers and the background flag, which comes back when
/// the main window was hidden, then the markers naming process `own`.
pub fn relaunch_args(
    args: impl IntoIterator<Item = OsString>,
    main_visible: bool,
    own: u32,
) -> Vec<OsString> {
    let mut kept = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == MARKER {
            args.next();
        } else if arg != BACKGROUND_FLAG && arg != AFTER_LOSS {
            kept.push(arg);
        }
    }
    if !main_visible {
        kept.push(BACKGROUND_FLAG.into());
    }
    kept.extend([AFTER_LOSS.into(), MARKER.into(), own.to_string().into()]);
    kept
}

#[cfg(test)]
mod tests {
    use super::{
        AFTER_LOSS, BACKGROUND_FLAG, FAKE_REPEAT_AFTER, MARKER, Outcome, after_loop_error,
        after_loss, count_loss, fake_loss_after, relaunch_args,
    };
    use std::ffi::OsString;
    use std::time::Duration;

    #[test]
    fn an_error_before_the_app_ran_past_its_start_stays_a_start_failure() {
        assert_eq!(after_loop_error(false, 0, true), Outcome::StartFailed);
        assert_eq!(after_loop_error(false, 5, false), Outcome::StartFailed);
    }

    // The GPU driver reset a running copy's surface (CS2 starting on an AMD card).
    #[test]
    fn a_lost_surface_after_the_app_ran_starts_a_new_copy_with_the_same_renderer() {
        assert_eq!(
            after_loop_error(true, 1, true),
            Outcome::Relaunch { software: false }
        );
        assert_eq!(
            after_loop_error(true, 2, false),
            Outcome::Relaunch { software: false }
        );
    }

    #[test]
    fn the_third_loss_in_the_window_falls_back_to_software_or_stops_without_one() {
        assert_eq!(
            after_loop_error(true, 3, true),
            Outcome::Relaunch { software: true }
        );
        assert_eq!(after_loop_error(true, 3, false), Outcome::Stop);
        assert_eq!(after_loop_error(true, 4, false), Outcome::Stop);
    }

    #[test]
    fn losses_count_only_within_the_window_and_the_record_keeps_those() {
        let now = 10_000;
        assert_eq!(count_loss("", now), (1, "10000\n".to_owned()));
        let held = "8000\n9500\nnot a time\n9900\n";
        assert_eq!(count_loss(held, now), (3, "9500\n9900\n10000\n".to_owned()));
        // A clock set back leaves no loss from the future in the count.
        assert_eq!(count_loss("20000\n", now), (1, "10000\n".to_owned()));
    }

    #[test]
    fn the_new_copy_gets_this_launchs_arguments_and_the_window_state() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            relaunch_args(args(&["--x"]), true, 42),
            args(&["--x", AFTER_LOSS, MARKER, "42"])
        );
        assert_eq!(
            relaunch_args(
                args(&[BACKGROUND_FLAG, AFTER_LOSS, MARKER, "7", "--x"]),
                true,
                42
            ),
            args(&["--x", AFTER_LOSS, MARKER, "42"])
        );
        assert_eq!(
            relaunch_args(args(&[]), false, 42),
            args(&[BACKGROUND_FLAG, AFTER_LOSS, MARKER, "42"])
        );
    }

    // Restart Now also passes the marker; a new version that fails at its first frame stays a start failure.
    #[test]
    fn only_a_copy_a_lost_surface_started_counts_as_running_from_its_start() {
        assert!(!after_loss(["audiosync.exe", MARKER, "7"]));
        assert!(after_loss(["audiosync.exe", AFTER_LOSS, MARKER, "7"]));
        assert!(!after_loss(["audiosync.exe"]));
        assert!(!after_loss([AFTER_LOSS]));
    }

    #[test]
    fn a_fake_loss_needs_the_debug_log_and_a_plain_number_fakes_only_the_first_copy() {
        for value in [Some("8"), Some("repeat")] {
            assert_eq!(fake_loss_after(false, value, false, true), None);
        }
        assert_eq!(
            fake_loss_after(true, Some("8"), false, true),
            Some(Duration::from_secs(8))
        );
        assert_eq!(fake_loss_after(true, Some("8"), true, true), None);
        assert_eq!(
            fake_loss_after(true, Some("repeat"), true, true),
            Some(FAKE_REPEAT_AFTER)
        );
        assert_eq!(fake_loss_after(true, Some("repeat"), true, false), None);
        assert_eq!(fake_loss_after(true, Some("soon"), false, true), None);
        assert_eq!(fake_loss_after(true, None, false, true), None);
    }
}
