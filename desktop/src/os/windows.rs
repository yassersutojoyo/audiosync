//! Windows 11: the WASAPI audio backend and the media sessions that feed
//! it, the session's single-instance lock, the notification-area icon,
//! `%LOCALAPPDATA%` as the data folder's base, DPAPI for the pairing file,
//! the PC's DNS name and the user's time format.

use std::path::PathBuf;
use std::rc::{Rc, Weak};

pub use super::engine::Media;

use super::Started;
use super::engine;
use crate::audio::now_playing::SharedLabels;
use crate::audio::pairings::{Seal, Sealer};
use crate::debug_log::DebugLog;
use crate::view::App;
use crate::{LOG_FLUSH_AT_EXIT, StartError, audio, launch_at_login, single_instance};

// Declared here, not in main.rs: it is built on the Windows-only tray-icon crate.
#[path = "../tray.rs"]
mod tray;

/// The WASAPI backend, with the media sessions as the playing track and the
/// now-playing card's controls.
pub fn start(log: &DebugLog) -> Result<Started, StartError> {
    let labels = SharedLabels::default();
    let (track_reader, card_reader) = audio::wasapi::read_media_sessions(log.clone());
    let audio = audio::wasapi::open().map_err(|error| StartError::Audio(error.to_string()))?;
    log.log(|| "audio: the WASAPI backend is open".to_owned());
    let backend = engine::backend(audio, log, &labels, Box::new(track_reader));
    Ok(Started {
        backend,
        media: Media::new(labels, Box::new(card_reader)),
    })
}

/// `backend` in place of the WASAPI one, for a preview: the media sessions
/// still feed the now-playing card.
#[cfg(feature = "mock-backend")]
pub fn start_with(log: &DebugLog, backend: Box<dyn crate::ui_model::Backend>) -> Started {
    let (_track_reader, card_reader) = audio::wasapi::read_media_sessions(log.clone());
    Started {
        backend,
        media: Media::new(SharedLabels::default(), Box::new(card_reader)),
    }
}

/// This copy's hold on the session, from [`claim_instance`].
pub struct Instance {
    /// `Err` holds why a later hand-over can't reach this copy (it runs
    /// anyway).
    lock: Result<single_instance::Instance, String>,
    /// How the hand-over to a copy that was quitting went, when this launch
    /// waited for it to go.
    waited_for: Option<single_instance::HandOver>,
}

/// Before the log, which a launch empties, and before any audio, tray or
/// window: `None` when a second launch only handed over to the running copy
/// and exits.
pub fn claim_instance() -> Option<Instance> {
    let launched = std::time::Instant::now();
    let (lock, waited_for) = match single_instance::acquire() {
        single_instance::Acquired::First(instance) => (instance, None),
        single_instance::Acquired::Second
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
        single_instance::Acquired::Second => {
            // A copy that is quitting doesn't answer; once it's gone, this
            // launch starts instead, as on the Mac.
            let (outcome, first) = single_instance::second_launch(
                single_instance::hand_over,
                || match single_instance::acquire() {
                    single_instance::Acquired::First(instance) => Some(instance),
                    single_instance::Acquired::Second => None,
                },
                || launched.elapsed(),
                std::thread::sleep,
            );
            match first {
                Some(instance) => (instance, Some(outcome)),
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

impl Instance {
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
        if let Ok(instance) = &self.lock {
            RUNNING_APP.with(|cell| {
                *cell.borrow_mut() = Some((Rc::downgrade(app), log.clone()));
            });
            instance.listen(show_main_for_second_launch);
        }
    }
}

/// The notification-area icon; it stays until it is dropped.
pub type Tray = Rc<tray::Tray>;

/// Adds the tray icon for `app`; `None` when it couldn't be shown, so
/// closing the main window quits.
pub fn create_tray(app: &Rc<App>, log: &DebugLog) -> Option<Tray> {
    match tray::Tray::create(app) {
        Ok(tray) => {
            log.log(|| "tray icon: shown".to_owned());
            app.set_tray_available(true);
            Some(tray)
        }
        Err(error) => {
            log.log(|| format!("tray icon: not shown ({error}); closing the main window quits"));
            None
        }
    }
}

/// `%LOCALAPPDATA%`, the folder the app's data folder goes in, or `None`
/// when Windows names no such folder.
pub fn data_base() -> Option<PathBuf> {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    local.is_absolute().then_some(local)
}

/// DPAPI, for the current Windows user.
pub fn sealer() -> Sealer {
    Sealer::new(&Dpapi)
}

struct Dpapi;

impl Seal for Dpapi {
    fn seal(&self, plain: &[u8]) -> std::io::Result<Vec<u8>> {
        dpapi::protect(plain)
    }

    fn open(&self, sealed: &[u8]) -> std::io::Result<zeroize::Zeroizing<Vec<u8>>> {
        dpapi::unprotect(sealed)
    }
}

mod dpapi {
    //! `CryptProtectData` and `CryptUnprotectData` for the current user,
    //! never with a prompt.

    use std::io;

    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::w;
    use zeroize::{Zeroize, Zeroizing};

    /// Extra entropy, so another app's DPAPI call for the same user can't
    /// simply decrypt the file.
    const ENTROPY: &[u8] = b"AudioSync AirPlay pairings";

    fn blob(bytes: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
        let size = u32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too much data"))?;
        Ok(CRYPT_INTEGER_BLOB {
            cbData: size,
            pbData: bytes.as_ptr().cast_mut(),
        })
    }

    /// Copies what DPAPI returned, wipes it when `wipe`, and frees it.
    fn take(out: CRYPT_INTEGER_BLOB, wipe: bool) -> Vec<u8> {
        if out.pbData.is_null() {
            return Vec::new();
        }
        // SAFETY: DPAPI returned `cbData` bytes at `pbData`, allocated with
        // LocalAlloc, which nothing else holds; they are copied, then freed
        // once.
        unsafe {
            let bytes = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
            let copy = bytes.to_vec();
            if wipe {
                bytes.zeroize();
            }
            LocalFree(Some(HLOCAL(out.pbData.cast())));
            copy
        }
    }

    pub fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
        let input = blob(plain)?;
        let entropy = blob(ENTROPY)?;
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` and `entropy` point at live slices for the call;
        // DPAPI fills `out`, which `take` frees.
        unsafe {
            CryptProtectData(
                &input,
                w!("AudioSync AirPlay pairings"),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        }
        Ok(take(out, false))
    }

    pub fn unprotect(sealed: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
        let input = blob(sealed)?;
        let entropy = blob(ENTROPY)?;
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `protect`; the decrypted bytes are wiped before
        // they are freed.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        }
        Ok(Zeroizing::new(take(out, true)))
    }
}

/// Windows' DNS host name for this PC.
pub fn computer_name() -> Option<String> {
    use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
    use windows::core::PWSTR;

    let mut size = 0u32;
    // The first call fails and reports the size the name needs.
    // SAFETY: no buffer is passed; `size` is a valid out parameter.
    let _ = unsafe { GetComputerNameExW(ComputerNameDnsHostname, None, &mut size) };
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: `buffer` holds `size` UTF-16 units, as `size` says.
    unsafe {
        GetComputerNameExW(
            ComputerNameDnsHostname,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut size,
        )
    }
    .ok()?;
    buffer.truncate(size as usize);
    String::from_utf16(&buffer).ok()
}

/// The local time now, in the user's short time format ("09.40", "9:40 AM"):
/// when a notice was posted. Hours and minutes when the format can't be read.
pub fn notice_time() -> String {
    use windows::Win32::Globalization::{GetTimeFormatEx, TIME_NOSECONDS};
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::core::PCWSTR;

    let mut text = [0u16; 64];
    // SAFETY: formats the current local time (no time given) in the user's
    // locale and format (no locale name, no format) into `text`, whose length
    // the call is given.
    let written = unsafe {
        GetTimeFormatEx(
            PCWSTR::null(),
            TIME_NOSECONDS,
            None,
            PCWSTR::null(),
            Some(&mut text),
        )
    };
    // The count includes the terminating NUL; 0 means it failed.
    match usize::try_from(written) {
        Ok(length) if length > 1 => String::from_utf16_lossy(&text[..length - 1]),
        _ => {
            // SAFETY: only reads the clock.
            let now = unsafe { GetLocalTime() };
            format!("{:02}.{:02}", now.wHour, now.wMinute)
        }
    }
}
