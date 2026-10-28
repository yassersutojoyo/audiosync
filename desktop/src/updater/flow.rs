//! One check (spec S295, S296), one install after the click (S298 to
//! S302), the leftovers at launch (S304) and when the launch check is due
//! (S305). Everything here runs on the updater's worker thread.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;

use super::appcast::{self, Arch, Update};
use super::archive;
use super::fetch::{self, Fetch, FetchError, Kind};
use super::files::{Files, Pause, Retrying, is_blocked, is_disk_full, is_locked};
use super::relaunch::Launch;
use super::signature;
use super::swap::{self, Rename, StepLog, SwapError, Swapped};
use super::version::Version;
use crate::debug_log::DebugLog;

pub const TEMP_PREFIX: &str = "AudioSync-update-";
pub const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// What a check or an install works with; tests swap the fetch, the files,
/// the folders and the key.
pub struct Env {
    pub feed: String,
    pub fetch: Arc<dyn Fetch>,
    pub files: Arc<dyn Files>,
    pub temp_root: PathBuf,
    pub install_dir: PathBuf,
    pub key: VerifyingKey,
    pub arch: Arch,
    pub running: Version,
    pub system: Version,
    pub max_download: u64,
    pub log: DebugLog,
    pub launch: Arc<dyn Launch>,
    /// How long Restart Now waits for the new version's start signal.
    pub start_wait: Duration,
    /// The waits before lock retries (spec S315), which a quit cuts short.
    pub pause: Arc<Pause>,
}

#[derive(Debug, PartialEq)]
pub enum Checked {
    Update(Update),
    UpToDate,
    Failed,
}

/// Fetches and reads the appcast and picks the update in it.
pub fn check(env: &Env) -> Checked {
    let body = match fetch::appcast(env.fetch.as_ref(), &env.feed) {
        Ok(body) => body,
        Err(error) => {
            env.log
                .log(|| format!("updater: the appcast fetch failed ({error:?})"));
            return Checked::Failed;
        }
    };
    let text = String::from_utf8_lossy(&body);
    let read = match appcast::parse(&text) {
        Ok(read) => read,
        Err(error) => {
            env.log
                .log(|| format!("updater: the appcast can't be read ({error:?})"));
            return Checked::Failed;
        }
    };
    env.log.log(|| {
        format!(
            "updater: appcast fetched, {} bytes, {} items read, {} skipped{}",
            body.len(),
            read.items.len(),
            read.skipped.len(),
            if read.skipped.is_empty() {
                String::new()
            } else {
                format!(" ({})", read.skipped.join("; "))
            }
        )
    });
    match appcast::pick(&read, env.arch, &env.running, &env.system) {
        Some(update) => {
            env.log.log(|| {
                format!(
                    "updater: update {} picked for {} (running {})",
                    update.version_text,
                    env.arch.os_value(),
                    env.running
                )
            });
            Checked::Update(update)
        }
        None => {
            env.log.log(|| {
                format!(
                    "updater: no update for {} above {}",
                    env.arch.os_value(),
                    env.running
                )
            });
            Checked::UpToDate
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    Done(Swapped),
    Cancelled,
    DownloadFailed,
    BadSignature,
    BrokenDownload,
    DiskFull,
    ReadOnly,
    InUse,
    Failed,
    RestoreFailed {
        renames: Vec<Rename>,
    },
    /// Windows security removed or blocked an update file (spec S315).
    Blocked,
}

fn host(url: &str) -> String {
    fetch::https_target(url).map_or_else(|| "?".to_owned(), |target| target.host)
}

/// A new, empty `AudioSync-update-<random>` folder in `root`.
fn new_temp_folder(root: &Path) -> io::Result<PathBuf> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    for _ in 0..8 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        let dir = root.join(format!("{TEMP_PREFIX}{:016x}", hasher.finish()));
        match fs::create_dir(&dir) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            other => return other.map(|()| dir),
        }
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

/// Downloads, verifies, unpacks and swaps `update`; the temp folder goes
/// whichever way it ends. `progress` gets the percentage when the size is
/// known.
pub fn install(
    env: &Env,
    update: &Update,
    progress: &dyn Fn(Option<u8>),
    cancel: &AtomicBool,
) -> Installed {
    let dir = match new_temp_folder(&env.temp_root) {
        Ok(dir) => dir,
        Err(error) => {
            env.log
                .log(|| format!("updater: the temp folder can't be made ({error})"));
            return if is_disk_full(&error) {
                Installed::DiskFull
            } else {
                Installed::Failed
            };
        }
    };
    let held = hold(&dir);
    if let Err(error) = &held {
        env.log.log(|| {
            format!("updater: the temp folder can't be held, another copy may delete it ({error})")
        });
    }
    let outcome = install_in(env, update, &dir, progress, cancel);
    drop(held);
    let removed = fs::remove_dir_all(&dir);
    env.log.log(|| {
        let said = match &outcome {
            // The swap's own steps are logged already.
            Installed::Done(_) => "Done".to_owned(),
            other => format!("{other:?}"),
        };
        format!(
            "updater: {said}; temp folder {}",
            if removed.is_ok() {
                "removed"
            } else {
                "not removed"
            }
        )
    });
    outcome
}

/// A handle on `dir` that shares reading and writing but not deleting, so
/// another copy's `remove_leftovers` (the same user in another session)
/// sees the folder in use and leaves it.
fn hold(dir: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    fs::OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(dir)
}

/// Whether another copy holds `dir` through `hold`.
fn held_elsewhere(dir: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_BACKUP_SEMANTICS};
    fs::OpenOptions::new()
        .access_mode(DELETE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(dir)
        .is_err_and(|error| is_locked(&error))
}

fn install_in(
    env: &Env,
    update: &Update,
    dir: &Path,
    progress: &dyn Fn(Option<u8>),
    cancel: &AtomicBool,
) -> Installed {
    let files = Retrying {
        inner: env.files.as_ref(),
        log: &env.log,
        pause: &env.pause,
    };
    let enclosure = &update.enclosure;
    let zip = dir.join("update.zip");
    let limit = enclosure
        .length
        .unwrap_or(env.max_download)
        .min(env.max_download);
    let started = Instant::now();
    let mut received: u64 = 0;
    let mut write_error: Option<io::Error> = None;
    let mut out = match files.create(&zip, true) {
        Ok(out) => out,
        Err(error) => return file_failure(env, "create the download", &error),
    };
    let mut last_percent = None;
    let fetched = env.fetch.get(&enclosure.url, Kind::Download, &mut |bytes| {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        received += bytes.len() as u64;
        if received > limit {
            return false;
        }
        if let Err(error) = out.write_all(bytes) {
            write_error = Some(error);
            return false;
        }
        let percent = enclosure
            .length
            .filter(|length| *length > 0)
            .map(|length| u8::try_from(received * 100 / length).unwrap_or(100));
        if percent != last_percent {
            last_percent = percent;
            progress(percent);
        }
        true
    });
    let flushed = out.flush().and_then(|()| out.sync());
    drop(out);
    env.log.log(|| {
        format!(
            "updater: download from {}: {received} bytes in {} ms ({fetched:?})",
            host(&enclosure.url),
            started.elapsed().as_millis()
        )
    });
    if cancel.load(Ordering::Relaxed) {
        return Installed::Cancelled;
    }
    if let Some(error) = write_error.or(flushed.err()) {
        return file_failure(env, "write the download", &error);
    }
    match fetched {
        Ok(()) => {}
        Err(FetchError::Stopped) if received > limit => {
            env.log
                .log(|| format!("updater: the download passed its {limit} bytes"));
            return Installed::DownloadFailed;
        }
        Err(_) => return Installed::DownloadFailed,
    }
    if enclosure.length.is_some_and(|length| received < length) {
        env.log
            .log(|| "updater: the download ended short of its length".to_owned());
        return Installed::DownloadFailed;
    }

    let mut bytes = Vec::new();
    if let Err(error) = files
        .open(&zip)
        .and_then(|mut file| file.read_to_end(&mut bytes))
    {
        // The download gone before its check was removed by Windows security.
        if error.kind() == io::ErrorKind::NotFound {
            return blocked(env, "read the download", &error);
        }
        return file_failure(env, "read the download", &error);
    }
    let good = signature::verifies(&bytes, enclosure.signature.as_deref(), &env.key);
    env.log.log(|| {
        format!(
            "updater: signature of {} from {} {}",
            update.version_text,
            enclosure.url,
            if good { "verified" } else { "is bad" }
        )
    });
    if !good {
        return Installed::BadSignature;
    }
    if cancel.load(Ordering::Relaxed) {
        return Installed::Cancelled;
    }

    let unpacked = archive::unpack(&bytes, env.arch);
    drop(bytes);
    env.log.log(|| match &unpacked {
        Ok(unpacked) => format!(
            "updater: unpacked {}{}",
            archive::EXE_NAME,
            if unpacked.notices.is_some() {
                " and the notices"
            } else {
                ""
            }
        ),
        Err(error) => format!("updater: the zip is refused ({error})"),
    });
    let unpacked = match unpacked {
        Ok(unpacked) => unpacked,
        Err(_) => return Installed::BrokenDownload,
    };
    // The signature covers the bytes only, not the version the appcast names.
    match archive::product_version(&unpacked.exe) {
        Some(reported) if reported == update.version && reported > env.running => {}
        reported => {
            env.log.log(|| {
                format!(
                    "updater: the zip is refused (its exe reports version {}, not {})",
                    reported.map_or_else(|| "none".to_owned(), |v| v.to_string()),
                    update.version_text
                )
            });
            return Installed::BrokenDownload;
        }
    }
    // The last point where a Cancel still leaves the install folder untouched.
    if cancel.load(Ordering::Relaxed) {
        return Installed::Cancelled;
    }

    let mut steps = StepLog::new();
    let swapped = swap::swap(
        &env.install_dir,
        &unpacked.exe,
        unpacked.notices.as_deref(),
        &files,
        &mut steps,
    );
    for (step, code) in &steps {
        env.log
            .log(|| format!("updater: swap {step} (error {code})"));
    }
    match swapped {
        Ok(swapped) => Installed::Done(swapped),
        Err(SwapError::ReadOnly) => Installed::ReadOnly,
        Err(SwapError::InUse) => Installed::InUse,
        Err(SwapError::DiskFull) => Installed::DiskFull,
        Err(SwapError::Failed { .. }) => Installed::Failed,
        Err(SwapError::RestoreFailed { renames }) => Installed::RestoreFailed { renames },
        Err(SwapError::Blocked) => Installed::Blocked,
    }
}

/// What a failed file step means: a Windows security block, a full disk,
/// or another failure.
fn file_failure(env: &Env, step: &str, error: &io::Error) -> Installed {
    if is_blocked(error) {
        blocked(env, step, error)
    } else if is_disk_full(error) {
        Installed::DiskFull
    } else {
        env.log
            .log(|| format!("updater: {step} failed (error {})", code(error)));
        Installed::Failed
    }
}

fn blocked(env: &Env, step: &str, error: &io::Error) -> Installed {
    env.log.log(|| {
        format!(
            "updater: {step}: blocked by Windows security (error {})",
            code(error)
        )
    });
    Installed::Blocked
}

fn code(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(-1)
}

/// Deletes the `.new` files in the install folder and every temp folder an
/// earlier update left; the `.bak` stays.
pub fn remove_leftovers(install_dir: &Path, temp_root: &Path, log: &DebugLog) {
    for name in [
        format!("{}.new", archive::EXE_NAME),
        format!("{}.new", archive::NOTICES_NAME),
    ] {
        let path = install_dir.join(&name);
        if path.exists() {
            let removed = fs::remove_file(&path);
            log.log(|| format!("updater: leftover {name} {}", removed_text(&removed)));
        }
    }
    let Ok(entries) = fs::read_dir(temp_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(TEMP_PREFIX) && entry.path().is_dir() {
            if held_elsewhere(&entry.path()) {
                log.log(|| format!("updater: leftover {name} kept (in use by another copy)"));
                continue;
            }
            let removed = fs::remove_dir_all(entry.path());
            log.log(|| format!("updater: leftover {name} {}", removed_text(&removed)));
        }
    }
}

fn removed_text(result: &io::Result<()>) -> String {
    match result {
        Ok(()) => "removed".to_owned(),
        Err(error) => format!("not removed ({error})"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LaunchCheck {
    Off,
    Due(&'static str),
    NotDue,
}

/// Whether the launch check runs, from the switch and the last successful
/// check's time, both in seconds since the Unix epoch.
pub fn launch_check(automatic: bool, last: Option<u64>, now: u64) -> LaunchCheck {
    if !automatic {
        return LaunchCheck::Off;
    }
    match last {
        None => LaunchCheck::Due("no check stored"),
        Some(last) if last > now => LaunchCheck::Due("the stored check lies in the future"),
        Some(last) if now - last >= CHECK_INTERVAL_SECS => LaunchCheck::Due("24 hours passed"),
        Some(_) => LaunchCheck::NotDue,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::updater::archive::tests::{fake_exe, fake_exe_reporting, zip_of};
    use crate::updater::fetch::tests::MemoryFetch;
    use crate::updater::files::{LOCK_RETRIES, RealFiles, Sink};
    use crate::updater::signature::test_key;
    use crate::updater::tests::test_dir;
    use std::collections::HashMap;

    pub const FEED: &str = "https://feed.invalid/appcast.xml";
    pub const ZIP_URL: &str = "https://feed.invalid/AudioSync-0.9.0-windows-x64.zip";

    pub fn appcast_with(enclosure_os: &str, length: usize, signature: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel>
<item><sparkle:version>9</sparkle:version><sparkle:shortVersionString>0.9.0</sparkle:shortVersionString>
<enclosure url="{ZIP_URL}" length="{length}" sparkle:os="{enclosure_os}" sparkle:edSignature="{signature}"/></item>
</channel></rss>"#
        )
    }

    pub struct Fixture {
        pub env: Env,
        pub fetch: Arc<MemoryFetch>,
    }

    /// An install folder with an old exe, a temp root and a feed offering
    /// 0.9.0 for x64, signed with a throwaway key unless `bad_signature`.
    pub fn fixture(
        name: &str,
        bad_signature: bool,
        files: Arc<dyn Files>,
        log: DebugLog,
    ) -> Fixture {
        let root = test_dir(name);
        let install_dir = root.join("install");
        let temp_root = root.join("temp");
        fs::create_dir_all(&install_dir).unwrap();
        fs::create_dir_all(&temp_root).unwrap();
        fs::write(install_dir.join(archive::EXE_NAME), b"old exe").unwrap();
        let zip = zip_of(&[
            (
                archive::EXE_NAME,
                &fake_exe_reporting([0, 9, 0, 0], b"new exe"),
            ),
            (archive::NOTICES_NAME, b"new notices"),
        ]);
        let key = test_key::new();
        let signed = test_key::sign(&key, if bad_signature { b"other" } else { &zip });
        let mut fetch = MemoryFetch::default();
        fetch.files.insert(
            FEED.to_owned(),
            appcast_with("windows-x64", zip.len(), &signed).into_bytes(),
        );
        fetch.files.insert(ZIP_URL.to_owned(), zip);
        let fetch = Arc::new(fetch);
        Fixture {
            env: Env {
                feed: FEED.to_owned(),
                fetch: fetch.clone(),
                files,
                temp_root,
                install_dir,
                key: key.verifying_key(),
                arch: Arch::X64,
                running: Version::parse("0.8.0").unwrap(),
                system: Version::parse("10.0.26100").unwrap(),
                max_download: MAX_DOWNLOAD,
                log,
                launch: Arc::new(NoLaunch),
                start_wait: Duration::from_millis(300),
                pause: Arc::new(Pause::real()),
            },
            fetch,
        }
    }

    /// A launch that never starts anything; a test that restarts sets its own.
    pub struct NoLaunch;

    impl Launch for NoLaunch {
        fn start(&self, _: &Path, _: Duration) -> crate::updater::relaunch::Start {
            crate::updater::relaunch::Start::Failed(io::Error::other("no launch in this test"))
        }
    }

    pub fn update_of(env: &Env) -> Update {
        match check(env) {
            Checked::Update(update) => update,
            other => panic!("an update, not {other:?}"),
        }
    }

    fn temp_folders(env: &Env) -> Vec<String> {
        fs::read_dir(&env.temp_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn run(env: &Env, update: &Update) -> Installed {
        install(env, update, &|_| {}, &AtomicBool::new(false))
    }

    #[test]
    fn the_fake_appcast_end_to_end_through_a_memory_fetch() {
        let f = fixture(
            "end-to-end",
            false,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        let update = update_of(&f.env);
        assert_eq!(update.version_text, "0.9.0");
        let percents = std::sync::Mutex::new(Vec::new());
        let done = install(
            &f.env,
            &update,
            &|p| percents.lock().unwrap().push(p),
            &AtomicBool::new(false),
        );
        assert!(matches!(done, Installed::Done(_)), "{done:?}");
        let exe = fs::read(f.env.install_dir.join(archive::EXE_NAME)).unwrap();
        assert!(exe.ends_with(b"new exe"));
        assert_eq!(
            fs::read(f.env.install_dir.join("audiosync.exe.bak")).unwrap(),
            b"old exe"
        );
        assert_eq!(percents.lock().unwrap().last(), Some(&Some(100)));
        assert!(temp_folders(&f.env).is_empty());

        // The other arch only, or the same version: nothing is offered.
        let mut arm = fixture(
            "end-to-end-arm",
            false,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        arm.env.arch = Arch::Arm64;
        assert_eq!(check(&arm.env), Checked::UpToDate);
        let mut same = fixture(
            "end-to-end-same",
            false,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        same.env.running = Version::parse("0.9").unwrap();
        assert_eq!(check(&same.env), Checked::UpToDate);
    }

    /// Serves `files` by path over plain HTTP on 127.0.0.1, and answers
    /// `/redirect/<path>` with a 302 to `/<path>`.
    fn serve(files: HashMap<String, Vec<u8>>) -> u16 {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let path = request.split(' ').nth(1).unwrap_or("/").to_owned();
                let head = |status: &str, extra: &str, length: usize| {
                    format!(
                        "HTTP/1.1 {status}\r\n{extra}Content-Length: {length}\r\nConnection: close\r\n\r\n"
                    )
                };
                let reply = if let Some(rest) = path.strip_prefix("/redirect") {
                    head("302 Found", &format!("Location: {rest}\r\n"), 0).into_bytes()
                } else if let Some(body) = files.get(&path) {
                    let mut reply = head("200 OK", "", body.len()).into_bytes();
                    reply.extend_from_slice(body);
                    reply
                } else {
                    head("404 Not Found", "", 0).into_bytes()
                };
                let _ = stream.write_all(&reply);
            }
        });
        port
    }

    // covers: S307 (what WinHTTP downloads and the swap installs carries no mark; whether
    // SmartScreen prompts at the relaunch is checked by hand)
    #[test]
    fn the_fake_appcast_on_loopback() {
        if std::env::var_os("CI").is_none() {
            eprintln!("skipped: serves on 127.0.0.1, which runs in CI only");
            return;
        }
        let zip = zip_of(&[
            (
                archive::EXE_NAME,
                &fake_exe_reporting([0, 9, 0, 0], b"served exe"),
            ),
            (archive::NOTICES_NAME, b"served notices"),
        ]);
        let key = test_key::new();
        let good = test_key::sign(&key, &zip);
        let mut files = HashMap::new();
        files.insert("/AudioSync.zip".to_owned(), zip.clone());
        let port = serve(files.clone());
        let zip_url = format!("http://127.0.0.1:{port}/redirect/AudioSync.zip");
        let appcast = |os: &str, signature: &str| {
            appcast_with(os, zip.len(), signature).replace(ZIP_URL, &zip_url)
        };
        files.insert(
            "/appcast.xml".to_owned(),
            appcast("windows-x64", &good).into_bytes(),
        );
        files.insert(
            "/bad.xml".to_owned(),
            appcast("windows-x64", &test_key::sign(&key, b"x")).into_bytes(),
        );
        files.insert(
            "/arm.xml".to_owned(),
            appcast("windows-arm64", &good).into_bytes(),
        );
        let port = serve(files);
        let feed = |name: &str| format!("http://127.0.0.1:{port}/{name}");
        let winhttp = Arc::new(fetch::WinHttp {
            user_agent: fetch::user_agent("0.8.0", "x64"),
            plain_loopback: true,
        });
        let setup = |name: &str, feed_name: &str| {
            let mut f = fixture(name, false, Arc::new(RealFiles), DebugLog::default());
            f.env.fetch = winhttp.clone();
            f.env.key = key.verifying_key();
            f.env.feed = feed(feed_name);
            f
        };

        let happy = setup("loopback", "appcast.xml");
        let update = update_of(&happy.env);
        assert!(matches!(run(&happy.env, &update), Installed::Done(_)));
        let exe = fs::read(happy.env.install_dir.join(archive::EXE_NAME)).unwrap();
        assert!(exe.ends_with(b"served exe"));
        assert!(!marked_from_the_web(
            &happy.env.install_dir.join(archive::EXE_NAME)
        ));
        assert_eq!(
            fs::read(happy.env.install_dir.join("audiosync.exe.bak")).unwrap(),
            b"old exe"
        );
        assert!(temp_folders(&happy.env).is_empty());

        let bad = setup("loopback-bad", "bad.xml");
        let update = update_of(&bad.env);
        assert_eq!(run(&bad.env, &update), Installed::BadSignature);
        assert_eq!(
            fs::read(bad.env.install_dir.join(archive::EXE_NAME)).unwrap(),
            b"old exe"
        );
        assert!(temp_folders(&bad.env).is_empty());

        assert_eq!(
            check(&setup("loopback-arm", "arm.xml").env),
            Checked::UpToDate
        );
        let mut same = setup("loopback-same", "appcast.xml");
        same.env.running = Version::parse("0.9.0").unwrap();
        assert_eq!(check(&same.env), Checked::UpToDate);
        assert_eq!(
            check(&setup("loopback-404", "missing.xml").env),
            Checked::Failed
        );

        let plain = fetch::WinHttp {
            user_agent: fetch::user_agent("0.8.0", "x64"),
            plain_loopback: false,
        };
        assert!(
            fetch::appcast(&plain, &feed("appcast.xml")).is_err(),
            "plain HTTP is refused"
        );
    }

    // covers: S299
    #[test]
    fn a_bad_signature_deletes_the_download_and_changes_nothing() {
        let f = fixture(
            "bad-signature",
            true,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        let update = update_of(&f.env);
        assert_eq!(run(&f.env, &update), Installed::BadSignature);
        assert_eq!(
            fs::read(f.env.install_dir.join(archive::EXE_NAME)).unwrap(),
            b"old exe"
        );
        assert!(!f.env.install_dir.join("audiosync.exe.bak").exists());
        assert!(temp_folders(&f.env).is_empty());
    }

    /// Real files, except that every open in the temp folder after the first
    /// finds the file replaced by an unsigned one, as another process of the
    /// same user could replace it.
    struct Planter {
        opened: std::sync::Mutex<u32>,
    }

    impl Files for Planter {
        fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>> {
            RealFiles.create(path, exclusive)
        }
        fn remove(&self, path: &Path) -> io::Result<()> {
            RealFiles.remove(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            RealFiles.rename(from, to)
        }
        fn open(&self, path: &Path) -> io::Result<fs::File> {
            let in_temp = path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|dir| dir.to_string_lossy().starts_with(TEMP_PREFIX));
            let mut opened = self.opened.lock().unwrap();
            if in_temp && *opened > 0 {
                let planted = fake_exe(Arch::X64.pe_machine(), b"planted exe");
                if path.file_name().is_some_and(|name| name == "update.zip") {
                    fs::write(path, zip_of(&[(archive::EXE_NAME, &planted)]))?;
                } else {
                    fs::write(path, planted)?;
                }
            }
            *opened += u32::from(in_temp);
            fs::File::open(path)
        }
    }

    // covers: S299, S300
    #[test]
    fn only_the_verified_bytes_are_unpacked_and_installed() {
        let f = fixture(
            "planted",
            false,
            Arc::new(Planter {
                opened: std::sync::Mutex::new(0),
            }),
            DebugLog::default(),
        );
        let update = update_of(&f.env);
        assert!(matches!(run(&f.env, &update), Installed::Done(_)));
        assert!(
            fs::read(f.env.install_dir.join(archive::EXE_NAME))
                .unwrap()
                .ends_with(b"new exe")
        );
    }

    /// Offers `exe` as 0.9.0 in a zip signed with a new throwaway key.
    fn offer(env: &mut Env, exe: &[u8]) {
        let zip = zip_of(&[(archive::EXE_NAME, exe)]);
        let key = test_key::new();
        let signed = test_key::sign(&key, &zip);
        let mut fetch = MemoryFetch::default();
        fetch.files.insert(
            FEED.to_owned(),
            appcast_with("windows-x64", zip.len(), &signed).into_bytes(),
        );
        fetch.files.insert(ZIP_URL.to_owned(), zip);
        env.fetch = Arc::new(fetch);
        env.key = key.verifying_key();
    }

    // covers: S300
    #[test]
    fn an_exe_reporting_another_version_than_offered_is_refused() {
        for (case, exe, running) in [
            ("older", fake_exe_reporting([0, 7, 0, 0], b"0.7.0"), "0.8.0"),
            (
                "none",
                fake_exe(Arch::X64.pe_machine(), b"no version"),
                "0.8.0",
            ),
            (
                "not newer",
                fake_exe_reporting([0, 9, 0, 0], b"0.9.0"),
                "0.9.0",
            ),
        ] {
            let mut f = fixture(
                &format!("version-{case}"),
                false,
                Arc::new(RealFiles),
                DebugLog::default(),
            );
            offer(&mut f.env, &exe);
            let update = update_of(&f.env);
            f.env.running = Version::parse(running).unwrap();
            assert_eq!(run(&f.env, &update), Installed::BrokenDownload, "{case}");
            assert_eq!(
                fs::read(f.env.install_dir.join(archive::EXE_NAME)).unwrap(),
                b"old exe",
                "{case}"
            );
            assert!(temp_folders(&f.env).is_empty(), "{case}");
        }

        let mut f = fixture(
            "version-offered",
            false,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        offer(&mut f.env, &fake_exe_reporting([0, 9, 0, 0], b"0.9.0"));
        let update = update_of(&f.env);
        assert!(matches!(run(&f.env, &update), Installed::Done(_)));
    }

    // covers: S297, S306
    #[test]
    fn nothing_downloads_before_the_click() {
        let f = fixture(
            "no-download",
            false,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        let _ = update_of(&f.env);
        assert_eq!(*f.fetch.asked.lock().unwrap(), [FEED]);
        assert!(temp_folders(&f.env).is_empty());
    }

    // covers: S298
    #[test]
    fn a_download_over_its_length_or_the_cap_stops_and_is_deleted() {
        let f = fixture(
            "over-length",
            false,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        let mut update = update_of(&f.env);
        let real = update.enclosure.length.unwrap();
        update.enclosure.length = Some(real - 1);
        assert_eq!(run(&f.env, &update), Installed::DownloadFailed);
        update.enclosure.length = Some(real + 1);
        assert_eq!(
            run(&f.env, &update),
            Installed::DownloadFailed,
            "short of its length"
        );
        update.enclosure.length = None;
        let mut capped = fixture("over-cap", false, Arc::new(RealFiles), DebugLog::default());
        capped.env.max_download = real - 1;
        assert_eq!(run(&capped.env, &update), Installed::DownloadFailed);
        assert!(temp_folders(&f.env).is_empty());
        assert!(temp_folders(&capped.env).is_empty());
        assert_eq!(
            fs::read(f.env.install_dir.join(archive::EXE_NAME)).unwrap(),
            b"old exe"
        );
    }

    // covers: S302
    #[test]
    fn a_disk_full_write_deletes_every_partial_file() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(112))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl Sink for Full {
            fn sync(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        /// Real files whose writes fail once `full` names them.
        struct FullAt(&'static str);
        impl Files for FullAt {
            fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>> {
                let file = RealFiles.create(path, exclusive)?;
                if path.file_name().is_some_and(|name| name == self.0) {
                    Ok(Box::new(Full))
                } else {
                    Ok(file)
                }
            }
            fn remove(&self, path: &Path) -> io::Result<()> {
                RealFiles.remove(path)
            }
            fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
                RealFiles.rename(from, to)
            }
        }
        for (at, case) in [("update.zip", "download"), ("audiosync.exe.new", "copy")] {
            let f = fixture(
                &format!("disk-full-{case}"),
                false,
                Arc::new(FullAt(at)),
                DebugLog::default(),
            );
            let update = update_of(&f.env);
            assert_eq!(run(&f.env, &update), Installed::DiskFull, "{case}");
            assert!(temp_folders(&f.env).is_empty(), "{case}");
            let mut left: Vec<_> = fs::read_dir(&f.env.install_dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            left.sort();
            assert_eq!(left, [archive::EXE_NAME], "{case}");
        }
    }

    // covers: S304
    #[test]
    fn leftover_new_files_and_temp_folders_go_at_launch_and_the_bak_stays() {
        let f = fixture("leftovers", false, Arc::new(RealFiles), DebugLog::default());
        let install = &f.env.install_dir;
        fs::write(install.join("audiosync.exe.new"), b"x").unwrap();
        fs::write(install.join("THIRD-PARTY-NOTICES.txt.new"), b"x").unwrap();
        fs::write(install.join("audiosync.exe.bak"), b"previous").unwrap();
        fs::create_dir_all(f.env.temp_root.join("AudioSync-update-abc").join("inner")).unwrap();
        fs::create_dir_all(f.env.temp_root.join("Other-folder")).unwrap();
        remove_leftovers(install, &f.env.temp_root, &DebugLog::default());
        assert!(!install.join("audiosync.exe.new").exists());
        assert!(!install.join("THIRD-PARTY-NOTICES.txt.new").exists());
        assert!(install.join("audiosync.exe.bak").exists());
        assert_eq!(temp_folders(&f.env), ["Other-folder"]);
    }

    /// A fetch whose download starts only after another copy of the app,
    /// the same user in another session, ran its launch cleanup.
    struct SweptMidDownload {
        inner: Arc<MemoryFetch>,
        install_dir: PathBuf,
        temp_root: PathBuf,
    }

    impl Fetch for SweptMidDownload {
        fn get(&self, url: &str, kind: Kind, sink: &mut fetch::Sink<'_>) -> Result<(), FetchError> {
            if kind == Kind::Download {
                remove_leftovers(&self.install_dir, &self.temp_root, &DebugLog::default());
            }
            self.inner.get(url, kind, sink)
        }
    }

    // covers: S304
    #[test]
    fn another_sessions_cleanup_leaves_a_running_download_alone() {
        let mut f = fixture(
            "other-session",
            false,
            Arc::new(RealFiles),
            DebugLog::default(),
        );
        f.env.fetch = Arc::new(SweptMidDownload {
            inner: f.fetch.clone(),
            install_dir: f.env.install_dir.clone(),
            temp_root: f.env.temp_root.clone(),
        });
        let update = update_of(&f.env);
        let done = run(&f.env, &update);
        assert!(matches!(done, Installed::Done(_)), "{done:?}");
        assert!(temp_folders(&f.env).is_empty());
    }

    // covers: S305
    #[test]
    fn the_launch_check_runs_once_a_day_and_a_failed_check_stores_no_time() {
        let now = 1_800_000_000;
        assert_eq!(
            launch_check(true, None, now),
            LaunchCheck::Due("no check stored")
        );
        assert_eq!(
            launch_check(true, Some(now - 23 * 3600), now),
            LaunchCheck::NotDue
        );
        assert!(matches!(
            launch_check(true, Some(now - 24 * 3600), now),
            LaunchCheck::Due(_)
        ));
        assert!(matches!(
            launch_check(true, Some(now + 60), now),
            LaunchCheck::Due(_)
        ));
        let mut offline = fixture("offline", false, Arc::new(RealFiles), DebugLog::default());
        offline.env.feed = "https://unreachable.invalid/appcast.xml".to_owned();
        assert_eq!(check(&offline.env), Checked::Failed);
    }

    // covers: S309
    #[test]
    fn the_updater_writes_its_debug_log_lines() {
        let dir = crate::debug_log::test_dir("updater-log");
        let path = dir.join("log.txt");
        let log = DebugLog::new(Some(path.clone()));
        let f = fixture("log", false, Arc::new(RealFiles), log.clone());
        fs::write(f.env.install_dir.join("audiosync.exe.new"), b"x").unwrap();
        remove_leftovers(&f.env.install_dir, &f.env.temp_root, &log);
        let update = update_of(&f.env);
        assert!(matches!(run(&f.env, &update), Installed::Done(_)));
        assert!(log.flush(crate::debug_log::TEST_FLUSH));
        let messages = crate::debug_log::read_messages(&path);
        crate::debug_log::assert_in_order(
            &messages,
            &[
                "updater: leftover audiosync.exe.new removed",
                "1 items read, 0 skipped",
                "updater: update 0.9.0 picked",
                "updater: download from feed.invalid: ",
                "updater: signature of 0.9.0 from https://feed.invalid/AudioSync-0.9.0-windows-x64.zip verified",
                "updater: unpacked audiosync.exe and the notices",
                "updater: swap 1: create audiosync.exe.new (error 0)",
                "updater: swap 3: rename audiosync.exe to audiosync.exe.bak (error 0)",
                "updater: swap 4: rename audiosync.exe.new to audiosync.exe (error 0)",
                "updater: Done; temp folder removed",
            ],
        );
        let profile = std::env::var("USERPROFILE").unwrap_or_default();
        assert!(
            profile.is_empty() || messages.iter().all(|line| !line.contains(&profile)),
            "no line holds a profile path"
        );
    }

    /// Real files that fail one operation with `code`: `op` on the file
    /// named `to` (a rename by its source and target), counting the tries.
    struct Infected {
        op: &'static str,
        from: &'static str,
        to: &'static str,
        code: i32,
        tries: std::sync::Mutex<u32>,
    }

    impl Infected {
        fn hit(&self, op: &str, from: Option<&Path>, to: &Path) -> Option<io::Error> {
            let named = |path: &Path, name: &str| path.file_name().is_some_and(|n| n == name);
            let at = op == self.op
                && named(to, self.to)
                && from.is_none_or(|from| named(from, self.from));
            if !at {
                return None;
            }
            *self.tries.lock().unwrap() += 1;
            Some(io::Error::from_raw_os_error(self.code))
        }
    }

    impl Files for Infected {
        fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>> {
            match self.hit("create", None, path) {
                Some(error) => Err(error),
                None => RealFiles.create(path, exclusive),
            }
        }
        fn remove(&self, path: &Path) -> io::Result<()> {
            RealFiles.remove(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            match self.hit("rename", Some(from), to) {
                Some(error) => Err(error),
                None => RealFiles.rename(from, to),
            }
        }
    }

    // covers: S302, S315, S316
    #[test]
    fn an_infected_error_at_any_swap_step_undoes_the_swap_and_offers_the_release_page() {
        for code in [225, 226] {
            for (case, op, from, to) in [
                ("copy", "create", "", "audiosync.exe.new"),
                ("step3", "rename", "audiosync.exe", "audiosync.exe.bak"),
                ("step4", "rename", "audiosync.exe.new", "audiosync.exe"),
            ] {
                let files = Arc::new(Infected {
                    op,
                    from,
                    to,
                    code,
                    tries: std::sync::Mutex::new(0),
                });
                let f = fixture(
                    &format!("infected-{case}-{code}"),
                    false,
                    files.clone(),
                    DebugLog::default(),
                );
                let update = update_of(&f.env);
                assert_eq!(run(&f.env, &update), Installed::Blocked, "{case} {code}");
                assert_eq!(*files.tries.lock().unwrap(), 1, "{case} {code}: no retry");
                assert!(temp_folders(&f.env).is_empty(), "{case} {code}");
                let mut left: Vec<_> = fs::read_dir(&f.env.install_dir)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                left.sort();
                assert_eq!(left, [archive::EXE_NAME], "{case} {code}");
                assert_eq!(
                    fs::read(f.env.install_dir.join(archive::EXE_NAME)).unwrap(),
                    b"old exe",
                    "{case} {code}"
                );
            }
        }
    }

    /// Real files, except that the read of the download after it is written
    /// finds it held without sharing by another handle until `release` is sent.
    struct Held {
        release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        held: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl Files for Held {
        fn create(&self, path: &Path, exclusive: bool) -> io::Result<Box<dyn Sink>> {
            RealFiles.create(path, exclusive)
        }
        fn remove(&self, path: &Path) -> io::Result<()> {
            RealFiles.remove(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            RealFiles.rename(from, to)
        }
        fn open(&self, path: &Path) -> io::Result<fs::File> {
            if path.file_name().is_some_and(|name| name == "update.zip")
                && let Some(release) = self.release.lock().unwrap().take()
            {
                use std::os::windows::fs::OpenOptionsExt;
                let holder = fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(path)
                    .expect("the test holds the download");
                *self.held.lock().unwrap() = Some(std::thread::spawn(move || {
                    let _ = release.recv();
                    drop(holder);
                }));
            }
            fs::File::open(path)
        }
    }

    // covers: S315
    #[test]
    fn a_sharing_violation_is_retried_five_times_then_fails() {
        // The waits are counted, not slept, and the exe is let go once they
        // add up to `hold`: a loaded machine stretches neither part.
        let held = |name: &str, hold: Duration| {
            let (release, receiver) = std::sync::mpsc::channel();
            let files = Arc::new(Held {
                release: std::sync::Mutex::new(Some(receiver)),
                held: std::sync::Mutex::new(None),
            });
            let mut f = fixture(name, false, files.clone(), DebugLog::default());
            let waits = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (holder, counted) = (files.clone(), waits.clone());
            let on_time = std::sync::Mutex::new(Some(release.clone()));
            f.env.pause = Arc::new(Pause::counted(move |wait| {
                let mut waits = counted.lock().unwrap();
                waits.push(wait);
                if waits.iter().sum::<Duration>() >= hold
                    && let Some(release) = on_time.lock().unwrap().take()
                {
                    let _ = release.send(());
                    let thread = holder.held.lock().unwrap().take();
                    thread.expect("the exe was held").join().unwrap();
                }
            }));
            (f, files, release, waits)
        };

        let (f, _, _, waits) = held("held-1s", Duration::from_secs(1));
        let update = update_of(&f.env);
        assert!(matches!(run(&f.env, &update), Installed::Done(_)));
        assert_eq!(*waits.lock().unwrap(), LOCK_RETRIES[..3]);
        assert!(
            fs::read(f.env.install_dir.join(archive::EXE_NAME))
                .unwrap()
                .ends_with(b"new exe")
        );

        let (f, files, release, waits) = held("held-throughout", Duration::MAX);
        let update = update_of(&f.env);
        let outcome = run(&f.env, &update);
        let _ = release.send(());
        if let Some(holder) = files.held.lock().unwrap().take() {
            holder.join().unwrap();
        }
        assert_eq!(outcome, Installed::Failed);
        assert_eq!(*waits.lock().unwrap(), LOCK_RETRIES, "five retries");
        assert_eq!(
            fs::read(f.env.install_dir.join(archive::EXE_NAME)).unwrap(),
            b"old exe"
        );
        assert!(!f.env.install_dir.join("audiosync.exe.new").exists());
    }

    /// Whether `path` carries a Mark of the Web, a `Zone.Identifier` stream.
    fn marked_from_the_web(path: &Path) -> bool {
        assert!(path.exists(), "{}", path.display());
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        match fs::File::open(PathBuf::from(stream)) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => panic!("{}: {error}", path.display()),
        }
    }

    // covers: S315
    #[test]
    fn the_updater_writes_no_zone_identifier_stream() {
        let f = fixture("zone", false, Arc::new(RealFiles), DebugLog::default());
        let update = update_of(&f.env);
        assert!(matches!(run(&f.env, &update), Installed::Done(_)));
        assert!(!marked_from_the_web(
            &f.env.install_dir.join(archive::EXE_NAME)
        ));
    }

    #[cfg(windows)]
    fn mark_from_the_web(path: &Path) {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        fs::write(PathBuf::from(stream), "[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();
        assert!(marked_from_the_web(path));
    }

    // A 0.8.x install unzipped by Explorer from a browser download, an
    // earlier `.bak` included, carries the mark the swap has to rename past.
    // covers: S301, S304, S307, S315
    #[cfg(windows)]
    #[test]
    fn an_old_exe_marked_from_the_web_is_swapped_and_the_new_one_is_unmarked() {
        let f = fixture("zone-old", false, Arc::new(RealFiles), DebugLog::default());
        let install = f.env.install_dir.clone();
        let old = install.join(archive::EXE_NAME);
        let earlier = install.join("audiosync.exe.bak");
        fs::write(&earlier, b"older exe").unwrap();
        mark_from_the_web(&old);
        mark_from_the_web(&earlier);
        let update = update_of(&f.env);
        let outcome = run(&f.env, &update);
        assert!(matches!(outcome, Installed::Done(_)), "{outcome:?}");
        assert!(fs::read(&old).unwrap().ends_with(b"new exe"));
        assert!(!marked_from_the_web(&old));
        assert_eq!(fs::read(&earlier).unwrap(), b"old exe");
        // A rename keeps the streams; the updater leaves the old mark alone.
        assert!(marked_from_the_web(&earlier));
        assert!(!install.join("audiosync.exe.new").exists());
        assert!(temp_folders(&f.env).is_empty());
        drop(f);
        let _ = fs::remove_dir_all(install.parent().unwrap());
    }
}
