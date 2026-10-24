//! Hosts without an audio backend yet, where the UI is worked on. With the
//! `mock-backend` feature the windows show example speakers and nothing
//! plays; without it the app says it can't start. There is no
//! single-instance lock, tray icon or pairing sealer here, and no data
//! folder: settings and pairings last until AudioSync quits.

use std::rc::Rc;

use super::Started;
use crate::StartError;
use crate::audio::pairings::{Seal, Sealer};
use crate::debug_log::DebugLog;
use crate::view::App;

#[cfg(feature = "mock-backend")]
pub fn start(log: &DebugLog) -> Result<Started, StartError> {
    log.log(|| "audio: example speakers from the mock backend; nothing plays".to_owned());
    Ok(Started {
        backend: Box::new(crate::ui_model::MockBackend::new()),
        media: Media,
    })
}

#[cfg(not(feature = "mock-backend"))]
pub fn start(_log: &DebugLog) -> Result<Started, StartError> {
    Err(StartError::Audio(
        "this system has no audio backend yet (the mock-backend feature shows example speakers)"
            .to_owned(),
    ))
}

/// `backend` as it is: there is nothing else to open.
#[cfg(feature = "mock-backend")]
pub fn start_with(_log: &DebugLog, backend: Box<dyn crate::ui_model::Backend>) -> Started {
    Started {
        backend,
        media: Media,
    }
}

/// No playing track and no now-playing card.
#[cfg_attr(not(feature = "mock-backend"), allow(dead_code))]
pub struct Media;

impl Media {
    pub fn attach(self, _app: &Rc<App>) {}
}

/// Every launch runs: there is no lock to hand over through.
pub struct Instance;

pub fn claim_instance() -> Option<Instance> {
    Some(Instance)
}

impl Instance {
    pub fn log_state(&self, _log: &DebugLog) {}

    pub fn listen(&self, _app: &Rc<App>, _log: &DebugLog) {}
}

/// No tray icon: closing the main window quits.
pub enum Tray {}

pub fn create_tray(_app: &Rc<App>, _log: &DebugLog) -> Option<Tray> {
    None
}

/// Nothing can be sealed, so nothing is stored.
pub fn sealer() -> Sealer {
    Sealer::new(&Unsupported)
}

struct Unsupported;

fn unsupported() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, "no sealer on this system")
}

impl Seal for Unsupported {
    fn seal(&self, _plain: &[u8]) -> std::io::Result<Vec<u8>> {
        Err(unsupported())
    }

    fn open(&self, _sealed: &[u8]) -> std::io::Result<zeroize::Zeroizing<Vec<u8>>> {
        Err(unsupported())
    }
}

pub fn computer_name() -> Option<String> {
    std::env::var("HOSTNAME").ok()
}

/// The time now in UTC, hours and minutes: std reads no time zone.
pub fn notice_time() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    format!("{:02}.{:02}", seconds / 3600 % 24, seconds / 60 % 60)
}
