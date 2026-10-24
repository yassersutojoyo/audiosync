//! A launch's claim on the session, whatever holds it (a named mutex on
//! Windows, a file lock on Linux): the first copy runs and listens, a
//! second one hands over to it and exits, or waits for a copy that is
//! quitting (`crate::single_instance` has the rules).

use std::rc::{Rc, Weak};

use crate::debug_log::DebugLog;
use crate::single_instance::{self, Acquired, HandOver};
use crate::view::App;
use crate::{LOG_FLUSH_AT_EXIT, launch_at_login};

/// The system's hold on the session, once this copy is the first.
pub trait Listen {
    /// Starts the listener thread: each request runs `show` on the UI
    /// thread, then answers the second copy.
    fn listen(&self, show: fn());
}

/// This copy's hold on the session, from [`claim`].
pub struct Instance<L> {
    /// `Err` holds why a later hand-over can't reach this copy (it runs
    /// anyway).
    lock: Result<L, String>,
    /// How the hand-over to a copy that was quitting went, when this launch
    /// waited for it to go.
    waited_for: Option<HandOver>,
}

/// Before the log, which a launch empties, and before any audio, tray or
/// window: `None` when a second launch only handed over to the running copy
/// and exits.
pub fn claim<L>(
    acquire: impl Fn() -> Acquired<L>,
    hand_over: impl FnOnce() -> HandOver,
) -> Option<Instance<L>> {
    let launched = std::time::Instant::now();
    let (lock, waited_for) = match acquire() {
        Acquired::First(lock) => (lock, None),
        Acquired::Second
            if !launch_at_login::hands_over_to_running_copy(
                launch_at_login::started_in_background(std::env::args_os()),
            ) =>
        {
            let log = DebugLog::appending_from_env();
            log.log(|| {
                format!(
                    "process {}: AudioSync already runs in this session; started with {}, \
                     so it leaves that copy as it is; exiting",
                    std::process::id(),
                    launch_at_login::BACKGROUND_FLAG
                )
            });
            log.flush(LOG_FLUSH_AT_EXIT);
            return None;
        }
        Acquired::Second => {
            // A copy that is quitting doesn't answer; once it's gone, this
            // launch starts instead, as on the Mac.
            let (outcome, first) = single_instance::second_launch(
                hand_over,
                || match acquire() {
                    Acquired::First(lock) => Some(lock),
                    Acquired::Second => None,
                },
                || launched.elapsed(),
                std::thread::sleep,
            );
            match first {
                Some(lock) => (lock, Some(outcome)),
                None => {
                    let log = DebugLog::appending_from_env();
                    log.log(|| outcome.log_line(std::process::id()));
                    log.flush(LOG_FLUSH_AT_EXIT);
                    return None;
                }
            }
        }
    };
    Some(Instance { lock, waited_for })
}

thread_local! {
    /// The app and its log, for a second launch's request to show the main
    /// window.
    static RUNNING_APP: std::cell::RefCell<Option<(Weak<App>, DebugLog)>> =
        const { std::cell::RefCell::new(None) };
}

/// A second launch asked for the main window: shown as by the tray's Open.
fn show_main_for_second_launch() {
    let running = RUNNING_APP.with(|cell| {
        let cell = cell.borrow();
        let (app, log) = cell.as_ref()?;
        Some((app.upgrade()?, log.clone()))
    });
    if let Some((app, log)) = running {
        log.log(|| "a second launch asked for the main window: shown".to_owned());
        app.show_main();
    }
}

impl<L: Listen> Instance<L> {
    /// Logs how this copy got the session, once the log is open.
    pub fn log_state(&self, log: &DebugLog) {
        if let Some(outcome) = self.waited_for {
            log.log(|| outcome.took_over_line());
        }
        match &self.lock {
            Ok(_) => log.log(|| "single instance: this is the only copy in the session".to_owned()),
            Err(error) => log.log(|| {
                format!("single instance: a second launch can't reach this copy ({error})")
            }),
        }
    }

    /// From now on a second launch shows `app`'s main window.
    pub fn listen(&self, app: &Rc<App>, log: &DebugLog) {
        if let Ok(lock) = &self.lock {
            RUNNING_APP.with(|cell| {
                *cell.borrow_mut() = Some((Rc::downgrade(app), log.clone()));
            });
            lock.listen(show_main_for_second_launch);
        }
    }
}
