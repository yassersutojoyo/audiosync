//! Adding a silent output on a PC that has none
//! (docs/WINDOWS_SILENT_OUTPUT_SPEC.md): the double-audio notice offers VB-CABLE
//! by VB-Audio, which AudioSync downloads from the vendor when the user asks,
//! or when the installer runs [`INSTALLER_ARG`], checks against a pinned
//! SHA-256 and runs with the vendor's silent switches.
//! Once its endpoints are active, the default switch (S469) and the app move
//! (S471) take the source there as they take any other virtual output.
//!
//! [`run_pinned`] is the whole install over the [`Platform`] seam, which tests drive
//! with fakes; nothing in a test downloads, extracts to a real folder or
//! launches a setup.

// Only the Windows app installs it; Linux compiles the logic for its tests.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::ui_model::{SilentFailure, SilentOutput};

/// VB-CABLE's pack 46 on the vendor's server (VC1).
pub const PACK_URL: &str = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip";
/// Its SHA-256 on 2026-11-20 (VC2).
pub const PACK_SHA256: &str = "bc44e8974191a5a2427d6c979e2dab08dbb09925f3f435dfa2f0ba3e45b6eb90";
/// The 64-bit setup in the pack (VC3), and its silent switches (VC5, VC33).
pub const SETUP: &str = "VBCABLE_Setup_x64.exe";
pub const SWITCHES: &str = "-i -h";
/// What the installer passes to add the cable under its elevation (VC25).
pub const INSTALLER_ARG: &str = "--add-vb-cable";
/// The vendor's page, which the About credit links to (VC22).
pub const VENDOR_PAGE: &str = "https://vb-audio.com/Cable/";
/// The cable's hardware ID and device name (VC9).
pub const HARDWARE_ID: &str = "VBAudioVACWDM";
pub const DEVICE_NAME: &str = "VB-Audio Virtual Cable";
/// The download's limits (VC31).
pub const MAX_PACK: usize = 4 * 1024 * 1024;
/// How long the endpoints are waited for, from the UAC prompt, and after the
/// setup exited early (VC34).
pub const ENDPOINT_WAIT: Duration = Duration::from_secs(10 * 60);
pub const AFTER_EXIT: Duration = Duration::from_secs(60);
/// How long after the endpoints became active a default moved to the cable
/// is set back (VC40).
pub const GUARD: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_secs(1);
/// The folder the pack is extracted to, in the app's data folder or next to
/// the exe (VC37).
pub const FOLDER_NAME: &str = "VB-CABLE";

/// Whether a device is VB-CABLE: by its hardware IDs, or by its name when
/// they couldn't be read (VC45), never by an endpoint's name.
pub fn is_vb_cable(hardware_ids: &[String], device_name: Option<&str>) -> bool {
    if hardware_ids.is_empty() {
        return device_name.is_some_and(|name| name.trim().eq_ignore_ascii_case(DEVICE_NAME));
    }
    hardware_ids
        .iter()
        .any(|id| id.trim().eq_ignore_ascii_case(HARDWARE_ID))
}

/// What the double-audio notice offers (VC21, VC23): `missing` when it shows
/// because no silent endpoint exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Nothing,
    Button,
    /// An ARM64 PC: one honest line, no button.
    NotOnArm64,
}

pub fn offer(missing: bool, cable_present: bool, arm64: bool) -> Offer {
    match (missing && !cable_present, arm64) {
        (false, _) => Offer::Nothing,
        (true, true) => Offer::NotOnArm64,
        (true, false) => Offer::Button,
    }
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A default endpoint: render or capture, and its role (VC30).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub capture: bool,
    pub role: Role,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Console,
    Multimedia,
    Communications,
}

pub const SLOTS: [Slot; 6] = {
    const fn slot(capture: bool, role: Role) -> Slot {
        Slot { capture, role }
    }
    [
        slot(false, Role::Console),
        slot(false, Role::Multimedia),
        slot(false, Role::Communications),
        slot(true, Role::Console),
        slot(true, Role::Multimedia),
        slot(true, Role::Communications),
    ]
};

/// Why the setup didn't start.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// The UAC prompt was refused (`ERROR_CANCELLED`).
    Refused,
    Failed(String),
}

/// A running setup.
pub trait Setup: Send {
    fn exited(&mut self) -> bool;
}

/// Where the install meets Windows.
pub trait Platform: Send + Sync {
    /// Whether the PC is ARM64, also when the app runs emulated.
    fn arm64(&self) -> bool;
    /// Whether a VB-CABLE device exists, in any state.
    fn cable_present(&self) -> bool;
    /// The IDs of VB-CABLE's active endpoints, both flows.
    fn cable_endpoints(&self) -> Vec<String>;
    /// Whether an endpoint is active.
    fn endpoint_active(&self, id: &str) -> bool;
    /// The default endpoint of each of [`SLOTS`], in their order.
    fn defaults(&self) -> Vec<Option<String>>;
    fn set_default(&self, slot: Slot, id: &str) -> Result<(), String>;
    /// Keeps VC30's defaults on disk while an install may still move them;
    /// `None` deletes them (VC42).
    fn keep_before(&self, kept: Option<&Kept>) -> Result<(), String>;
    /// The record [`Self::keep_before`] kept.
    fn kept_before(&self) -> Result<Option<Kept>, String>;
    /// The pack from [`PACK_URL`], within the limits of VC31.
    fn download(&self) -> Result<Vec<u8>, String>;
    /// The folder the pack is extracted to: emptied, then held while the
    /// setup runs; the handles it returns keep each file from changing.
    fn unpack(&self, pack: &[u8]) -> Result<(PathBuf, Vec<std::fs::File>), String>;
    fn launch(&self, setup: &Path, switches: &str) -> Result<Box<dyn Setup>, Launch>;
    fn now(&self) -> Instant;
    fn pause(&self, duration: Duration);
}

/// Where an install is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    /// The setup was launched: the UAC prompt, then the silent install.
    Waiting,
}

/// Each default now on a cable endpoint that wasn't at VC30, with what it was.
fn stray<'a>(
    platform: &dyn Platform,
    before: &'a [Option<String>],
) -> Vec<(Slot, &'a str, String)> {
    let cable = platform.cable_endpoints();
    SLOTS
        .iter()
        .zip(before)
        .zip(platform.defaults())
        .filter_map(|((slot, was), is)| {
            let (Some(was), Some(is)) = (was, is) else {
                return None;
            };
            (cable.contains(&is) && !cable.contains(was) && platform.endpoint_active(was))
                .then_some((*slot, was.as_str(), is))
        })
        .collect()
}

/// Sets back each default the install moved to a cable endpoint (VC40).
fn put_back(platform: &dyn Platform, before: &[Option<String>]) -> Vec<String> {
    stray(platform, before)
        .into_iter()
        .map(|(slot, was, is)| {
            let result = platform.set_default(slot, was);
            format!(
                "silent output: {slot:?} default moved to {is} by the install, set back to \
                 {was}: {result:?}"
            )
        })
        .collect()
}

/// VC30's defaults and the install that read them (VC42).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Kept {
    install: u64,
    pub before: Vec<Option<String>>,
}

impl Kept {
    /// Names the install by the wall clock in nanoseconds, kept rising within
    /// the process so two installs never share a name.
    pub fn new(before: Vec<Option<String>>) -> Kept {
        static LAST: Mutex<u64> = Mutex::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as u64);
        let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
        *last = now.max(*last + 1);
        Kept {
            install: *last,
            before,
        }
    }
}

/// Held across writing a record and across [`restore_kept`]'s read and
/// delete, so the launch check never deletes a record Fix It just wrote.
static RECORD: Mutex<()> = Mutex::new(());

/// Whether the record on disk is still `kept`, not a later install's.
fn still_kept(platform: &dyn Platform, kept: &Kept) -> bool {
    platform
        .kept_before()
        .is_ok_and(|now| now.as_ref() == Some(kept))
}

/// The install of the pack whose SHA-256 is `pin` ([`PACK_SHA256`] but in
/// tests): `phase` hears where it is and `log` each line for the debug log.
/// A failure carries its cause for the log.
fn run_pinned(
    platform: &dyn Platform,
    pin: &str,
    phase: &mut dyn FnMut(Phase),
    log: &mut dyn FnMut(String),
) -> Result<(), (SilentFailure, String)> {
    let before = platform.defaults();
    log(format!(
        "silent output: defaults before the install {before:?}"
    ));
    let kept = Kept::new(before.clone());
    let written = {
        let _record = RECORD.lock().unwrap_or_else(PoisonError::into_inner);
        platform.keep_before(Some(&kept))
    };
    if let Err(why) = written {
        log(format!(
            "silent output: the defaults before the install couldn't be kept: {why}"
        ));
    }
    let result = run_kept(platform, pin, &before, phase, log);
    if let Err(why) = platform.keep_before(None) {
        log(format!(
            "silent output: the kept defaults couldn't be deleted: {why}"
        ));
    }
    result
}

/// [`run_pinned`] once VC30's defaults `before` are kept (VC42).
fn run_kept(
    platform: &dyn Platform,
    pin: &str,
    before: &[Option<String>],
    phase: &mut dyn FnMut(Phase),
    log: &mut dyn FnMut(String),
) -> Result<(), (SilentFailure, String)> {
    phase(Phase::Downloading);
    let pack = platform
        .download()
        .map_err(|why| (SilentFailure::Download, why))?;
    if sha256(&pack) != pin {
        return Err((
            SilentFailure::Mismatch,
            format!("the download ({} bytes) isn't the pinned pack", pack.len()),
        ));
    }
    // Running the setup with the cable present offers its removal (VC10).
    if platform.cable_present() {
        return Err((
            SilentFailure::Restart,
            "a VB-CABLE device appeared before the setup ran".to_owned(),
        ));
    }
    let (folder, _held) = platform
        .unpack(&pack)
        .map_err(|why| (SilentFailure::Installer, why))?;
    drop(pack);
    phase(Phase::Waiting);
    let started = platform.now();
    let mut setup = match platform.launch(&folder.join(SETUP), SWITCHES) {
        Ok(setup) => setup,
        Err(Launch::Refused) => {
            return Err((
                SilentFailure::Refused,
                "the UAC prompt was refused".to_owned(),
            ));
        }
        Err(Launch::Failed(why)) => return Err((SilentFailure::Installer, why)),
    };
    let mut deadline = started + ENDPOINT_WAIT;
    let mut exited = false;
    let active = loop {
        put_back(platform, before).into_iter().for_each(&mut *log);
        let now = platform.now();
        if !platform.cable_endpoints().is_empty() {
            break now;
        }
        if !exited && setup.exited() {
            exited = true;
            deadline = deadline.min(now + AFTER_EXIT);
            log("silent output: the setup exited, its endpoints aren't active yet".to_owned());
        }
        if now >= deadline {
            let why = format!("no active endpoint after {:?}", now - started);
            return Err(if platform.cable_present() {
                (SilentFailure::Restart, why)
            } else {
                (SilentFailure::Installer, why)
            });
        }
        platform.pause(POLL);
    };
    log(format!(
        "silent output: VB-CABLE's endpoints are active {:?} after the launch",
        active - started
    ));
    // The default switch looks at the cable only after this (VC41).
    while platform.now() < active + GUARD {
        platform.pause(POLL);
        put_back(platform, before).into_iter().for_each(&mut *log);
    }
    log(format!(
        "silent output: defaults after the install {:?}",
        platform.defaults()
    ));
    Ok(())
}

/// The install Fix It runs, for the installer step (VC25).
pub fn install(platform: &dyn Platform, log: &mut dyn FnMut(String)) -> Result<(), String> {
    run_pinned(platform, PACK_SHA256, &mut |_| {}, log)
        .map_err(|(failure, why)| format!("{failure:?}: {why}"))
}

/// At launch and in S474's guard: VC40 for an install whose process ended
/// before its own VC40 did (VC42). The setup may still be running, so this
/// lasts until the cable's endpoints have been active for [`GUARD`], or
/// [`ENDPOINT_WAIT`] when they never come.
pub fn restore_kept(platform: &dyn Platform, log: &mut dyn FnMut(String)) {
    let kept = match platform.kept_before() {
        Ok(Some(kept)) => kept,
        Ok(None) => return,
        Err(why) => {
            return log(format!(
                "silent output: the kept defaults couldn't be read: {why}"
            ));
        }
    };
    log("silent output: an install ended before its defaults were checked".to_owned());
    let before = &kept.before;
    let started = platform.now();
    let mut active_since = None;
    loop {
        if !still_kept(platform, &kept) {
            return log("silent output: a later install keeps the record now".to_owned());
        }
        put_back(platform, before).into_iter().for_each(&mut *log);
        let now = platform.now();
        let over = if platform.cable_endpoints().is_empty() {
            now >= started + ENDPOINT_WAIT
        } else {
            now >= *active_since.get_or_insert(now) + GUARD
        };
        if over {
            break;
        }
        platform.pause(POLL);
    }
    if !stray(platform, before).is_empty() {
        return log("silent output: a default is still on the cable, the record stays".to_owned());
    }
    let _record = RECORD.lock().unwrap_or_else(PoisonError::into_inner);
    if !still_kept(platform, &kept) {
        return;
    }
    if let Err(why) = platform.keep_before(None) {
        log(format!(
            "silent output: the kept defaults couldn't be deleted: {why}"
        ));
    }
}

/// Whether the command line is the installer's step (VC25).
pub fn installer_asks(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    args.into_iter()
        .nth(1)
        .is_some_and(|arg| arg == INSTALLER_ARG)
}

/// What a running install reports to the adapter.
enum Step {
    Phase(Phase),
    Log(String),
    Done(Result<(), (SilentFailure, String)>),
}

/// The install the notice drives: its state between polls, and the worker
/// thread while one runs.
#[derive(Default)]
pub struct SilentInstall {
    platform: Option<Arc<dyn Platform>>,
    worker: Option<Receiver<Step>>,
    phase: Option<Phase>,
    failed: Option<SilentFailure>,
    /// Installed: the cable is there, the notice's line asks for a restart
    /// until the source is heard once.
    installed: bool,
    /// The digest the pack must have: [`PACK_SHA256`] but in tests.
    pin: String,
}

/// What a poll found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Polled {
    pub changed: bool,
    pub lines: Vec<String>,
    /// The install just succeeded.
    pub added: bool,
}

impl SilentInstall {
    pub fn new(platform: Arc<dyn Platform>) -> SilentInstall {
        SilentInstall {
            platform: Some(platform),
            pin: PACK_SHA256.to_owned(),
            ..SilentInstall::default()
        }
    }

    /// An install that takes a pack whose SHA-256 is `pin`.
    #[cfg(test)]
    pub fn pinned(platform: Arc<dyn Platform>, pin: String) -> SilentInstall {
        SilentInstall {
            pin,
            ..SilentInstall::new(platform)
        }
    }

    /// What the double-audio notice shows; `missing` as in [`offer`].
    pub fn shown(&self, missing: bool) -> SilentOutput {
        let Some(platform) = self.platform.as_ref() else {
            return SilentOutput::None;
        };
        match (self.phase, self.failed) {
            (Some(Phase::Downloading), _) => return SilentOutput::Downloading,
            (Some(Phase::Waiting), _) => return SilentOutput::Waiting,
            (None, Some(SilentFailure::Restart)) => return SilentOutput::Restart,
            _ if self.installed => return SilentOutput::Restart,
            _ => {}
        }
        match offer(missing, platform.cable_present(), platform.arm64()) {
            Offer::Nothing => SilentOutput::None,
            Offer::NotOnArm64 => SilentOutput::NotOnArm64,
            Offer::Button => SilentOutput::Offered {
                failed: self.failed,
            },
        }
    }

    /// Whether an install succeeded, so the notice clearing means the
    /// source is heard once.
    pub fn installed(&self) -> bool {
        self.installed
    }

    /// Whether an install runs.
    pub fn running(&self) -> bool {
        self.worker.is_some()
    }

    pub fn forget_installed(&mut self) {
        self.installed = false;
    }

    /// Starts an install unless one runs (VC24) or the button isn't offered.
    pub fn start(&mut self, missing: bool) -> bool {
        if self.worker.is_some() || !matches!(self.shown(missing), SilentOutput::Offered { .. }) {
            return false;
        }
        let Some(platform) = self.platform.clone() else {
            return false;
        };
        let pin = self.pin.clone();
        let (send, receive) = channel();
        let spawned = std::thread::Builder::new()
            .name("silent output".to_owned())
            .spawn(move || {
                let phases = send.clone();
                let lines = send.clone();
                let result = run_pinned(
                    platform.as_ref(),
                    &pin,
                    &mut |phase| {
                        let _ = phases.send(Step::Phase(phase));
                    },
                    &mut |line| {
                        let _ = lines.send(Step::Log(line));
                    },
                );
                let _ = send.send(Step::Done(result));
            });
        if spawned.is_err() {
            self.failed = Some(SilentFailure::Installer);
            return false;
        }
        self.worker = Some(receive);
        self.failed = None;
        self.phase = Some(Phase::Downloading);
        true
    }

    /// Takes in what the install reported since the last poll.
    pub fn poll(&mut self) -> Polled {
        let mut polled = Polled::default();
        let Some(worker) = self.worker.as_ref() else {
            return polled;
        };
        loop {
            match worker.try_recv() {
                Ok(Step::Phase(phase)) => {
                    polled.changed |= self.phase != Some(phase);
                    self.phase = Some(phase);
                }
                Ok(Step::Log(line)) => polled.lines.push(line),
                Ok(Step::Done(result)) => {
                    self.worker = None;
                    self.phase = None;
                    polled.changed = true;
                    match result {
                        Ok(()) => {
                            self.installed = true;
                            polled.added = true;
                            polled
                                .lines
                                .push("silent output: VB-CABLE was added".to_owned());
                        }
                        Err((failure, why)) => {
                            self.failed = Some(failure);
                            polled
                                .lines
                                .push(format!("silent output: {failure:?}: {why}"));
                        }
                    }
                    return polled;
                }
                Err(TryRecvError::Empty) => return polled,
                Err(TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.phase = None;
                    self.failed = Some(SilentFailure::Installer);
                    polled.changed = true;
                    polled
                        .lines
                        .push("silent output: the install stopped without a result".to_owned());
                    return polled;
                }
            }
        }
    }

    /// Waits for a running install to finish, for tests.
    #[cfg(test)]
    pub fn finish(&mut self) -> Polled {
        let mut all = Polled::default();
        while self.worker.is_some() {
            let polled = self.poll();
            all.changed |= polled.changed;
            all.added |= polled.added;
            all.lines.extend(polled.lines);
            std::thread::yield_now();
        }
        all
    }
}

/// The pack through `fetch`, refusing more than [`MAX_PACK`] bytes (VC31).
#[cfg(any(windows, target_os = "linux"))]
pub fn fetch_pack(fetch: &dyn crate::updater::fetch::Fetch) -> Result<Vec<u8>, String> {
    use crate::updater::fetch::{FetchError, Kind};
    let mut pack = Vec::new();
    let mut over = false;
    let result = fetch.get(PACK_URL, Kind::Pinned, &mut |bytes| {
        if pack.len() + bytes.len() > MAX_PACK {
            over = true;
            return false;
        }
        pack.extend_from_slice(bytes);
        true
    });
    match (result, over) {
        (Err(FetchError::Stopped), true) => Err(format!("the pack is over {MAX_PACK} bytes")),
        (Err(error), _) => Err(format!("{error:?}")),
        (Ok(()), _) => Ok(pack),
    }
}

/// Extracts `pack` into `folder`, emptied first, and opens each file again
/// to hold it unchanged: compared byte for byte with the verified pack and
/// shared for reading only (VC32).
#[cfg(any(windows, target_os = "linux"))]
pub fn unpack_into(pack: &[u8], folder: &Path) -> Result<Vec<std::fs::File>, String> {
    use std::io::Read;

    let failed = |what: &str, error: &dyn std::fmt::Display| format!("{what}: {error}");
    if folder.exists() {
        std::fs::remove_dir_all(folder).map_err(|error| failed("emptying the folder", &error))?;
    }
    std::fs::create_dir_all(folder).map_err(|error| failed("creating the folder", &error))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(pack))
        .map_err(|error| failed("reading the pack", &error))?;
    let mut held = Vec::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| failed("reading an entry", &error))?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| format!("an entry outside the folder: {:?}", entry.name()))?;
        let path = folder.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&path).map_err(|error| failed("creating a folder", &error))?;
            continue;
        }
        total += entry.size();
        if total > 64 * MAX_PACK as u64 {
            return Err("the pack unpacks to too much".to_owned());
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| failed("unpacking an entry", &error))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| failed("creating a folder", &error))?;
        }
        std::fs::write(&path, &bytes).map_err(|error| failed("writing a file", &error))?;
        let mut file = read_only(&path).map_err(|error| failed("holding a file", &error))?;
        let mut written = Vec::new();
        file.read_to_end(&mut written)
            .map_err(|error| failed("reading a file back", &error))?;
        if written != bytes {
            return Err(format!("{} changed after it was written", path.display()));
        }
        held.push(file);
    }
    Ok(held)
}

/// Opens `path` for reading with no write or delete sharing.
#[cfg(any(windows, target_os = "linux"))]
fn read_only(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0);
    }
    options.open(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::*;

    const SPEAKERS: &str = "{0.0.0.00000000}.{speakers}";
    const MIC: &str = "{0.0.1.00000000}.{mic}";
    const HEADSET: &str = "{0.0.1.00000000}.{headset}";
    const CABLE_IN: &str = "{0.0.0.00000000}.{cable-in}";
    const CABLE_OUT: &str = "{0.0.1.00000000}.{cable-out}";
    const STAND_IN: &[u8] = b"a stand-in for the pack";

    /// The digest of the fake's pack, for [`SilentInstall::pinned`].
    pub(crate) fn stand_in_pin() -> String {
        sha256(STAND_IN)
    }

    /// What the fake Windows does, by the time since the setup launched.
    #[derive(Default)]
    pub(crate) struct World {
        now: Duration,
        launched_at: Option<Duration>,
        /// The cable's endpoints become active.
        active_at: Option<Duration>,
        /// The setup exits.
        exits_at: Option<Duration>,
        /// The cable device exists from then, with no endpoint unless
        /// `active_at` comes.
        device_at: Option<Duration>,
        device: bool,
        active: bool,
        /// Windows moves the render console and multimedia and the capture
        /// console defaults to the cable as it appears (VC8).
        moves_defaults: bool,
        /// Defaults the user changes: when, which slot, to what.
        user_moves: Vec<(Duration, usize, String)>,
        defaults: Vec<Option<String>>,
        sets: Vec<(Slot, String)>,
        unpacked: bool,
        launched: Vec<(PathBuf, String)>,
        /// What [`Platform::keep_before`] holds on disk.
        kept: Option<Kept>,
        /// The app's process ends then, as at a crash or a quit.
        ends_at: Option<Duration>,
        /// Fix It's install keeps its own record then.
        fix_it_at: Option<Duration>,
    }

    pub(crate) struct Fake {
        world: Arc<Mutex<World>>,
        pack: Result<Vec<u8>, String>,
        launch: fn() -> Result<(), Launch>,
        arm64: bool,
    }

    struct FakeSetup(Arc<Mutex<World>>);

    impl Setup for FakeSetup {
        fn exited(&mut self) -> bool {
            let world = self.0.lock().unwrap();
            world
                .launched_at
                .zip(world.exits_at)
                .is_some_and(|(at, exits)| world.now >= at + exits)
        }
    }

    pub(crate) fn fake() -> Fake {
        let speakers = Some(SPEAKERS.to_owned());
        let mic = Some(MIC.to_owned());
        let world = World {
            active_at: Some(Duration::from_secs(20)),
            defaults: vec![
                speakers.clone(),
                speakers.clone(),
                speakers,
                mic.clone(),
                mic.clone(),
                mic,
            ],
            ..World::default()
        };
        Fake {
            world: Arc::new(Mutex::new(world)),
            pack: Ok(STAND_IN.to_vec()),
            launch: || Ok(()),
            arm64: false,
        }
    }

    impl Fake {
        pub(crate) fn world(&self) -> MutexGuard<'_, World> {
            self.world.lock().unwrap()
        }

        /// The install with the stand-in pack as the pinned one.
        fn install(&self) -> Result<(), SilentFailure> {
            run_pinned(self, &sha256(STAND_IN), &mut |_| {}, &mut |_| {})
                .map_err(|(failure, _)| failure)
        }
    }

    impl Platform for Fake {
        fn arm64(&self) -> bool {
            self.arm64
        }
        fn cable_present(&self) -> bool {
            self.world().device
        }
        fn cable_endpoints(&self) -> Vec<String> {
            if self.world().active {
                vec![CABLE_IN.to_owned(), CABLE_OUT.to_owned()]
            } else {
                Vec::new()
            }
        }
        fn endpoint_active(&self, id: &str) -> bool {
            [SPEAKERS, MIC, HEADSET].contains(&id) || self.cable_endpoints().iter().any(|c| c == id)
        }
        fn defaults(&self) -> Vec<Option<String>> {
            self.world().defaults.clone()
        }
        fn set_default(&self, slot: Slot, id: &str) -> Result<(), String> {
            let mut world = self.world();
            let index = SLOTS.iter().position(|each| *each == slot).unwrap();
            world.defaults[index] = Some(id.to_owned());
            world.sets.push((slot, id.to_owned()));
            Ok(())
        }
        fn keep_before(&self, kept: Option<&Kept>) -> Result<(), String> {
            self.world().kept = kept.cloned();
            Ok(())
        }
        fn kept_before(&self) -> Result<Option<Kept>, String> {
            Ok(self.world().kept.clone())
        }
        fn download(&self) -> Result<Vec<u8>, String> {
            self.pack.clone()
        }
        fn unpack(&self, _pack: &[u8]) -> Result<(PathBuf, Vec<std::fs::File>), String> {
            self.world().unpacked = true;
            Ok((PathBuf::from("data").join(FOLDER_NAME), Vec::new()))
        }
        fn launch(&self, setup: &Path, switches: &str) -> Result<Box<dyn Setup>, Launch> {
            (self.launch)()?;
            let mut world = self.world();
            world
                .launched
                .push((setup.to_path_buf(), switches.to_owned()));
            world.launched_at = Some(world.now);
            Ok(Box::new(FakeSetup(self.world.clone())))
        }
        fn now(&self) -> Instant {
            static ORIGIN: OnceLock<Instant> = OnceLock::new();
            *ORIGIN.get_or_init(Instant::now) + self.world().now
        }
        fn pause(&self, duration: Duration) {
            let mut world = self.world();
            world.now += duration;
            if world.fix_it_at.is_some_and(|at| world.now >= at) {
                world.fix_it_at = None;
                world.kept = Some(Kept::new(world.defaults.clone()));
            }
            let Some(since) = world.launched_at.map(|at| world.now - at) else {
                return;
            };
            if world.device_at.is_some_and(|at| since >= at) {
                world.device = true;
            }
            if world.active_at.is_some_and(|at| since >= at) && !world.active {
                world.device = true;
                world.active = true;
                if world.moves_defaults {
                    world.defaults[0] = Some(CABLE_IN.to_owned());
                    world.defaults[1] = Some(CABLE_IN.to_owned());
                    world.defaults[3] = Some(CABLE_OUT.to_owned());
                }
            }
            if world.ends_at.is_some_and(|at| since >= at) {
                drop(world);
                panic!("the app's process ended mid-install");
            }
            let due: Vec<_> = world
                .user_moves
                .iter()
                .filter(|(at, ..)| since >= *at)
                .cloned()
                .collect();
            world.user_moves.retain(|(at, ..)| since < *at);
            for (_, index, id) in due {
                world.defaults[index] = Some(id);
            }
        }
    }

    // covers: VC42
    #[test]
    fn an_install_cut_short_leaves_its_defaults_for_the_next_launch_to_set_back() {
        let fake = fake();
        let before = fake.defaults();
        {
            let mut world = fake.world();
            world.moves_defaults = true;
            // Ends as Windows moves the defaults to the cable, before VC40.
            world.ends_at = Some(Duration::from_secs(20));
        }
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fake.install()));
        assert!(ended.is_err());
        assert_eq!(fake.defaults()[0].as_deref(), Some(CABLE_IN));
        assert_eq!(
            fake.world().kept.as_ref().map(|kept| &kept.before),
            Some(&before)
        );
        fake.world().ends_at = None;
        restore_kept(&fake, &mut |_| {});
        assert_eq!(fake.defaults(), before);
        assert_eq!(fake.world().kept, None);
    }

    // covers: VC42
    #[test]
    fn a_record_outlives_a_setup_that_adds_the_cable_after_the_app_ended() {
        let fake = fake();
        let before = fake.defaults();
        {
            let mut world = fake.world();
            world.moves_defaults = true;
            // Ends 10 s after the launch; the setup goes on, active at 20 s.
            world.ends_at = Some(Duration::from_secs(10));
        }
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fake.install()));
        assert!(ended.is_err());
        fake.world().ends_at = None;
        restore_kept(&fake, &mut |_| {});
        fake.pause(Duration::from_secs(20) + GUARD);
        assert_eq!(fake.defaults(), before);
        assert_eq!(fake.world().kept, None);
    }

    // covers: VC42
    #[test]
    fn a_fix_it_started_while_the_launch_check_runs_keeps_its_record() {
        let fake = fake();
        {
            let mut world = fake.world();
            world.kept = Some(Kept::new(world.defaults.clone()));
            world.fix_it_at = Some(Duration::from_secs(5));
        }
        restore_kept(&fake, &mut |_| {});
        assert!(fake.world().kept.is_some());
    }

    // covers: VC42
    #[test]
    fn an_install_that_ran_its_course_deletes_its_record() {
        let fake = fake();
        fake.world().moves_defaults = true;
        assert_eq!(fake.install(), Ok(()));
        assert_eq!(fake.world().kept, None);
    }

    // covers: VC1, VC2
    #[test]
    fn the_pin_is_pack_46_and_its_sha256() {
        assert_eq!(
            PACK_URL,
            "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip"
        );
        assert_eq!(
            PACK_SHA256,
            "bc44e8974191a5a2427d6c979e2dab08dbb09925f3f435dfa2f0ba3e45b6eb90"
        );
        assert_eq!(MAX_PACK, 4 * 1024 * 1024);
        assert_eq!(
            crate::updater::fetch::PINNED_TIMEOUT,
            Duration::from_secs(60)
        );
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // covers: VC32, VC35
    #[test]
    fn one_changed_byte_is_a_mismatch_that_extracts_and_launches_nothing() {
        assert_eq!(fake().install(), Ok(()));
        let mut changed = STAND_IN.to_vec();
        changed[0] ^= 1;
        let mut bad = fake();
        bad.pack = Ok(changed);
        assert_eq!(bad.install(), Err(SilentFailure::Mismatch));
        assert!(!bad.world().unpacked);
        assert!(bad.world().launched.is_empty());
        // The real pin refuses the stand-in.
        let real = fake();
        let result = run_pinned(&real, PACK_SHA256, &mut |_| {}, &mut |_| {})
            .map_err(|(failure, _)| failure);
        assert_eq!(result, Err(SilentFailure::Mismatch));
        assert!(!real.world().unpacked);
    }

    // covers: VC33
    #[test]
    fn the_setup_runs_silently_from_the_extracted_folder() {
        let fake = fake();
        assert_eq!(fake.install(), Ok(()));
        assert_eq!(
            fake.world().launched,
            vec![(
                PathBuf::from("data").join(FOLDER_NAME).join(SETUP),
                "-i -h".to_owned()
            )]
        );
        assert_eq!(SETUP, "VBCABLE_Setup_x64.exe");
    }

    // covers: VC21, VC35
    #[test]
    fn each_failure_has_its_cause() {
        let mut offline = fake();
        offline.pack = Err("offline".to_owned());
        assert_eq!(offline.install(), Err(SilentFailure::Download));
        assert!(offline.world().launched.is_empty());

        let mut refused = fake();
        refused.launch = || Err(Launch::Refused);
        assert_eq!(refused.install(), Err(SilentFailure::Refused));

        let mut broken = fake();
        broken.launch = || Err(Launch::Failed("no file".to_owned()));
        assert_eq!(broken.install(), Err(SilentFailure::Installer));

        // The setup would offer to remove a cable that appeared meanwhile.
        let present = fake();
        present.world().device = true;
        assert_eq!(present.install(), Err(SilentFailure::Restart));
        assert!(present.world().launched.is_empty());
    }

    // covers: VC34, VC35
    #[test]
    fn the_endpoints_decide_not_the_setup() {
        // Active while VB-Audio's window is still open: a success.
        let open = fake();
        open.world().active_at = Some(Duration::from_secs(90));
        assert_eq!(open.install(), Ok(()));

        // Nothing in 10 minutes: the installer failed.
        let never = fake();
        never.world().active_at = None;
        assert_eq!(never.install(), Err(SilentFailure::Installer));
        let waited = never.world().now;
        assert!(
            waited >= ENDPOINT_WAIT && waited <= ENDPOINT_WAIT + POLL,
            "{waited:?}"
        );

        // The window closed at 20 s with nothing installed: 60 s more.
        let closed = fake();
        closed.world().active_at = None;
        closed.world().exits_at = Some(Duration::from_secs(20));
        assert_eq!(closed.install(), Err(SilentFailure::Installer));
        let waited = closed.world().now;
        assert!(
            waited >= Duration::from_secs(80) && waited <= Duration::from_secs(81),
            "{waited:?}"
        );

        // A device with no active endpoint by then: restart to finish.
        let restart = fake();
        restart.world().active_at = None;
        restart.world().exits_at = Some(Duration::from_secs(20));
        restart.world().device_at = Some(Duration::from_secs(20));
        assert_eq!(restart.install(), Err(SilentFailure::Restart));
    }

    // covers: VC40, VC41
    #[test]
    fn only_defaults_the_install_moved_to_the_cable_are_set_back() {
        let fake = fake();
        fake.world().moves_defaults = true;
        // The user picks a headset microphone during the install: theirs.
        fake.world()
            .user_moves
            .push((Duration::from_secs(5), 4, HEADSET.to_owned()));
        assert_eq!(fake.install(), Ok(()));
        let world = fake.world();
        let speakers = Some(SPEAKERS.to_owned());
        let mic = Some(MIC.to_owned());
        assert_eq!(
            world.defaults,
            vec![
                speakers.clone(),
                speakers.clone(),
                speakers,
                mic.clone(),
                Some(HEADSET.to_owned()),
                mic,
            ]
        );
        let set: Vec<Slot> = world.sets.iter().map(|(slot, _)| *slot).collect();
        assert_eq!(set, vec![SLOTS[0], SLOTS[1], SLOTS[3]]);
        // `run` returns, and the switch may look at the cable, only after
        // the guard.
        assert!(world.now >= Duration::from_secs(20) + GUARD);
    }

    // covers: VC40
    #[test]
    fn a_default_goes_back_only_to_an_endpoint_that_still_exists() {
        let fake = fake();
        fake.world().moves_defaults = true;
        fake.world().defaults[0] = Some("{0.0.0.00000000}.{gone}".to_owned());
        assert_eq!(fake.install(), Ok(()));
        let world = fake.world();
        assert_eq!(world.defaults[0].as_deref(), Some(CABLE_IN));
        assert!(world.sets.iter().all(|(slot, _)| *slot != SLOTS[0]));
    }

    // covers: VC40
    #[test]
    fn a_move_to_the_cable_after_the_guard_is_left_alone() {
        let fake = fake();
        fake.world().user_moves.push((
            Duration::from_secs(20) + GUARD + POLL,
            0,
            CABLE_IN.to_owned(),
        ));
        assert_eq!(fake.install(), Ok(()));
        for _ in 0..5 {
            fake.pause(POLL);
        }
        let world = fake.world();
        assert_eq!(world.defaults[0].as_deref(), Some(CABLE_IN));
        assert!(world.sets.is_empty());
    }

    // covers: VC45
    #[test]
    fn vb_cable_is_known_by_its_hardware_id_then_its_device_name() {
        assert!(is_vb_cable(&["VBAudioVACWDM".to_owned()], Some("Speakers")));
        assert!(is_vb_cable(&["vbaudiovacwdm".to_owned()], None));
        assert!(!is_vb_cable(
            &["VBAudioVACWDM_A".to_owned()],
            Some(DEVICE_NAME)
        ));
        assert!(is_vb_cable(&[], Some("VB-Audio Virtual Cable")));
        assert!(!is_vb_cable(&[], Some("VB-Audio Cable A")));
        assert!(!is_vb_cable(&[], None));
    }

    // covers: VC21, VC23
    #[test]
    fn the_button_needs_no_silent_endpoint_no_cable_and_x64() {
        assert_eq!(offer(true, false, false), Offer::Button);
        assert_eq!(offer(false, false, false), Offer::Nothing);
        assert_eq!(offer(true, true, false), Offer::Nothing);
        assert_eq!(offer(true, false, true), Offer::NotOnArm64);
        assert_eq!(offer(true, true, true), Offer::Nothing);

        let mut arm = fake();
        arm.arm64 = true;
        let mut install = SilentInstall::new(Arc::new(arm));
        assert_eq!(install.shown(true), SilentOutput::NotOnArm64);
        assert!(!install.start(true));
        let present = fake();
        present.world().device = true;
        assert_eq!(
            SilentInstall::new(Arc::new(present)).shown(true),
            SilentOutput::None
        );
        assert_eq!(SilentInstall::default().shown(true), SilentOutput::None);
    }

    // covers: VC24, VC35
    #[test]
    fn a_failure_brings_the_button_back_with_its_line_and_a_busy_click_does_nothing() {
        let mut offline = fake();
        offline.pack = Err("offline".to_owned());
        let mut install = SilentInstall::new(Arc::new(offline));
        assert_eq!(install.shown(true), SilentOutput::Offered { failed: None });
        assert!(install.start(true));
        assert_eq!(install.shown(true), SilentOutput::Downloading);
        assert!(!install.start(true), "a second click while busy");
        let polled = install.finish();
        assert!(polled.changed && !polled.added);
        assert!(
            polled.lines.iter().any(|line| line.contains("offline")),
            "{polled:?}"
        );
        assert_eq!(
            install.shown(true),
            SilentOutput::Offered {
                failed: Some(SilentFailure::Download)
            }
        );
        // The real pin: the stand-in is a mismatch.
        let mut install = SilentInstall::new(Arc::new(fake()));
        assert!(install.start(true));
        install.finish();
        assert_eq!(
            install.shown(true),
            SilentOutput::Offered {
                failed: Some(SilentFailure::Mismatch)
            }
        );
        // A notice without the button starts nothing.
        let mut install = SilentInstall::new(Arc::new(fake()));
        assert!(!install.start(false));
    }

    // covers: VC25, VC53
    #[test]
    fn the_installer_adds_vb_cable_for_everyone_on_x64_and_removes_only_its_own() {
        let iss = include_str!("../../installer/audiosync.iss");
        assert!(installer_asks([
            "audiosync.exe".into(),
            INSTALLER_ARG.into()
        ]));
        assert!(!installer_asks(["audiosync.exe".into()]));
        let run = iss
            .lines()
            .find(|line| line.contains(&format!("\"{INSTALLER_ARG}\"")))
            .expect("the installer runs the step");
        assert!(run.contains("Check: AddsVbCable"), "{run}");
        assert!(
            !run.contains("postinstall"),
            "under the installer's elevation: {run}"
        );
        let step = &iss[..iss.find(run).unwrap()];
        assert!(
            step.rfind("#if Arch == \"x64\"") > step.rfind("#endif"),
            "x64 only"
        );
        assert!(iss.contains("Result := IsAdminInstallMode and not HasVbCable;"));
        for line in [
            "It also adds VB-CABLE by VB-Audio, so you hear your PC''s sound once.",
            "The origin of VB-CABLE : www.vb-cable.com.",
            "VB-CABLE is a donationware, all participations are welcome.",
        ] {
            assert!(iss.contains(line), "{line}");
        }
        assert!(iss.contains(
            r#"Filename: "{app}\VB-CABLE\VBCABLE_Setup_x64.exe"; Parameters: "-u -h"; Flags: runhidden waituntilterminated skipifdoesntexist"#
        ));
    }

    // covers: VC32
    #[test]
    fn the_pack_is_extracted_inside_its_folder_and_held() {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(SETUP, options).unwrap();
        zip.write_all(b"setup").unwrap();
        zip.start_file("win10/readme.txt", options).unwrap();
        zip.write_all(b"text").unwrap();
        let pack = zip.finish().unwrap().into_inner();
        let folder = std::env::temp_dir().join(format!("audiosync-vcable-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("left over"), b"old").unwrap();
        let held = unpack_into(&pack, &folder).unwrap();
        assert_eq!(held.len(), 2);
        assert_eq!(std::fs::read(folder.join(SETUP)).unwrap(), b"setup");
        assert!(!folder.join("left over").exists());
        #[cfg(windows)]
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(folder.join(SETUP))
                .is_err(),
            "held without write sharing"
        );
        drop(held);
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
