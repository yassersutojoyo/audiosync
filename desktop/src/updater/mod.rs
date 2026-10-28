//! The Windows updater (spec section 19): checks the appcast the Mac reads,
//! asks first, then downloads, verifies, unzips and swaps the exe on a
//! worker thread, and relaunches only on the user's click.

pub mod appcast;
pub mod archive;
pub mod fetch;
pub mod files;
pub mod flow;
pub mod relaunch;
pub mod signature;
pub mod swap;
pub mod version;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use appcast::{Arch, Update};
use flow::{Checked, Env, Installed};
use relaunch::Start;
use version::Version;

use crate::debug_log::DebugLog;
pub use crate::ui_model::{UpdateRecord, UpdateRecordReason, UpdaterButton, UpdaterNotice};

/// `SUFeedURL` in `Info.plist`.
pub const FEED_URL: &str =
    "https://github.com/yassersutojoyo/audiosync/releases/latest/download/appcast.xml";
/// How long a quit waits for a failed restart's undo (spec S303).
const QUIT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

pub const RELEASE_PAGE: &str = "https://github.com/yassersutojoyo/audiosync/releases/latest";

/// Whether this exe updates itself: only a build the release CI job made
/// for a push to main, which sets `AUDIOSYNC_RELEASE_BUILD` to `1`.
pub fn enabled() -> bool {
    marked(option_env!("AUDIOSYNC_RELEASE_BUILD")) && !cfg!(test)
}

fn marked(value: Option<&str>) -> bool {
    value == Some("1")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Launch,
    Manual,
}

/// What the view does after a button.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    OpenReleasePage,
}

enum Event {
    Checked(Trigger, Checked),
    Progress(Option<u8>),
    Installed(String, Installed),
    /// The new version's start, and the undo of the swap the worker ran
    /// when it didn't signal.
    Started(Start, Option<Undone>),
}

/// The undo of a swap after a failed restart (spec S303).
struct Undone {
    result: Result<(), swap::SwapError>,
    steps: swap::StepLog,
}

/// Undoes `swapped`, each delete and rename with the lock retries of spec
/// S315; on the worker thread after a failed restart, since they can wait.
fn undo(env: &Env, swapped: &swap::Swapped) -> Undone {
    let files = files::Retrying {
        inner: env.files.as_ref(),
        log: &env.log,
        pause: &env.pause,
    };
    let mut steps = swap::StepLog::new();
    let result = swap::undo_completed(&env.install_dir, swapped, &files, &mut steps);
    Undone { result, steps }
}

/// What [`Updater::poll`] saw.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Polled {
    pub changed: bool,
    /// A check fetched and read the appcast, so its time is stored.
    pub checked: bool,
    /// The new version signalled its start: stop streaming, save the
    /// settings and quit (spec S303, S314).
    pub quit: bool,
    /// [`Updater::record`] changed, so the settings keep the new one.
    pub record_changed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    Idle,
    Checking,
    Installing,
    /// Restart Now waits for the new version's start signal.
    Restarting,
}

pub struct Updater {
    env: Arc<Env>,
    sender: Sender<Event>,
    events: Receiver<Event>,
    notice: Option<UpdaterNotice>,
    /// When the notice was set, as the notice list shows it.
    notice_time: String,
    offered: Option<Update>,
    work: Work,
    /// A manual check asked while a launch check ran; that check answers it.
    manual_waiting: bool,
    cancel: Arc<AtomicBool>,
    swapped: Option<(String, swap::Swapped)>,
    /// Held by the restart's worker through its undo; a quit that stops
    /// waiting for that worker sets it, so no undo starts that the end of
    /// the process could cut in half.
    undo_closed: Arc<Mutex<bool>>,
    /// Runs on the worker right after it releases `undo_closed`.
    #[cfg(test)]
    after_undo: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The version that failed its first start or was blocked (spec S316).
    record: Option<UpdateRecord>,
}

impl Updater {
    pub fn new(env: Env) -> Updater {
        let (sender, events) = mpsc::channel();
        Updater {
            env: Arc::new(env),
            sender,
            events,
            notice: None,
            notice_time: String::new(),
            offered: None,
            work: Work::Idle,
            manual_waiting: false,
            cancel: Arc::new(AtomicBool::new(false)),
            swapped: None,
            undo_closed: Arc::new(Mutex::new(false)),
            #[cfg(test)]
            after_undo: None,
            record: None,
        }
    }

    /// The record the settings kept, before the launch check.
    pub fn set_record(&mut self, record: Option<UpdateRecord>) {
        self.record = record;
    }

    pub fn record(&self) -> Option<&UpdateRecord> {
        self.record.as_ref()
    }

    pub fn notice(&self) -> Option<&UpdaterNotice> {
        self.notice.as_ref()
    }

    pub fn notice_time(&self) -> &str {
        &self.notice_time
    }

    /// Stamps the time only when the kind of notice changes, so a download's
    /// percentage doesn't move it.
    fn set_notice(&mut self, notice: Option<UpdaterNotice>) {
        let kind = |notice: &Option<UpdaterNotice>| notice.as_ref().map(std::mem::discriminant);
        if notice.is_some() && kind(&notice) != kind(&self.notice) {
            self.notice_time = crate::platform::notice_time();
        }
        self.notice = notice;
    }

    /// Starts a check on the worker thread; after a swap it fetches nothing
    /// and shows Restart Now again.
    pub fn check(&mut self, trigger: Trigger) {
        self.env.log.log(|| format!("updater: {trigger:?} check"));
        if let Some((version, _)) = &self.swapped {
            self.env
                .log
                .log(|| format!("updater: {version} is installed and waits for a restart"));
            let version = version.clone();
            self.set_notice(Some(UpdaterNotice::Installed { version }));
            return;
        }
        match self.work {
            Work::Idle => {}
            Work::Checking => {
                if trigger == Trigger::Manual {
                    self.env
                        .log
                        .log(|| "updater: a check runs, it answers this one".to_owned());
                    self.manual_waiting = true;
                    self.set_notice(Some(UpdaterNotice::Checking));
                }
                return;
            }
            Work::Installing if self.cancel.load(Ordering::Relaxed) => {
                // The cancelled download's notice is gone; answer once it stops.
                if trigger == Trigger::Manual {
                    self.env.log.log(|| {
                        "updater: a cancelled install stops, then this check runs".to_owned()
                    });
                    self.manual_waiting = true;
                    self.set_notice(Some(UpdaterNotice::Checking));
                }
                return;
            }
            Work::Restarting => {
                self.env
                    .log
                    .log(|| "updater: a restart runs, check skipped".to_owned());
                return;
            }
            Work::Installing => {
                // The download notice already on screen is the answer.
                self.env
                    .log
                    .log(|| "updater: an install runs, check skipped".to_owned());
                return;
            }
        }
        self.work = Work::Checking;
        if trigger == Trigger::Manual {
            self.set_notice(Some(UpdaterNotice::Checking));
        }
        let env = self.env.clone();
        let sender = self.sender.clone();
        self.spawn(move || {
            let _ = sender.send(Event::Checked(trigger, flow::check(&env)));
        });
    }

    /// The launch check of spec S305, from the switch and the stored time.
    pub fn launch_check(&mut self, automatic: bool, last: Option<u64>, now: u64) {
        match flow::launch_check(automatic, last, now) {
            flow::LaunchCheck::Due(why) => {
                self.env
                    .log
                    .log(|| format!("updater: launch check due ({why})"));
                self.check(Trigger::Launch);
            }
            skipped => self
                .env
                .log
                .log(|| format!("updater: launch check skipped ({skipped:?})")),
        }
    }

    fn spawn(&self, work: impl FnOnce() + Send + 'static) {
        let started = std::thread::Builder::new()
            .name("audiosync-updater".to_owned())
            .spawn(work);
        if let Err(error) = started {
            self.env
                .log
                .log(|| format!("updater: no worker thread ({error})"));
        }
    }

    /// Takes what the worker reported.
    pub fn poll(&mut self) -> Polled {
        let mut polled = Polled::default();
        while let Ok(event) = self.events.try_recv() {
            polled.changed = true;
            self.apply(event, &mut polled);
        }
        polled
    }

    fn apply(&mut self, event: Event, polled: &mut Polled) {
        match event {
            Event::Checked(trigger, checked) => {
                self.work = Work::Idle;
                let waiting = std::mem::take(&mut self.manual_waiting);
                let manual = trigger == Trigger::Manual || waiting;
                match checked {
                    Checked::Update(update) => {
                        polled.checked = true;
                        let last = self.last_time(&update, polled);
                        if let (Some(reason), false) = (last, manual) {
                            self.env.log.log(|| {
                                format!(
                                    "updater: {} not offered at launch ({reason:?} last time)",
                                    update.version_text
                                )
                            });
                        } else {
                            self.set_notice(Some(UpdaterNotice::Available {
                                new: update.version_text.clone(),
                                running: self.env.running.to_string(),
                                last,
                            }));
                            self.offered = Some(update);
                        }
                    }
                    Checked::UpToDate => {
                        polled.checked = true;
                        if manual {
                            self.set_notice(Some(UpdaterNotice::UpToDate {
                                running: self.env.running.to_string(),
                            }));
                        }
                    }
                    Checked::Failed if manual => {
                        self.set_notice(Some(UpdaterNotice::Unreachable));
                    }
                    Checked::Failed => {}
                }
            }
            Event::Progress(percent) => {
                if self.work == Work::Installing && !self.cancel.load(Ordering::Relaxed) {
                    self.set_notice(Some(UpdaterNotice::Downloading { percent }));
                }
            }
            Event::Installed(version, installed) => {
                self.work = Work::Idle;
                let notice = match installed {
                    Installed::Done(swapped) => {
                        self.swapped = Some((version.clone(), swapped));
                        self.keep_record(None, polled);
                        Some(UpdaterNotice::Installed { version })
                    }
                    Installed::Cancelled => None,
                    Installed::DownloadFailed => Some(UpdaterNotice::DownloadFailed),
                    Installed::BadSignature => Some(UpdaterNotice::BadSignature),
                    Installed::BrokenDownload => Some(UpdaterNotice::BrokenDownload),
                    Installed::DiskFull => Some(UpdaterNotice::DiskFull),
                    Installed::ReadOnly => Some(UpdaterNotice::ReadOnly),
                    Installed::InUse => Some(UpdaterNotice::InUse),
                    Installed::Failed => Some(UpdaterNotice::Failed),
                    Installed::RestoreFailed { renames } => Some(UpdaterNotice::RestoreFailed {
                        renames: notice_renames(renames),
                    }),
                    Installed::Blocked => {
                        self.keep_record(
                            Some(UpdateRecord {
                                version,
                                reason: UpdateRecordReason::Blocked,
                            }),
                            polled,
                        );
                        Some(UpdaterNotice::Blocked)
                    }
                };
                self.set_notice(notice);
                if std::mem::take(&mut self.manual_waiting) {
                    self.check(Trigger::Manual);
                }
            }
            Event::Started(start, undone) => {
                self.work = Work::Idle;
                let Some(undone) = undone else {
                    if let Start::Signalled { pid } = start {
                        self.env.log.log(|| {
                            format!("updater: process {pid} signalled its start, quitting")
                        });
                        polled.quit = true;
                    }
                    return;
                };
                match start {
                    Start::Failed(error)
                        if files::is_blocked(&error)
                            || error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        self.start_failed(
                            &error.to_string(),
                            UpdateRecordReason::Blocked,
                            undone,
                            polled,
                        );
                    }
                    other => {
                        let why = match other {
                            Start::Exited { pid, code } => {
                                format!("process {pid} ended ({code:?}) before its start signal")
                            }
                            Start::TimedOut { pid } => format!(
                                "process {pid} gave no start signal in {} ms and was ended",
                                self.env.start_wait.as_millis()
                            ),
                            Start::Failed(error) => format!("the exe didn't start ({error})"),
                            Start::Signalled { pid } => {
                                format!("process {pid} signalled, yet the swap was undone")
                            }
                        };
                        self.start_failed(&why, UpdateRecordReason::FailedStart, undone, polled);
                    }
                }
            }
        }
    }

    /// A button of the updater's notice was pressed.
    pub fn press(&mut self, button: UpdaterButton) -> Action {
        self.env.log.log(|| format!("updater: {button:?} pressed"));
        match button {
            UpdaterButton::TryAgain if self.notice == Some(UpdaterNotice::Unreachable) => {
                self.check(Trigger::Manual);
                Action::Nothing
            }
            UpdaterButton::Download | UpdaterButton::TryAgain => {
                self.install();
                Action::Nothing
            }
            UpdaterButton::NotNow => {
                self.notice = None;
                Action::Nothing
            }
            UpdaterButton::Cancel => {
                self.cancel_install();
                Action::Nothing
            }
            UpdaterButton::OpenReleasePage => Action::OpenReleasePage,
            UpdaterButton::RestartNow => {
                self.restart();
                Action::Nothing
            }
        }
    }

    /// Answers a Cancel at once; the worker stops at its next byte or step,
    /// and a connect or stall still waiting runs into its timeout unseen.
    fn cancel_install(&mut self) {
        if self.work != Work::Installing {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.env
            .log
            .log(|| "updater: install cancelled by the user".to_owned());
        self.notice = None;
    }

    fn install(&mut self) {
        let Some(update) = self.offered.clone() else {
            return;
        };
        if self.work != Work::Idle || self.swapped.is_some() {
            return;
        }
        self.work = Work::Installing;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.set_notice(Some(UpdaterNotice::Downloading { percent: None }));
        let env = self.env.clone();
        let sender = self.sender.clone();
        let cancel = self.cancel.clone();
        self.spawn(move || {
            let progress_sender = sender.clone();
            let progress = move |percent| {
                let _ = progress_sender.send(Event::Progress(percent));
            };
            let installed = flow::install(&env, &update, &progress, &cancel);
            let _ = sender.send(Event::Installed(update.version_text.clone(), installed));
        });
    }

    /// Starts the installed exe on the worker thread, which waits for its
    /// start signal while streaming goes on (spec S303, S314).
    fn restart(&mut self) {
        let Some((_, swapped)) = self.swapped.clone() else {
            return;
        };
        if self.work != Work::Idle {
            return;
        }
        let exe = self.env.install_dir.join(archive::EXE_NAME);
        self.work = Work::Restarting;
        let env = self.env.clone();
        let sender = self.sender.clone();
        let undo_closed = self.undo_closed.clone();
        #[cfg(test)]
        let after_undo = self.after_undo.clone();
        self.spawn(move || {
            // An installed exe gone since the swap was removed by Windows
            // security (spec S315).
            let start = if exe.exists() {
                env.launch.start(&exe, env.start_wait)
            } else {
                Start::Failed(std::io::Error::from(std::io::ErrorKind::NotFound))
            };
            if matches!(start, Start::Signalled { .. }) {
                let _ = sender.send(Event::Started(start, None));
                return;
            }
            let closed = undo_closed.lock().unwrap_or_else(PoisonError::into_inner);
            if *closed {
                return;
            }
            let undone = undo(&env, &swapped);
            // Sent before the guard goes, so a quit that takes the guard
            // finds the result.
            let _ = sender.send(Event::Started(start, Some(undone)));
            drop(closed);
            #[cfg(test)]
            if let Some(after_undo) = after_undo {
                after_undo();
            }
        });
    }

    /// Why the stored version didn't install last time, when `update` is
    /// that version; a higher one clears the record (spec S316).
    fn last_time(&mut self, update: &Update, polled: &mut Polled) -> Option<UpdateRecordReason> {
        let record = self.record.as_ref()?;
        match Version::parse(&record.version) {
            Some(stored) if stored == update.version => Some(record.reason),
            Some(stored) if stored > update.version => None,
            _ => {
                self.keep_record(None, polled);
                None
            }
        }
    }

    fn keep_record(&mut self, record: Option<UpdateRecord>, polled: &mut Polled) {
        if self.record != record {
            self.env.log.log(|| format!("updater: record {record:?}"));
            self.record = record;
            polled.record_changed = true;
        }
    }

    /// The new exe couldn't be started or gave no start signal, and `undone`
    /// deleted the new files and renamed each `.bak` back (spec S303).
    fn start_failed(
        &mut self,
        why: &str,
        reason: UpdateRecordReason,
        undone: Undone,
        polled: &mut Polled,
    ) {
        self.env
            .log
            .log(|| format!("updater: restart failed ({why})"));
        let Some((version, _)) = self.swapped.take() else {
            return;
        };
        self.keep_record(Some(UpdateRecord { version, reason }), polled);
        for (step, code) in &undone.steps {
            self.env
                .log
                .log(|| format!("updater: {step} (error {code})"));
        }
        self.set_notice(Some(match undone.result {
            Err(swap::SwapError::RestoreFailed { renames }) => UpdaterNotice::RestoreFailed {
                renames: notice_renames(renames),
            },
            _ if reason == UpdateRecordReason::Blocked => UpdaterNotice::Blocked,
            _ => UpdaterNotice::RelaunchFailed,
        }));
    }

    /// At quit, after a swap: a failed restart's undo still running ends
    /// first (spec S303), and an exe Windows security removed gets the
    /// `.bak` back, so the next start finds one (spec S315). Lock retries
    /// share [`files::QUIT_RETRY_BUDGET`]. True when the record changed.
    pub fn at_quit(&mut self) -> bool {
        self.env.pause.quit();
        let mut polled = Polled::default();
        if self.work == Work::Restarting {
            self.finish_restart(&mut polled);
        }
        let Some((_, swapped)) = &self.swapped else {
            return polled.record_changed;
        };
        if self.env.install_dir.join(archive::EXE_NAME).exists() {
            return polled.record_changed;
        }
        let undone = undo(&self.env, swapped);
        self.start_failed(
            "the installed exe is gone at quit",
            UpdateRecordReason::Blocked,
            undone,
            &mut polled,
        );
        polled.record_changed
    }

    /// Waits up to [`QUIT_WAIT`] for the restart's worker; past that, a
    /// running undo still ends, but none starts any more.
    fn finish_restart(&mut self, polled: &mut Polled) {
        let deadline = std::time::Instant::now() + QUIT_WAIT;
        while self.work == Work::Restarting {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.events.recv_timeout(left) {
                Ok(event) => self.apply(event, polled),
                Err(_) => break,
            }
        }
        if self.work != Work::Restarting {
            return;
        }
        *self
            .undo_closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        while let Ok(event) = self.events.try_recv() {
            self.apply(event, polled);
        }
        if self.work == Work::Restarting {
            self.env
                .log
                .log(|| "updater: quit while the new version's start was awaited".to_owned());
        }
    }

    /// Closing the notice of a running download (its X or Dismiss All)
    /// cancels the download, so the closed notice doesn't come back.
    pub fn dismiss(&mut self) {
        if matches!(self.notice, Some(UpdaterNotice::Downloading { .. })) {
            self.cancel_install();
        }
        self.notice = None;
    }

    #[cfg(test)]
    fn wait(&mut self) -> Polled {
        let mut polled = Polled::default();
        while self.work != Work::Idle {
            let event = self
                .events
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("the worker reports");
            polled.changed = true;
            self.apply(event, &mut polled);
        }
        polled
    }
}

fn notice_renames(renames: Vec<swap::Rename>) -> Vec<(String, String)> {
    renames
        .into_iter()
        .map(|rename| (rename.bak, rename.live))
        .collect()
}

/// The running exe's folder, links followed.
pub fn install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(PathBuf::from)
}

/// The updater of this exe, or `None` when it doesn't update itself.
pub fn for_this_exe(log: DebugLog) -> Option<Updater> {
    for_exe(
        log,
        enabled(),
        crate::package::current(),
        install_dir,
        std::env::temp_dir(),
    )
}

/// Whether the updater runs: in a release build only, and never from a
/// package, whose files Windows keeps read-only and checks for tampering.
fn runs(release: bool, identity: &crate::package::Identity, log: &DebugLog) -> bool {
    let _ = (identity, log);
    release
}

/// The updater for a release flag and package identity. The leftover sweep
/// runs only past the gate: a packaged run leaves the unpackaged copy's
/// `.new` files and update folders alone.
fn for_exe(
    log: DebugLog,
    release: bool,
    identity: &crate::package::Identity,
    install_dir: impl FnOnce() -> Option<PathBuf>,
    temp_root: PathBuf,
) -> Option<Updater> {
    if !runs(release, identity, &log) {
        return None;
    }
    let install_dir = install_dir()?;
    let arch = Arch::own();
    let running = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let system = crate::platform::windows_version()
        .map(|v| Version::from_parts(&[v.major.into(), v.minor.into(), v.build.into()]))
        .unwrap_or_else(|| Version::from_parts(&[10, 0, 22000]));
    flow::remove_leftovers(&install_dir, &temp_root, &log);
    Some(Updater::new(Env {
        feed: FEED_URL.to_owned(),
        fetch: Arc::new(fetch::WinHttp {
            user_agent: fetch::user_agent(env!("CARGO_PKG_VERSION"), arch.name()),
            plain_loopback: false,
        }),
        files: Arc::new(files::RealFiles),
        temp_root,
        install_dir,
        key: signature::key(signature::RELEASE_KEY)?,
        arch,
        running,
        system,
        max_download: flow::MAX_DOWNLOAD,
        log,
        launch: Arc::new(relaunch::ProcessLaunch),
        start_wait: relaunch::START_WAIT,
        pause: Arc::new(files::Pause::real()),
    }))
}

#[cfg(test)]
pub mod tests {
    use std::path::{Path, PathBuf};

    use std::sync::Mutex;

    use super::*;
    use crate::updater::fetch::tests::MemoryFetch;
    use crate::updater::fetch::{Fetch, FetchError, Kind, Sink};
    use crate::updater::files::RealFiles;
    use crate::updater::flow::tests::fixture;

    /// A new, empty folder for the updater test `name` in the target folder.
    pub fn test_dir(name: &str) -> PathBuf {
        let exe = std::env::current_exe().expect("the test executable has a path");
        let dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("the test executable is in the target folder")
            .join("updater-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the test folder can be made");
        dir
    }

    fn updater(name: &str) -> (Updater, Arc<MemoryFetch>) {
        let f = fixture(name, false, Arc::new(RealFiles), DebugLog::default());
        (Updater::new(f.env), f.fetch)
    }

    /// Holds the fetch of one kind after its first byte until the test
    /// opens the gate, so the test can act while it runs.
    struct GatedFetch {
        inner: Arc<MemoryFetch>,
        kind: Kind,
        reached: Mutex<mpsc::Sender<()>>,
        gate: Mutex<mpsc::Receiver<()>>,
    }

    impl Fetch for GatedFetch {
        fn get(&self, url: &str, kind: Kind, sink: &mut Sink<'_>) -> Result<(), FetchError> {
            if kind != self.kind {
                return self.inner.get(url, kind, sink);
            }
            let mut first = true;
            self.inner.get(url, kind, &mut |bytes: &[u8]| {
                if !std::mem::take(&mut first) {
                    return sink(bytes);
                }
                let (head, tail) = bytes.split_at(1);
                if !sink(head) {
                    return false;
                }
                let _ = self.reached.lock().unwrap().send(());
                let _ = self.gate.lock().unwrap().recv();
                sink(tail)
            })
        }
    }

    struct Gated {
        updater: Updater,
        fetch: Arc<MemoryFetch>,
        reached: mpsc::Receiver<()>,
        gate: mpsc::Sender<()>,
    }

    fn gated(name: &str, kind: Kind) -> Gated {
        let mut f = fixture(name, false, Arc::new(RealFiles), DebugLog::default());
        let (reached_sender, reached) = mpsc::channel();
        let (gate, gate_receiver) = mpsc::channel();
        f.env.fetch = Arc::new(GatedFetch {
            inner: f.fetch.clone(),
            kind,
            reached: Mutex::new(reached_sender),
            gate: Mutex::new(gate_receiver),
        });
        Gated {
            updater: Updater::new(f.env),
            fetch: f.fetch,
            reached,
            gate,
        }
    }

    const WAIT: std::time::Duration = std::time::Duration::from_secs(20);

    // covers: S298 (Cancel and closing the notice)
    #[test]
    fn cancel_during_the_download_answers_at_once_and_changes_nothing() {
        let Gated {
            mut updater,
            reached,
            gate,
            ..
        } = gated("cancel-download", Kind::Download);
        updater.check(Trigger::Manual);
        updater.wait();
        let dir = updater.env.install_dir.clone();
        let temp_root = updater.env.temp_root.clone();
        let untouched = |updater: &Updater| {
            assert_eq!(updater.notice(), None);
            assert_eq!(
                std::fs::read(dir.join(archive::EXE_NAME)).unwrap(),
                b"old exe"
            );
            let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
            assert_eq!(left.len(), 1, "only the old exe");
            assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
        };

        updater.press(UpdaterButton::Download);
        reached.recv_timeout(WAIT).expect("the download starts");
        assert_eq!(updater.press(UpdaterButton::Cancel), Action::Nothing);
        assert_eq!(updater.notice(), None, "Cancel answers before the worker");
        updater.poll();
        assert_eq!(updater.notice(), None, "progress after Cancel stays hidden");
        gate.send(()).unwrap();
        updater.wait();
        untouched(&updater);

        // Closing the download's notice cancels it the same way.
        updater.press(UpdaterButton::Download);
        reached
            .recv_timeout(WAIT)
            .expect("the download starts again");
        updater.poll();
        assert!(matches!(
            updater.notice(),
            Some(UpdaterNotice::Downloading { .. })
        ));
        updater.dismiss();
        gate.send(()).unwrap();
        updater.wait();
        untouched(&updater);
    }

    // covers: S298
    #[test]
    fn a_manual_check_right_after_cancel_is_answered_once_the_download_stops() {
        let Gated {
            mut updater,
            fetch,
            reached,
            gate,
        } = gated("check-after-cancel", Kind::Download);
        updater.check(Trigger::Manual);
        updater.wait();
        updater.press(UpdaterButton::Download);
        reached.recv_timeout(WAIT).expect("the download starts");
        updater.press(UpdaterButton::Cancel);
        updater.check(Trigger::Manual);
        assert_eq!(updater.notice(), Some(&UpdaterNotice::Checking));
        gate.send(()).unwrap();
        updater.wait();
        assert!(matches!(
            updater.notice(),
            Some(UpdaterNotice::Available { .. })
        ));
        let appcasts = fetch
            .asked
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.ends_with(".xml"))
            .count();
        assert_eq!(appcasts, 2, "the check ran after the download stopped");
        assert_eq!(
            std::fs::read(updater.env.install_dir.join(archive::EXE_NAME)).unwrap(),
            b"old exe"
        );
    }

    #[test]
    fn a_download_percentage_keeps_the_notice_time() {
        let (mut updater, _) = updater("notice-time");
        updater.set_notice(Some(UpdaterNotice::Downloading { percent: None }));
        updater.notice_time = "earlier".to_owned();
        updater.set_notice(Some(UpdaterNotice::Downloading { percent: Some(40) }));
        assert_eq!(updater.notice_time(), "earlier");
        updater.set_notice(Some(UpdaterNotice::DownloadFailed));
        assert_ne!(updater.notice_time(), "earlier");
    }

    // covers: S305
    #[test]
    fn a_manual_check_during_the_launch_check_gets_its_answer() {
        let Gated {
            mut updater,
            fetch,
            reached,
            gate,
        } = gated("manual-during-launch", Kind::Appcast);
        Arc::get_mut(&mut updater.env).unwrap().running = Version::parse("0.9.0").unwrap();
        updater.check(Trigger::Launch);
        reached
            .recv_timeout(WAIT)
            .expect("the launch check fetches");
        updater.check(Trigger::Manual);
        gate.send(()).unwrap();
        updater.wait();
        assert_eq!(
            updater.notice(),
            Some(&UpdaterNotice::UpToDate {
                running: "0.9.0".to_owned()
            })
        );
        assert_eq!(
            fetch.asked.lock().unwrap().len(),
            1,
            "one fetch answers both"
        );
    }

    // covers: S293
    #[test]
    fn only_a_release_build_on_windows_checks() {
        assert!(!enabled());
        assert!(marked(Some("1")));
        for other in [None, Some(""), Some("0"), Some("true")] {
            assert!(!marked(other), "{other:?}");
        }
        assert!(for_this_exe(DebugLog::default()).is_none());
        let plist =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../Info.plist"))
                .expect("Info.plist is readable");
        assert!(plist.contains(&format!("<string>{FEED_URL}</string>")));
    }

    #[test]
    fn a_packaged_release_build_never_updates_or_sweeps() {
        use crate::package::Identity;
        let name = "AudioSync_0.9.0.0_x64__abcdefghjkmnp";
        let packaged = Identity::Packaged(name.to_owned());
        let seed = |dir: &Path| {
            let install = dir.join("install");
            let temp = dir.join("temp");
            std::fs::create_dir_all(&install).unwrap();
            std::fs::write(install.join("audiosync.exe.new"), b"x").unwrap();
            std::fs::create_dir_all(temp.join("AudioSync-update-abc")).unwrap();
            (install, temp)
        };

        let dir = test_dir("packaged-off");
        let (install, temp) = seed(&dir);
        let path = dir.join("debug.log");
        let log = DebugLog::new(Some(path.clone()));
        assert!(!runs(true, &packaged, &log));
        let built = for_exe(
            log.clone(),
            true,
            &packaged,
            || Some(install.clone()),
            temp.clone(),
        );
        assert!(built.is_none());
        assert!(
            install.join("audiosync.exe.new").exists(),
            "the sweep didn't run"
        );
        assert!(
            temp.join("AudioSync-update-abc").exists(),
            "the sweep didn't run"
        );
        assert!(log.flush(crate::debug_log::TEST_FLUSH));
        let lines = crate::debug_log::read_messages(&path);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("updater: off") && line.contains(name)),
            "{lines:?}"
        );

        assert!(!runs(false, &Identity::Unpackaged, &DebugLog::default()));
        assert!(runs(true, &Identity::Unpackaged, &DebugLog::default()));
        let dir = test_dir("unpackaged-on");
        let (install, temp) = seed(&dir);
        let built = for_exe(
            DebugLog::default(),
            true,
            &Identity::Unpackaged,
            || Some(install.clone()),
            temp.clone(),
        );
        assert!(built.is_some());
        assert!(!install.join("audiosync.exe.new").exists(), "the sweep ran");
    }

    // covers: S305
    #[test]
    fn offline_at_launch_is_quiet_and_a_manual_check_says_so() {
        let (mut updater, _) = updater("offline-quiet");
        Arc::get_mut(&mut updater.env).unwrap().feed = "https://offline.invalid/a.xml".to_owned();
        updater.check(Trigger::Launch);
        assert_eq!(
            updater.wait(),
            Polled {
                changed: true,
                checked: false,
                ..Polled::default()
            }
        );
        assert_eq!(updater.notice(), None);
        updater.check(Trigger::Manual);
        updater.wait();
        assert_eq!(updater.notice(), Some(&UpdaterNotice::Unreachable));
        assert!(!updater.notice_time().is_empty());
        assert_eq!(
            updater.notice().unwrap().buttons(),
            [UpdaterButton::TryAgain]
        );
        updater.press(UpdaterButton::TryAgain);
        assert_eq!(updater.work, Work::Checking, "Try Again checks again");
        updater.wait();
        assert_eq!(updater.notice(), Some(&UpdaterNotice::Unreachable));
    }

    // covers: S305
    #[test]
    fn a_manual_check_says_it_is_checking_until_the_answer_replaces_it() {
        let (mut updater, _) = updater("checking");
        Arc::get_mut(&mut updater.env).unwrap().running = Version::parse("0.9.0").unwrap();
        let latest = Some(UpdaterNotice::UpToDate {
            running: "0.9.0".to_owned(),
        });
        updater.check(Trigger::Launch);
        assert_eq!(updater.notice(), None, "the launch check stays quiet");
        updater.wait();
        updater.check(Trigger::Manual);
        assert_eq!(updater.notice(), Some(&UpdaterNotice::Checking));
        assert!(updater.notice().unwrap().buttons().is_empty());
        updater.wait();
        assert_eq!(updater.notice(), latest.as_ref());

        // One asked while the launch check runs says so too.
        updater.dismiss();
        updater.check(Trigger::Launch);
        updater.check(Trigger::Manual);
        assert_eq!(updater.notice(), Some(&UpdaterNotice::Checking));
        updater.wait();
        assert_eq!(updater.notice(), latest.as_ref());
    }

    // covers: S305
    #[test]
    fn a_manual_check_with_nothing_newer_says_the_running_version_is_the_latest() {
        let (mut updater, _) = updater("up-to-date");
        Arc::get_mut(&mut updater.env).unwrap().running = Version::parse("0.9.0").unwrap();
        updater.check(Trigger::Launch);
        assert_eq!(
            updater.wait(),
            Polled {
                changed: true,
                checked: true,
                ..Polled::default()
            }
        );
        assert_eq!(updater.notice(), None);
        updater.check(Trigger::Manual);
        updater.wait();
        assert_eq!(
            updater.notice(),
            Some(&UpdaterNotice::UpToDate {
                running: "0.9.0".to_owned()
            })
        );
    }

    // covers: S297, S303, S306
    #[test]
    fn a_check_after_a_swap_without_a_restart_shows_restart_now_and_touches_no_file() {
        let (mut updater, fetch) = updater("swapped");
        updater.check(Trigger::Launch);
        updater.wait();
        assert_eq!(
            updater.notice(),
            Some(&UpdaterNotice::Available {
                new: "0.9.0".to_owned(),
                running: "0.8.0".to_owned(),
                last: None,
            })
        );
        assert_eq!(
            updater.notice().unwrap().buttons(),
            [UpdaterButton::Download, UpdaterButton::NotNow]
        );
        assert_eq!(updater.press(UpdaterButton::Download), Action::Nothing);
        updater.wait();
        let installed = Some(UpdaterNotice::Installed {
            version: "0.9.0".to_owned(),
        });
        assert_eq!(updater.notice(), installed.as_ref());
        let asked = fetch.asked.lock().unwrap().len();
        let exe = updater.env.install_dir.join(archive::EXE_NAME);
        let modified = std::fs::metadata(&exe).unwrap().modified().unwrap();

        updater.press(UpdaterButton::NotNow);
        for trigger in [Trigger::Manual, Trigger::Launch] {
            updater.check(trigger);
            assert_eq!(updater.notice(), installed.as_ref());
        }
        assert_eq!(fetch.asked.lock().unwrap().len(), asked, "nothing fetched");
        assert_eq!(
            std::fs::metadata(&exe).unwrap().modified().unwrap(),
            modified
        );
        Arc::get_mut(&mut updater.env).unwrap().launch = Arc::new(Signals);
        assert_eq!(updater.press(UpdaterButton::RestartNow), Action::Nothing);
        assert!(
            updater.wait().quit,
            "the new version signalled, so this one quits"
        );
        assert!(std::fs::read(&exe).unwrap().ends_with(b"new exe"));
    }

    // covers: S305
    #[test]
    fn turning_the_switch_off_stops_the_launch_check_not_the_manual_one() {
        let (mut updater, fetch) = updater("switch-off");
        updater.launch_check(false, None, 1_800_000_000);
        assert_eq!(updater.work, Work::Idle);
        assert!(fetch.asked.lock().unwrap().is_empty());
        updater.check(Trigger::Manual);
        updater.wait();
        assert_eq!(fetch.asked.lock().unwrap().len(), 1);
        assert!(matches!(
            updater.notice(),
            Some(UpdaterNotice::Available { .. })
        ));
    }

    // covers: S302, S306
    #[test]
    fn a_failed_install_reports_and_try_again_retries() {
        let (mut updater, _) = updater("try-again");
        updater.check(Trigger::Manual);
        updater.wait();
        let exe = updater.env.install_dir.join(archive::EXE_NAME);
        std::fs::write(updater.env.install_dir.join("audiosync.exe.new"), b"").unwrap();
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _held = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(updater.env.install_dir.join("audiosync.exe.new"))
                .unwrap();
            updater.press(UpdaterButton::Download);
            updater.wait();
            assert_eq!(updater.notice(), Some(&UpdaterNotice::InUse));
            assert_eq!(
                updater.notice().unwrap().buttons(),
                [UpdaterButton::TryAgain]
            );
        }
        updater.press(UpdaterButton::TryAgain);
        updater.wait();
        assert!(matches!(
            updater.notice(),
            Some(UpdaterNotice::Installed { .. })
        ));
        assert!(std::fs::read(exe).unwrap().ends_with(b"new exe"));
    }

    /// A new version that signals its start at once.
    struct Signals;

    impl relaunch::Launch for Signals {
        fn start(&self, _: &Path, _: std::time::Duration) -> Start {
            Start::Signalled { pid: 1 }
        }
    }

    /// Runs a stand-in for the new exe through the real wait for its signal.
    struct StandIn {
        command: fn() -> std::process::Command,
        old: u32,
    }

    impl relaunch::Launch for StandIn {
        fn start(&self, _: &Path, wait: std::time::Duration) -> Start {
            relaunch::start_and_wait(&mut (self.command)(), self.old, wait)
        }
    }

    /// A launch the test expects never to run.
    struct Never;

    impl relaunch::Launch for Never {
        fn start(&self, _: &Path, _: std::time::Duration) -> Start {
            panic!("nothing starts");
        }
    }

    /// An updater that has swapped 0.9.0 in and offers Restart Now.
    fn swapped(name: &str, launch: Arc<dyn relaunch::Launch>) -> Updater {
        let (mut updater, _) = updater(name);
        Arc::get_mut(&mut updater.env).unwrap().launch = launch;
        updater.check(Trigger::Manual);
        updater.wait();
        updater.press(UpdaterButton::Download);
        updater.wait();
        assert!(matches!(
            updater.notice(),
            Some(UpdaterNotice::Installed { .. })
        ));
        updater
    }

    fn folder(updater: &Updater) -> Vec<(String, Vec<u8>)> {
        let mut names: Vec<(String, Vec<u8>)> = std::fs::read_dir(&updater.env.install_dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        names.sort();
        names
    }

    fn old_folder() -> Vec<(String, Vec<u8>)> {
        vec![(archive::EXE_NAME.to_owned(), b"old exe".to_vec())]
    }

    fn record(version: &str, reason: UpdateRecordReason) -> Option<UpdateRecord> {
        Some(UpdateRecord {
            version: version.to_owned(),
            reason,
        })
    }

    // covers: S303, S314, S316
    #[test]
    fn a_new_exe_that_exits_or_never_signals_is_ended_and_the_bak_restored() {
        let exits = || {
            let mut command = std::process::Command::new("cmd.exe");
            command.args(["/c", "exit", "1"]);
            command
        };
        let silent = || relaunch::tests::sleeper(30);
        for (case, command, n) in [
            ("exits", exits as fn() -> std::process::Command, 4),
            ("silent", silent as fn() -> std::process::Command, 5),
        ] {
            let old = relaunch::tests::made_up_old(n);
            let mut updater = swapped(
                &format!("no-start-{case}"),
                Arc::new(StandIn { command, old }),
            );
            let pressed = std::time::Instant::now();
            assert_eq!(updater.press(UpdaterButton::RestartNow), Action::Nothing);
            assert!(
                pressed.elapsed() < std::time::Duration::from_millis(200),
                "{case}: the wait runs on the worker, so streaming goes on"
            );
            let polled = updater.wait();
            assert!(!polled.quit, "{case}");
            assert!(polled.record_changed, "{case}");
            assert_eq!(
                updater.notice(),
                Some(&UpdaterNotice::RelaunchFailed),
                "{case}"
            );
            assert_eq!(folder(&updater), old_folder(), "{case}");
            assert_eq!(
                updater.record(),
                record("0.9.0", UpdateRecordReason::FailedStart).as_ref(),
                "{case}"
            );
        }
    }

    // covers: S303, S315
    #[test]
    fn a_new_exe_gone_before_the_restart_restores_the_bak() {
        let mut updater = swapped("gone-before-restart", Arc::new(Never));
        std::fs::remove_file(updater.env.install_dir.join(archive::EXE_NAME)).unwrap();
        assert_eq!(updater.press(UpdaterButton::RestartNow), Action::Nothing);
        let polled = updater.wait();
        assert!(!polled.quit);
        assert_eq!(updater.notice(), Some(&UpdaterNotice::Blocked));
        assert_eq!(folder(&updater), old_folder());
        assert_eq!(
            updater.record(),
            record("0.9.0", UpdateRecordReason::Blocked).as_ref()
        );
    }

    // covers: S315
    #[test]
    fn quitting_after_a_swap_whose_exe_is_gone_renames_the_bak_back() {
        let mut kept = swapped("quit-exe-kept", Arc::new(Never));
        let before = folder(&kept);
        assert!(!kept.at_quit(), "an exe still there changes nothing");
        assert_eq!(folder(&kept), before);

        let mut updater = swapped("quit-exe-gone", Arc::new(Never));
        std::fs::remove_file(updater.env.install_dir.join(archive::EXE_NAME)).unwrap();
        assert!(updater.at_quit());
        assert_eq!(folder(&updater), old_folder());
        assert_eq!(
            updater.record(),
            record("0.9.0", UpdateRecordReason::Blocked).as_ref()
        );
    }

    // covers: S316
    #[test]
    fn a_failed_or_blocked_version_is_skipped_at_launch_offered_on_a_manual_check_and_cleared_by_a_newer_one()
     {
        for reason in [UpdateRecordReason::Blocked, UpdateRecordReason::FailedStart] {
            let (mut updater, _) = updater(&format!("record-{reason:?}"));
            updater.set_record(record("0.9.0", reason));
            updater.check(Trigger::Launch);
            let polled = updater.wait();
            assert!(polled.checked, "the appcast was read");
            assert_eq!(updater.notice(), None, "the launch check offers nothing");
            updater.check(Trigger::Manual);
            updater.wait();
            assert_eq!(
                updater.notice(),
                Some(&UpdaterNotice::Available {
                    new: "0.9.0".to_owned(),
                    running: "0.8.0".to_owned(),
                    last: Some(reason),
                })
            );
            assert_eq!(updater.record(), record("0.9.0", reason).as_ref());
            updater.press(UpdaterButton::Download);
            let polled = updater.wait();
            assert!(polled.record_changed);
            assert_eq!(updater.record(), None, "a completed install clears it");
        }

        let (mut updater, _) = updater("record-older");
        updater.set_record(record("0.8.5", UpdateRecordReason::Blocked));
        updater.check(Trigger::Launch);
        let polled = updater.wait();
        assert!(polled.record_changed);
        assert_eq!(updater.record(), None, "a higher version clears it");
        assert!(matches!(
            updater.notice(),
            Some(UpdaterNotice::Available { last: None, .. })
        ));

        // A bad signature stores nothing, so the next due check offers it.
        let f = fixture(
            "record-bad-signature",
            true,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        let mut updater = Updater::new(f.env);
        updater.check(Trigger::Launch);
        updater.wait();
        updater.press(UpdaterButton::Download);
        let polled = updater.wait();
        assert_eq!(updater.notice(), Some(&UpdaterNotice::BadSignature));
        assert!(!polled.record_changed);
        assert_eq!(updater.record(), None);
    }

    // covers: S315, S316
    #[test]
    fn a_download_that_vanishes_before_its_check_is_reported_as_blocked() {
        /// Real files, except that the finished download is gone before
        /// its check reads it.
        struct Vanishing;
        impl crate::updater::files::Files for Vanishing {
            fn create(
                &self,
                path: &Path,
                exclusive: bool,
            ) -> std::io::Result<Box<dyn crate::updater::files::Sink>> {
                RealFiles.create(path, exclusive)
            }
            fn remove(&self, path: &Path) -> std::io::Result<()> {
                RealFiles.remove(path)
            }
            fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
                RealFiles.rename(from, to)
            }
            fn open(&self, path: &Path) -> std::io::Result<std::fs::File> {
                if path.file_name().is_some_and(|name| name == "update.zip") {
                    let _ = std::fs::remove_file(path);
                }
                std::fs::File::open(path)
            }
        }
        let f = fixture("vanishing", false, Arc::new(Vanishing), DebugLog::default());
        let temp_root = f.env.temp_root.clone();
        let mut updater = Updater::new(f.env);
        updater.check(Trigger::Manual);
        updater.wait();
        updater.press(UpdaterButton::Download);
        let polled = updater.wait();
        assert_eq!(updater.notice(), Some(&UpdaterNotice::Blocked));
        assert!(polled.record_changed);
        assert_eq!(
            updater.record(),
            record("0.9.0", UpdateRecordReason::Blocked).as_ref()
        );
        assert_eq!(folder(&updater), old_folder());
        assert_eq!(std::fs::read_dir(temp_root).unwrap().count(), 0);
    }

    /// Real files, except that deleting `audiosync.exe` meets a sharing
    /// violation `locked` times, as while Defender still scans the new exe,
    /// and each try records the thread it ran on.
    struct Scanned {
        locked: Mutex<u32>,
        threads: Mutex<Vec<Option<String>>>,
    }

    impl crate::updater::files::Files for Scanned {
        fn create(
            &self,
            path: &Path,
            exclusive: bool,
        ) -> std::io::Result<Box<dyn crate::updater::files::Sink>> {
            RealFiles.create(path, exclusive)
        }
        fn remove(&self, path: &Path) -> std::io::Result<()> {
            if path
                .file_name()
                .is_some_and(|name| name == archive::EXE_NAME)
            {
                self.threads
                    .lock()
                    .unwrap()
                    .push(std::thread::current().name().map(str::to_owned));
                let mut locked = self.locked.lock().unwrap();
                if *locked > 0 {
                    *locked -= 1;
                    return Err(std::io::Error::from_raw_os_error(32));
                }
            }
            RealFiles.remove(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            RealFiles.rename(from, to)
        }
    }

    /// A new version that ends before its start signal.
    struct Exits;

    impl relaunch::Launch for Exits {
        fn start(&self, _: &Path, _: std::time::Duration) -> Start {
            Start::Exited {
                pid: 1,
                code: Some(1),
            }
        }
    }

    // covers: S303, S315
    #[test]
    fn a_new_exe_still_scanned_after_a_failed_restart_is_deleted_on_a_retry() {
        let files = Arc::new(Scanned {
            locked: Mutex::new(2),
            threads: Mutex::new(Vec::new()),
        });
        let waits = Arc::new(Mutex::new(Vec::new()));
        let mut f = fixture(
            "scanned-after-restart",
            false,
            files.clone(),
            DebugLog::default(),
        );
        let counted = waits.clone();
        f.env.pause = Arc::new(files::Pause::counted(move |wait| {
            counted.lock().unwrap().push(wait);
        }));
        f.env.launch = Arc::new(Exits);
        let mut updater = Updater::new(f.env);
        updater.check(Trigger::Manual);
        updater.wait();
        updater.press(UpdaterButton::Download);
        updater.wait();
        assert!(matches!(
            updater.notice(),
            Some(UpdaterNotice::Installed { .. })
        ));

        assert_eq!(updater.press(UpdaterButton::RestartNow), Action::Nothing);
        let polled = updater.wait();
        assert!(!polled.quit);
        assert_eq!(updater.notice(), Some(&UpdaterNotice::RelaunchFailed));
        assert_eq!(folder(&updater), old_folder());
        assert_eq!(
            *waits.lock().unwrap(),
            [
                std::time::Duration::from_millis(250),
                std::time::Duration::from_millis(500)
            ]
        );
        let threads = files.threads.lock().unwrap();
        assert_eq!(threads.len(), 3, "two locked tries, then the delete");
        assert!(
            threads
                .iter()
                .all(|name| name.as_deref() == Some("audiosync-updater")),
            "the undo runs on the worker, never on the UI thread: {threads:?}"
        );
    }

    /// An updater with `files` and the counting `sleep` that has swapped
    /// 0.9.0 in and offers Restart Now, which a new version that exits
    /// before its signal answers.
    fn swapped_with(
        name: &str,
        files: Arc<dyn crate::updater::files::Files>,
        sleep: impl Fn(std::time::Duration) + Send + Sync + 'static,
    ) -> Updater {
        let mut f = fixture(name, false, files, DebugLog::default());
        f.env.pause = Arc::new(files::Pause::counted(sleep));
        f.env.launch = Arc::new(Exits);
        let mut updater = Updater::new(f.env);
        updater.check(Trigger::Manual);
        updater.wait();
        updater.press(UpdaterButton::Download);
        updater.wait();
        assert!(matches!(
            updater.notice(),
            Some(UpdaterNotice::Installed { .. })
        ));
        updater
    }

    // covers: S303, S315
    #[test]
    fn a_quit_during_the_undo_of_a_failed_restart_waits_for_it_and_keeps_the_record() {
        let files = Arc::new(Scanned {
            locked: Mutex::new(3),
            threads: Mutex::new(Vec::new()),
        });
        let (entered_sender, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel::<()>();
        let (entered_sender, gate) = (Mutex::new(entered_sender), Mutex::new(gate));
        let mut updater = swapped_with("quit-during-undo", files, move |_| {
            let _ = entered_sender.lock().unwrap().send(());
            let _ = gate.lock().unwrap().recv();
        });
        assert_eq!(updater.press(UpdaterButton::RestartNow), Action::Nothing);
        entered
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the undo waits to retry the locked exe");
        drop(release);
        assert!(updater.at_quit(), "the quit stores the record");
        assert_eq!(
            updater.record(),
            record("0.9.0", UpdateRecordReason::FailedStart).as_ref()
        );
        assert_eq!(folder(&updater), old_folder());
    }

    /// Real files, except that renaming a `.bak` back always meets a
    /// sharing violation.
    struct LockedBaks;

    impl crate::updater::files::Files for LockedBaks {
        fn create(
            &self,
            path: &Path,
            exclusive: bool,
        ) -> std::io::Result<Box<dyn crate::updater::files::Sink>> {
            RealFiles.create(path, exclusive)
        }
        fn remove(&self, path: &Path) -> std::io::Result<()> {
            RealFiles.remove(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            if from.extension().is_some_and(|extension| extension == "bak") {
                return Err(std::io::Error::from_raw_os_error(32));
            }
            RealFiles.rename(from, to)
        }
    }

    // covers: S315
    #[test]
    fn the_undo_at_quit_spends_at_most_two_seconds_on_lock_retries() {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let counted = waits.clone();
        let mut updater = swapped_with("quit-retry-budget", Arc::new(LockedBaks), move |wait| {
            counted.lock().unwrap().push(wait);
        });
        std::fs::remove_file(updater.env.install_dir.join(archive::EXE_NAME)).unwrap();
        assert!(updater.at_quit());
        let spent: std::time::Duration = waits.lock().unwrap().iter().sum();
        assert!(
            spent <= std::time::Duration::from_secs(2),
            "one budget for the whole undo at quit, {spent:?}"
        );
        assert_eq!(
            updater.record(),
            record("0.9.0", UpdateRecordReason::Blocked).as_ref()
        );
    }

    // covers: S303, S316
    #[test]
    fn a_quit_right_after_the_undo_still_finds_its_result() {
        let (released_sender, released) = mpsc::channel();
        let (release, gate) = mpsc::channel::<()>();
        let (released_sender, gate) = (Mutex::new(released_sender), Mutex::new(gate));
        let mut updater = swapped_with("quit-after-undo", Arc::new(RealFiles), |_| {});
        updater.after_undo = Some(Arc::new(move || {
            let _ = released_sender.lock().unwrap().send(());
            let _ = gate
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(20));
        }));
        assert_eq!(updater.press(UpdaterButton::RestartNow), Action::Nothing);
        released
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the worker finished its undo");
        let changed = updater.at_quit();
        drop(release);
        assert!(changed, "the undo's result is there once its guard is free");
        assert_eq!(
            updater.record(),
            record("0.9.0", UpdateRecordReason::FailedStart).as_ref()
        );
        assert_eq!(folder(&updater), old_folder());
    }
}
