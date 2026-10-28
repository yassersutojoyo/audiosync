//! Linux: the PipeWire audio backend, with the MPRIS players as the playing
//! track and the now-playing card's controls. The instance lock, the pairing
//! sealer and the data folder's base come from `xdg`, the tray icon from
//! `status_notifier`.

pub use super::engine::Media;
pub use super::other::computer_name;
pub use super::status_notifier::create_tray;
pub use super::xdg::{Instance, claim_instance, data_base, sealer};

use super::Started;
use super::engine;
use crate::StartError;
use crate::audio;
use crate::audio::now_playing::SharedLabels;
use crate::debug_log::DebugLog;

/// The PipeWire backend, with the MPRIS players as the playing track.
pub fn start(log: &DebugLog) -> Result<Started, StartError> {
    let labels = SharedLabels::default();
    let (track_reader, card_reader) = audio::mpris::read_media_sessions(log.clone());
    let audio = audio::pipewire::open().map_err(|error| StartError::Audio(error.to_string()))?;
    log.log(|| "audio: the PipeWire backend is open".to_owned());
    let backend = engine::backend(audio, log, &labels, Box::new(track_reader));
    Ok(Started {
        backend,
        media: Media::new(labels, Box::new(card_reader)),
    })
}

/// `backend` in place of the PipeWire one, for a preview: the MPRIS players
/// still feed the now-playing card.
#[cfg(feature = "mock-backend")]
pub fn start_with(log: &DebugLog, backend: Box<dyn crate::ui_model::Backend>) -> Started {
    let (_track_reader, card_reader) = audio::mpris::read_media_sessions(log.clone());
    Started {
        backend,
        media: Media::new(SharedLabels::default(), Box::new(card_reader)),
    }
}

/// The local time now, hours and minutes, as Windows gives its notices.
#[allow(
    clippy::useless_conversion,
    reason = "time_t and c_long are 32-bit on some targets"
)]
pub fn notice_time() -> String {
    // SAFETY: time with a null pointer only returns the time.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: tm is plain data, which localtime_r fills.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals of the right types.
    let offset = if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        0
    } else {
        i64::from(tm.tm_gmtoff)
    };
    crate::platform::clock_at(i64::from(now), offset)
}
