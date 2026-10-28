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
//! A second launch holds no handle to the mutex. When no answer comes (the
//! running copy may be quitting), it checks every [`RELAUNCH_POLL`] whether
//! the mutex is free and, if it becomes free within [`RELAUNCH_WAIT`] of the
//! launch, starts as the first copy, as the Mac does; otherwise it exits.
//! Never two copies: a mutex held by an elevated copy, which a normal launch
//! may not open, counts as another copy too.
//!
//! The decisions are plain Rust and unit-tested; the Win32 calls stay thin.
//! Linux follows the same decisions with a file lock and a Unix socket
//! (`crate::os::xdg::instance`).

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

/// How long a second launch whose request went unanswered keeps trying to
/// become the first copy, counted from the launch: a quitting copy holds the
/// mutex through its teardown, up to 9 s with the updater's quit waits, so
/// this is S303's 10 s (the updater's relaunch wait), not the Mac's 5.5 s.
pub const RELAUNCH_WAIT: Duration = Duration::from_secs(10);

/// How often a second launch checks whether the running copy has gone.
pub const RELAUNCH_POLL: Duration = Duration::from_millis(250);

/// What creating the session's mutex reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutexState {
    /// Created: no other copy holds it.
    Created,
    /// Opened: another copy holds it.
    Existed,
    /// Access denied: another copy holds it, one with rights this launch
    /// lacks (an elevated copy's objects can't be opened by a normal launch).
    AccessDenied,
    /// Any other failure.
    Failed,
}

/// What a launch does, given what creating the mutex reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Startup {
    /// No other copy: start the app.
    Run,
    /// Another copy runs: ask it to show its main window, then exit.
    HandOver,
}

impl Startup {
    /// A mutex that can't be created for any reason but denied access
    /// counts as no other copy: the app still starts rather than refusing to
    /// run (and logs why). Denied access means the mutex exists, so another
    /// copy runs.
    pub fn decide(mutex: MutexState) -> Startup {
        match mutex {
            MutexState::Created | MutexState::Failed => Startup::Run,
            MutexState::Existed | MutexState::AccessDenied => Startup::HandOver,
        }
    }
}

/// A second launch: hands over with `hand_over`; when the running copy
/// didn't show its window (it may be quitting), tries `try_acquire` every
/// [`RELAUNCH_POLL`] until it returns the first copy's hold or `elapsed`
/// (time since the launch) reaches [`RELAUNCH_WAIT`]. `None` means exit.
pub fn second_launch<T>(
    hand_over: impl FnOnce() -> HandOver,
    mut try_acquire: impl FnMut() -> Option<T>,
    elapsed: impl Fn() -> Duration,
    mut sleep: impl FnMut(Duration),
) -> (HandOver, Option<T>) {
    let outcome = hand_over();
    if outcome == HandOver::Shown {
        return (outcome, None);
    }
    loop {
        if let Some(first) = try_acquire() {
            return (outcome, Some(first));
        }
        let spent = elapsed();
        if spent >= RELAUNCH_WAIT {
            return (outcome, None);
        }
        sleep(RELAUNCH_POLL.min(RELAUNCH_WAIT - spent));
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

    fn describe(self) -> String {
        match self {
            HandOver::Shown => "it showed its main window".to_owned(),
            HandOver::NoAnswer => format!("it didn't answer within {} s", ANSWER_WAIT.as_secs()),
            HandOver::NotSent => "the request couldn't be sent".to_owned(),
        }
    }

    /// The second copy's debug line when it exits.
    pub fn log_line(self, process: u32) -> String {
        format!(
            "process {process}: AudioSync already runs in this session; asked it to show \
             its main window ({}); exiting",
            self.describe()
        )
    }

    /// The debug line of a launch that became the first copy after the
    /// running one went away.
    #[cfg(any(windows, target_os = "linux"))]
    pub fn took_over_line(self) -> String {
        format!(
            "single instance: another copy was quitting when this launch asked it to show \
             its main window ({}); started once it was gone",
            self.describe()
        )
    }
}

/// What trying to hold the session found; `L` is the system's hold.
#[cfg(any(windows, target_os = "linux"))]
pub enum Acquired<L> {
    /// This is the only copy; `Err` holds why a later hand-over can't
    /// reach it (it runs anyway).
    First(Result<L, String>),
    /// Another copy runs.
    Second,
}

/// The first copy's listener: for each request `wait_for_request` returns,
/// calls `on_request`, until waiting fails.
#[cfg(any(windows, test))]
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

/// Creates the session's mutex and, when this is the first copy, the
/// hand-over events. A mutex that can't be created counts as the first copy,
/// unless access to it was denied (see [`Startup::decide`]).
#[cfg(windows)]
pub fn acquire() -> Acquired<Instance> {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError,
    };
    use windows::Win32::System::Threading::{CreateEventW, CreateMutexW};

    // SAFETY: the name is a NUL-terminated UTF-16 literal; no security
    // attributes. The last error is read right after the call that set it.
    let mutex = unsafe { CreateMutexW(None, false, MUTEX_NAME) };
    let state = match &mutex {
        Ok(_) if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS => MutexState::Existed,
        Ok(_) => MutexState::Created,
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => MutexState::AccessDenied,
        Err(_) => MutexState::Failed,
    };
    match Startup::decide(state) {
        Startup::HandOver => {
            // A second launch holds no handle: the mutex goes away with the
            // running copy, so a later try of this launch, or another
            // launch, can become the first copy.
            if let Ok(handle) = mutex {
                // SAFETY: opened above, closed once.
                let _ = unsafe { CloseHandle(handle) };
            }
            Acquired::Second
        }
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
        assert_eq!(Startup::decide(MutexState::Created), Startup::Run);
        assert_eq!(Startup::decide(MutexState::Existed), Startup::HandOver);
    }

    #[test]
    fn a_mutex_held_by_an_elevated_copy_hands_over_instead_of_running_twice() {
        assert_eq!(Startup::decide(MutexState::AccessDenied), Startup::HandOver);
    }

    #[test]
    fn any_other_mutex_failure_still_runs_the_app() {
        assert_eq!(Startup::decide(MutexState::Failed), Startup::Run);
    }

    /// A fake clock: `sleep` advances it; the hand-over takes `hand_over`.
    fn run_second_launch(
        outcome: HandOver,
        hand_over: Duration,
        free_after: Option<Duration>,
    ) -> (HandOver, Option<Duration>, u32, Duration) {
        let now = Cell::new(Duration::ZERO);
        let tries = Cell::new(0_u32);
        let (outcome, first) = second_launch(
            || {
                now.set(now.get() + hand_over);
                outcome
            },
            || {
                tries.set(tries.get() + 1);
                free_after.filter(|at| now.get() >= *at).map(|_| now.get())
            },
            || now.get(),
            |pause| {
                assert!(
                    pause <= RELAUNCH_POLL && pause > Duration::ZERO,
                    "{pause:?}"
                );
                now.set(now.get() + pause);
            },
        );
        (outcome, first, tries.get(), now.get())
    }

    #[test]
    fn a_shown_window_ends_the_launch_without_retrying() {
        let (outcome, first, tries, _) = run_second_launch(
            HandOver::Shown,
            Duration::from_millis(30),
            Some(Duration::ZERO),
        );
        assert_eq!(outcome, HandOver::Shown);
        assert_eq!((first, tries), (None, 0));
    }

    #[test]
    fn a_launch_during_the_old_copys_teardown_starts_once_it_is_gone() {
        // The quitting copy no longer answers (2 s) and is gone at 3.5 s.
        let gone = Duration::from_millis(3500);
        let (outcome, first, _, _) = run_second_launch(HandOver::NoAnswer, ANSWER_WAIT, Some(gone));
        assert_eq!(outcome, HandOver::NoAnswer);
        let started = first.expect("became the first copy");
        assert!(
            started >= gone && started < gone + RELAUNCH_POLL,
            "{started:?}"
        );
    }

    /// Exit, then a launch at once: the old copy holds the mutex through the
    /// updater's quit waits and the session's quit budget.
    #[cfg(windows)]
    #[test]
    fn a_launch_waits_out_the_longest_teardown_of_a_quitting_copy() {
        let budget = Duration::from_secs_f64(audiosync_core::policy::session::QUIT_BUDGET);
        let gone = crate::updater::QUIT_WAIT + crate::updater::files::QUIT_RETRY_BUDGET + budget;
        let (_, first, _, _) = run_second_launch(HandOver::NoAnswer, ANSWER_WAIT, Some(gone));
        assert!(first.is_some(), "gave up before the old copy was gone");
    }

    #[test]
    fn a_request_that_cannot_be_sent_also_waits_for_the_old_copy() {
        let gone = Duration::from_millis(1000);
        let (_, first, _, _) = run_second_launch(HandOver::NotSent, Duration::ZERO, Some(gone));
        assert_eq!(first, Some(gone));
    }

    #[test]
    fn a_copy_that_never_goes_away_ends_the_launch_after_the_relaunch_wait() {
        let (_, first, tries, ended) = run_second_launch(HandOver::NoAnswer, ANSWER_WAIT, None);
        assert_eq!(first, None);
        assert_eq!(ended, RELAUNCH_WAIT);
        // Polled every 250 ms from 2 s to 10 s, the last try at 10 s.
        assert_eq!(tries, 33);
    }

    #[test]
    fn the_relaunch_poll_matches_the_mac() {
        assert_eq!(RELAUNCH_POLL, Duration::from_millis(250));
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
        let claim = body.find("os::claim_instance()").expect("claim");
        let log = body.find("DebugLog::from_env(").expect("log");
        let run = body.find("match run(").expect("run");
        assert!(claim < log && log < run);
        let body_of = |source: &'static str, head: &str| {
            let start = &source[source.find(head).expect(head)..];
            &start[..start.find("\n}").expect("the function ends")]
        };
        let windows = body_of(include_str!("os/windows.rs"), "pub fn claim_instance()");
        assert!(windows.contains("single_instance::acquire"));
        let linux = body_of(
            include_str!("os/xdg/instance.rs"),
            "pub fn claim_instance()",
        );
        assert!(linux.contains("acquire_in("));
        let claim = body_of(include_str!("os/claim.rs"), "pub fn claim<");
        assert!(claim.contains("acquire()"));
        assert!(!claim.contains("DebugLog::from_env("));
    }
}
