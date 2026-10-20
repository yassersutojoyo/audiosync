//! One AudioSync per Windows session, as on the Mac: a second launch asks
//! the copy that already runs to show its main window, then exits.
//!
//! The first copy holds a named mutex in the session namespace (`Local\`),
//! created before the debug log, the audio, the tray icon or any window, so
//! a second launch never empties the log or opens a device. The hand-over
//! uses two named auto-reset events next to it: the second copy sets the
//! request event, and the first copy's listener thread passes the request to
//! the UI thread, which shows the main window (the tray's Open) and sets the
//! answer event. Events need no window: they reach the first copy whether
//! its main window is shown, hidden in the tray or minimized, and no other
//! app's window can answer by mistake, as one found by its title could. The
//! second copy lets the first one take the foreground
//! (`AllowSetForegroundWindow`), since Windows lets only the app the user
//! just started do that, and waits at most [`ANSWER_WAIT`] for the answer.
//! Whatever happens, it exits: never two copies.
//!
//! The decisions are plain Rust and unit-tested; the Win32 calls stay thin.

use std::time::Duration;

/// The names derive from the app's bundle identifier, shared with the Mac
/// app. `Local\` keeps them per session, so each signed-in user runs one.
#[cfg(windows)]
const MUTEX_NAME: windows::core::PCWSTR =
    windows::core::w!("Local\\io.github.yassersutojoyo.audiosync.instance");
#[cfg(windows)]
const REQUEST_NAME: windows::core::PCWSTR =
    windows::core::w!("Local\\io.github.yassersutojoyo.audiosync.show-main");
#[cfg(windows)]
const ANSWER_NAME: windows::core::PCWSTR =
    windows::core::w!("Local\\io.github.yassersutojoyo.audiosync.main-shown");

/// How long a second launch waits for the running copy to show its window.
pub const ANSWER_WAIT: Duration = Duration::from_secs(2);

/// What a launch does, given whether another copy holds the mutex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Startup {
    /// No other copy: start the app.
    Run,
    /// Another copy runs: ask it to show its main window, then exit.
    HandOver,
}

impl Startup {
    /// A mutex that can't be created at all counts as no other copy: the
    /// app still starts rather than refusing to run.
    pub fn decide(other_copy_runs: bool) -> Startup {
        if other_copy_runs {
            Startup::HandOver
        } else {
            Startup::Run
        }
    }
}

/// How the hand-over went, for the second copy's one debug line. The
/// second copy exits the same way (status 0) for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandOver {
    /// The running copy showed its main window.
    Shown,
    /// The request was sent, but no answer came within [`ANSWER_WAIT`]
    /// (the running copy is busy, hung, or still starting).
    NoAnswer,
    /// The request couldn't be sent (the running copy is starting or
    /// stopping and has no request event).
    NotSent,
}

impl HandOver {
    /// Sends the request with `send`, then waits for the answer with
    /// `wait_for_answer`, given at most [`ANSWER_WAIT`]. Doesn't wait when
    /// nothing was sent.
    pub fn run(
        send: impl FnOnce() -> bool,
        wait_for_answer: impl FnOnce(Duration) -> bool,
    ) -> Self {
        if !send() {
            HandOver::NotSent
        } else if wait_for_answer(ANSWER_WAIT) {
            HandOver::Shown
        } else {
            HandOver::NoAnswer
        }
    }

    /// The second copy's debug line.
    pub fn log_line(self, process: u32) -> String {
        let outcome = match self {
            HandOver::Shown => "it showed its main window".to_owned(),
            HandOver::NoAnswer => format!("it didn't answer within {} s", ANSWER_WAIT.as_secs()),
            HandOver::NotSent => "the request couldn't be sent".to_owned(),
        };
        format!(
            "process {process}: AudioSync already runs in this session; asked it to show \
             its main window ({outcome}); exiting"
        )
    }
}

/// The first copy's listener: for each request `wait_for_request` returns,
/// calls `on_request`, until waiting fails.
pub fn serve(mut wait_for_request: impl FnMut() -> bool, on_request: impl Fn()) {
    while wait_for_request() {
        on_request();
    }
}

/// A Win32 handle that can cross threads. Kernel object handles are valid
/// in every thread of the process.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct Handle(usize);

#[cfg(windows)]
impl Handle {
    fn new(handle: windows::Win32::Foundation::HANDLE) -> Handle {
        Handle(handle.0 as usize)
    }

    fn get(self) -> windows::Win32::Foundation::HANDLE {
        windows::Win32::Foundation::HANDLE(self.0 as *mut core::ffi::c_void)
    }
}

/// The first copy's hold on the session: the mutex and the hand-over
/// events, open until the process ends.
#[cfg(windows)]
pub struct Instance {
    request: Handle,
    answer: Handle,
}

/// What [`acquire`] found.
#[cfg(windows)]
pub enum Acquired {
    /// This is the only copy; `Err` holds why a later hand-over can't
    /// reach it (it runs anyway).
    First(Result<Instance, String>),
    /// Another copy runs.
    Second,
}

/// Creates the session's mutex and, when this is the first copy, the
/// hand-over events. A mutex that can't be created counts as the first copy.
#[cfg(windows)]
pub fn acquire() -> Acquired {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::{CreateEventW, CreateMutexW};

    // SAFETY: the name is a NUL-terminated UTF-16 literal; no security
    // attributes. The last error is read right after the call that set it.
    let mutex = unsafe { CreateMutexW(None, false, MUTEX_NAME) };
    let other_copy_runs = mutex.is_ok() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    match Startup::decide(other_copy_runs) {
        Startup::HandOver => Acquired::Second,
        Startup::Run => {
            // The mutex handle is never closed: it stays open until the
            // process ends, and Windows closes it then, also after a crash,
            // so the next launch starts normally.
            if let Err(error) = mutex {
                return Acquired::First(Err(format!("the instance mutex: {}", error.message())));
            }
            // SAFETY: as above; auto-reset events, not signaled.
            let events = unsafe {
                CreateEventW(None, false, false, REQUEST_NAME).and_then(|request| {
                    Ok((request, CreateEventW(None, false, false, ANSWER_NAME)?))
                })
            };
            Acquired::First(
                events
                    .map(|(request, answer)| Instance {
                        request: Handle::new(request),
                        answer: Handle::new(answer),
                    })
                    .map_err(|e| format!("the hand-over events: {}", e.message())),
            )
        }
    }
}

#[cfg(windows)]
impl Instance {
    /// Starts the listener thread: each request runs `show` on the UI
    /// thread, then answers the second copy.
    pub fn listen(&self, show: fn()) {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::{INFINITE, SetEvent, WaitForSingleObject};

        let (request, answer) = (self.request, self.answer);
        let _ = std::thread::Builder::new()
            .name("audiosync-single-instance".to_owned())
            .spawn(move || {
                serve(
                    // SAFETY: the event stays open for the process's life.
                    || unsafe { WaitForSingleObject(request.get(), INFINITE) } == WAIT_OBJECT_0,
                    || {
                        let _ = slint::invoke_from_event_loop(move || {
                            show();
                            // SAFETY: as above.
                            let _ = unsafe { SetEvent(answer.get()) };
                        });
                    },
                );
            });
    }
}

/// The second copy: lets the running copy take the foreground, asks it to
/// show its main window, and waits at most [`ANSWER_WAIT`] for it.
#[cfg(windows)]
pub fn hand_over() -> HandOver {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        EVENT_MODIFY_STATE, OpenEventW, ResetEvent, SYNCHRONIZATION_SYNCHRONIZE, SetEvent,
        WaitForSingleObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

    // Only the process the user just started may take the foreground; this
    // passes that right on. ASFW_ANY because the running copy's process ID
    // isn't known here; Windows withdraws the right at the next input.
    // SAFETY: no pointers.
    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    let access = EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE;
    // SAFETY: the names are NUL-terminated UTF-16 literals.
    let request = unsafe { OpenEventW(access, false, REQUEST_NAME) };
    let answer = unsafe { OpenEventW(access, false, ANSWER_NAME) };
    let outcome = match (&request, &answer) {
        (Ok(request), Ok(answer)) => HandOver::run(
            || {
                // SAFETY: both handles are open until the end of this
                // function. A late answer to an earlier launch is cleared
                // first, so only this request's answer counts.
                let _ = unsafe { ResetEvent(*answer) };
                unsafe { SetEvent(*request) }.is_ok()
            },
            |wait| {
                let ms = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
                // SAFETY: as above.
                let answered = unsafe { WaitForSingleObject(*answer, ms) };
                answered == WAIT_OBJECT_0
            },
        ),
        _ => HandOver::NotSent,
    };
    for handle in [request, answer].into_iter().flatten() {
        // SAFETY: each handle was opened above and is closed once.
        let _ = unsafe { CloseHandle(handle) };
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_launch_runs_unless_another_copy_holds_the_mutex() {
        assert_eq!(Startup::decide(false), Startup::Run);
        assert_eq!(Startup::decide(true), Startup::HandOver);
    }

    #[test]
    fn an_answered_request_means_the_window_was_shown() {
        let waited = Cell::new(None);
        let outcome = HandOver::run(
            || true,
            |wait| {
                waited.set(Some(wait));
                true
            },
        );
        assert_eq!(outcome, HandOver::Shown);
        assert_eq!(waited.get(), Some(ANSWER_WAIT));
    }

    #[test]
    fn a_hung_copy_leaves_the_request_unanswered_and_the_launch_still_ends() {
        assert_eq!(HandOver::run(|| true, |_| false), HandOver::NoAnswer);
    }

    #[test]
    fn a_request_that_cannot_be_sent_is_not_waited_for() {
        let waited = Cell::new(false);
        let outcome = HandOver::run(
            || false,
            |_| {
                waited.set(true);
                true
            },
        );
        assert_eq!(outcome, HandOver::NotSent);
        assert!(!waited.get());
    }

    #[test]
    fn the_answer_wait_is_about_two_seconds() {
        assert_eq!(ANSWER_WAIT, Duration::from_secs(2));
    }

    #[test]
    fn the_second_copy_logs_one_line_per_outcome() {
        for (outcome, says) in [
            (HandOver::Shown, "it showed its main window"),
            (HandOver::NoAnswer, "didn't answer within 2 s"),
            (HandOver::NotSent, "couldn't be sent"),
        ] {
            let line = outcome.log_line(42);
            assert!(
                line.starts_with("process 42: AudioSync already runs"),
                "{line}"
            );
            assert!(line.contains(says), "{line}");
            assert!(line.ends_with("exiting"), "{line}");
            assert_eq!(line.lines().count(), 1, "{line}");
        }
    }

    #[test]
    fn the_listener_shows_the_window_once_per_request_until_waiting_fails() {
        let requests = Cell::new(3_u32);
        let shown = Cell::new(0);
        serve(
            || {
                let left = requests.get();
                requests.set(left.saturating_sub(1));
                left > 0
            },
            || shown.set(shown.get() + 1),
        );
        assert_eq!(shown.get(), 3);
    }

    #[test]
    fn the_check_comes_before_the_log_and_the_app_start() {
        let main = include_str!("main.rs");
        let body = &main[main.find("fn main() -> ExitCode").expect("main")..];
        let acquire = body.find("single_instance::acquire()").expect("acquire");
        let log = body.find("DebugLog::from_env()").expect("log");
        let run = body.find("match run(").expect("run");
        assert!(acquire < log && log < run);
    }
}
