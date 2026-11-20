//! Adding a silent output on a PC that has none
//! (docs/WINDOWS_SILENT_OUTPUT_SPEC.md): the double-audio notice offers VB-CABLE
//! by VB-Audio, which AudioSync downloads from the vendor when the user asks,
//! checks against a pinned SHA-256 and opens in the vendor's own setup window.
//! Once its endpoints are active, the default switch (S469) and the app move
//! (S471) take the source there as they take any other virtual output.
//!
//! [`run_pinned`] is the whole install over the [`Platform`] seam, which tests drive
//! with fakes; nothing in a test downloads, extracts to a real folder or
//! launches a setup.

// Only the Windows app installs it; Linux compiles the logic for its tests.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::ui_model::{SilentFailure, SilentOutput};

/// VB-CABLE's pack 46 on the vendor's server (VC1).
pub const PACK_URL: &str = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip";
/// Its SHA-256 on 2026-11-20 (VC2).
pub const PACK_SHA256: &str = "bc44e8974191a5a2427d6c979e2dab08dbb09925f3f435dfa2f0ba3e45b6eb90";
/// The 64-bit setup in the pack (VC3), run with no switches (VC33).
pub const SETUP: &str = "VBCABLE_Setup_x64.exe";
/// The vendor's page, which the notice links to (VC22).
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
/// The folder in the app's data folder the pack is extracted to (VC37).
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
    /// The pack from [`PACK_URL`], within the limits of VC31.
    fn download(&self) -> Result<Vec<u8>, String>;
    /// The folder the pack is extracted to: emptied, then held while the
    /// setup runs; the handles it returns keep each file from changing.
    fn unpack(&self, pack: &[u8]) -> Result<(PathBuf, Vec<std::fs::File>), String>;
    fn launch(&self, setup: &Path) -> Result<Box<dyn Setup>, Launch>;
    fn now(&self) -> Instant;
    fn pause(&self, duration: Duration);
}

/// Where an install is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    /// The setup was launched: the UAC prompt, then the vendor's window.
    Waiting,
}

/// Sets back each default the install moved to a cable endpoint (VC40).
fn put_back(platform: &dyn Platform, before: &[Option<String>]) -> Vec<String> {
    let cable = platform.cable_endpoints();
    let now = platform.defaults();
    let mut lines = Vec::new();
    for ((slot, was), is) in SLOTS.iter().zip(before).zip(&now) {
        let (Some(was), Some(is)) = (was, is) else {
            continue;
        };
        if !cable.contains(is) || cable.contains(was) || !platform.endpoint_active(was) {
            continue;
        }
        let result = platform.set_default(*slot, was);
        lines.push(format!(
            "silent output: {slot:?} default moved to {is} by the install, set back to {was}: \
             {result:?}"
        ));
    }
    lines
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
    let mut setup = match platform.launch(&folder.join(SETUP)) {
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
        put_back(platform, &before).into_iter().for_each(&mut *log);
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
        put_back(platform, &before).into_iter().for_each(&mut *log);
    }
    log(format!(
        "silent output: defaults after the install {:?}",
        platform.defaults()
    ));
    Ok(())
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
        launched: Vec<PathBuf>,
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
        fn download(&self) -> Result<Vec<u8>, String> {
            self.pack.clone()
        }
        fn unpack(&self, _pack: &[u8]) -> Result<(PathBuf, Vec<std::fs::File>), String> {
            self.world().unpacked = true;
            Ok((PathBuf::from("data").join(FOLDER_NAME), Vec::new()))
        }
        fn launch(&self, setup: &Path) -> Result<Box<dyn Setup>, Launch> {
            (self.launch)()?;
            let mut world = self.world();
            world.launched.push(setup.to_path_buf());
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
    fn the_setup_runs_from_the_extracted_folder_with_no_switches() {
        let fake = fake();
        assert_eq!(fake.install(), Ok(()));
        assert_eq!(
            fake.world().launched,
            vec![PathBuf::from("data").join(FOLDER_NAME).join(SETUP)]
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
