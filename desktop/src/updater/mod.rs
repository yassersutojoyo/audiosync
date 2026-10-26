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

/// Whether this exe updates itself: only a build the release CI job made,
/// which sets `AUDIOSYNC_RELEASE_BUILD` at compile time.
pub const fn enabled() -> bool {
    option_env!("AUDIOSYNC_RELEASE_BUILD").is_some() && !cfg!(test)
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

pub struct Updater {
    env: Arc<Env>,
    sender: Sender<Event>,
    events: Receiver<Event>,
    notice: Option<UpdaterNotice>,
    offered: Option<Update>,
    working: bool,
    cancel: Arc<AtomicBool>,
    swapped: Option<String>,
}

impl Updater {
    pub fn new(env: Env) -> Updater {
        let (sender, events) = mpsc::channel();
        Updater {
            env: Arc::new(env),
            sender,
            events,
            notice: None,
            offered: None,
            working: false,
            cancel: Arc::new(AtomicBool::new(false)),
            swapped: None,
        }
    }

    pub fn notice(&self) -> Option<&UpdaterNotice> {
        self.notice.as_ref()
    }

    /// Starts a check on the worker thread; after a swap it fetches nothing
    /// and shows Relaunch Now again.
    pub fn check(&mut self, trigger: Trigger) {
        self.env.log.log(|| format!("updater: {trigger:?} check"));
        if let Some(version) = &self.swapped {
            self.env
                .log
                .log(|| format!("updater: {version} is installed and waits for a relaunch"));
            self.notice = Some(UpdaterNotice::Installed {
                version: version.clone(),
            });
            return;
        }
        if self.working {
            self.env
                .log
                .log(|| "updater: already at work, check skipped".to_owned());
            return;
        }
        self.working = true;
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
                self.working = false;
                match checked {
                    Checked::Update(update) => {
                        polled.checked = true;
                        self.notice = Some(UpdaterNotice::Available {
                            new: update.version_text.clone(),
                            running: self.env.running.to_string(),
                        });
                        self.offered = Some(update);
                    }
                    Checked::UpToDate => {
                        polled.checked = true;
                        if trigger == Trigger::Manual {
                            self.notice = Some(UpdaterNotice::UpToDate {
                                running: self.env.running.to_string(),
                            });
                        }
                    }
                    Checked::Failed if trigger == Trigger::Manual => {
                        self.notice = Some(UpdaterNotice::Unreachable);
                    }
                    Checked::Failed => {}
                }
            }
            Event::Progress(percent) => {
                if self.working {
                    self.notice = Some(UpdaterNotice::Downloading { percent });
                }
            }
            Event::Installed(version, installed) => {
                self.working = false;
                self.notice = match installed {
                    Installed::Done => {
                        self.swapped = Some(version.clone());
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
                    Installed::RestoreFailed { bak } => Some(UpdaterNotice::RestoreFailed { bak }),
                };
            }
        }
    }

    /// A button of the updater's notice was pressed.
    pub fn press(&mut self, button: UpdaterButton) -> Action {
        self.env.log.log(|| format!("updater: {button:?} pressed"));
        match button {
            UpdaterButton::Download | UpdaterButton::TryAgain => {
                self.install();
                Action::Nothing
            }
            UpdaterButton::NotNow => {
                self.notice = None;
                Action::Nothing
            }
            UpdaterButton::Cancel => {
                self.cancel.store(true, Ordering::Relaxed);
                Action::Nothing
            }
            UpdaterButton::OpenReleasePage => Action::OpenReleasePage,
            UpdaterButton::RelaunchNow => match &self.swapped {
                Some(_) => Action::Relaunch(self.env.install_dir.join(archive::EXE_NAME)),
                None => Action::Nothing,
            },
        }
    }

    fn install(&mut self) {
        let Some(update) = self.offered.clone() else {
            return;
        };
        if self.working || self.swapped.is_some() {
            return;
        }
        self.working = true;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.notice = Some(UpdaterNotice::Downloading { percent: None });
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

    /// The new exe couldn't be started: it goes and the running `.bak` is
    /// renamed back (spec S303).
    pub fn relaunch_failed(&mut self, error: &std::io::Error) {
        self.env
            .log
            .log(|| format!("updater: relaunch failed ({error})"));
        let mut steps = swap::StepLog::new();
        let undone =
            swap::undo_completed(&self.env.install_dir, self.env.files.as_ref(), &mut steps);
        for (step, code) in &steps {
            self.env
                .log
                .log(|| format!("updater: {step} (error {code})"));
        }
        self.swapped = None;
        self.notice = Some(match undone {
            Err(swap::SwapError::RestoreFailed { bak }) => UpdaterNotice::RestoreFailed { bak },
            _ => UpdaterNotice::RelaunchFailed,
        });
    }

    pub fn dismiss(&mut self) {
        if !matches!(self.notice, Some(UpdaterNotice::Downloading { .. })) {
            self.notice = None;
        }
    }

    #[cfg(test)]
    fn wait(&mut self) -> Polled {
        let mut polled = Polled::default();
        while self.working {
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

    use super::*;
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

    fn updater(name: &str) -> (Updater, Arc<crate::updater::fetch::tests::MemoryFetch>) {
        let f = fixture(name, false, Arc::new(RealFiles), DebugLog::default());
        (Updater::new(f.env), f.fetch)
    }

    #[test]
    fn only_a_release_build_on_windows_checks() {
        assert!(!enabled());
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
    fn a_check_after_a_swap_without_a_relaunch_shows_relaunch_now_and_touches_no_file() {
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
            updater.press(UpdaterButton::RelaunchNow),
            Action::Relaunch(exe)
        );
    }

    #[test]
    fn turning_the_switch_off_stops_the_launch_check_not_the_manual_one() {
        let (mut updater, fetch) = updater("switch-off");
        updater.launch_check(false, None, 1_800_000_000);
        assert!(!updater.working);
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
