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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use appcast::{Arch, Update};
use flow::{Checked, Env, Installed};
use version::Version;

use crate::debug_log::DebugLog;
pub use crate::ui_model::{UpdaterButton, UpdaterNotice};

/// `SUFeedURL` in `Info.plist`.
pub const FEED_URL: &str =
    "https://github.com/yassersutojoyo/audiosync/releases/latest/download/appcast.xml";
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
    /// Stop streaming, save the settings, start this exe and quit.
    Relaunch(PathBuf),
}

enum Event {
    Checked(Trigger, Checked),
    Progress(Option<u8>),
    Installed(String, Installed),
}

/// What [`Updater::poll`] saw.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Polled {
    pub changed: bool,
    /// A check fetched and read the appcast, so its time is stored.
    pub checked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    Idle,
    Checking,
    Installing,
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
        }
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
                }
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
                        self.set_notice(Some(UpdaterNotice::Available {
                            new: update.version_text.clone(),
                            running: self.env.running.to_string(),
                        }));
                        self.offered = Some(update);
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
                };
                self.set_notice(notice);
                if std::mem::take(&mut self.manual_waiting) {
                    self.check(Trigger::Manual);
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
            UpdaterButton::RestartNow => match &self.swapped {
                Some(_) => Action::Relaunch(self.env.install_dir.join(archive::EXE_NAME)),
                None => Action::Nothing,
            },
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

    /// The new exe couldn't be started: the new files go and each `.bak`
    /// the swap made is renamed back (spec S303).
    pub fn relaunch_failed(&mut self, error: &std::io::Error) {
        self.env
            .log
            .log(|| format!("updater: restart failed ({error})"));
        let Some((_, swapped)) = self.swapped.take() else {
            return;
        };
        let mut steps = swap::StepLog::new();
        let undone = swap::undo_completed(
            &self.env.install_dir,
            &swapped,
            self.env.files.as_ref(),
            &mut steps,
        );
        for (step, code) in &steps {
            self.env
                .log
                .log(|| format!("updater: {step} (error {code})"));
        }
        self.set_notice(Some(match undone {
            Err(swap::SwapError::RestoreFailed { renames }) => UpdaterNotice::RestoreFailed {
                renames: notice_renames(renames),
            },
            _ => UpdaterNotice::RelaunchFailed,
        }));
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
    if !enabled() {
        return None;
    }
    let install_dir = install_dir()?;
    let arch = Arch::own();
    let running = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let system = crate::platform::windows_version()
        .map(|v| Version::from_parts(&[v.major.into(), v.minor.into(), v.build.into()]))
        .unwrap_or_else(|| Version::from_parts(&[10, 0, 22000]));
    let temp_root = std::env::temp_dir();
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
        assert_eq!(updater.notice(), None);
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
    fn offline_at_launch_is_quiet_and_a_manual_check_says_so() {
        let (mut updater, _) = updater("offline-quiet");
        Arc::get_mut(&mut updater.env).unwrap().feed = "https://offline.invalid/a.xml".to_owned();
        updater.check(Trigger::Launch);
        assert_eq!(
            updater.wait(),
            Polled {
                changed: true,
                checked: false
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

    #[test]
    fn a_manual_check_with_nothing_newer_says_the_running_version_is_the_latest() {
        let (mut updater, _) = updater("up-to-date");
        Arc::get_mut(&mut updater.env).unwrap().running = Version::parse("0.9.0").unwrap();
        updater.check(Trigger::Launch);
        assert_eq!(
            updater.wait(),
            Polled {
                changed: true,
                checked: true
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

    #[test]
    fn a_check_after_a_swap_without_a_restart_shows_restart_now_and_touches_no_file() {
        let (mut updater, fetch) = updater("swapped");
        updater.check(Trigger::Launch);
        updater.wait();
        assert_eq!(
            updater.notice(),
            Some(&UpdaterNotice::Available {
                new: "0.9.0".to_owned(),
                running: "0.8.0".to_owned()
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
        assert_eq!(
            updater.press(UpdaterButton::RestartNow),
            Action::Relaunch(exe)
        );
    }

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
}
