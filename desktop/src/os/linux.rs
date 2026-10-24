//! Linux: the PipeWire audio backend, with the MPRIS players as the playing
//! track and the now-playing card's controls. The instance lock, the pairing
//! sealer and the data folder's base come from `xdg`, the tray icon from
//! `status_notifier`.

pub use super::engine::Media;
pub use super::other::{computer_name, notice_time};
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
