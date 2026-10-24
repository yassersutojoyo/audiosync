//! Linux: the PipeWire audio backend. The rest is still the UI-only build's
//! (`other`): no single-instance lock, tray icon, pairing sealer or data
//! folder, so settings and pairings last until AudioSync quits.

use std::path::PathBuf;
use std::sync::Arc;

pub use super::engine::Media;
pub use super::other::{Instance, claim_instance, computer_name, create_tray, notice_time, sealer};

use super::Started;
use super::engine;
use crate::StartError;
use crate::audio;
use crate::audio::now_playing::{Follow, Reading, SharedLabels, TrackReader};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{CardSource, CardTrack, MediaControls, Transport};

/// The PipeWire backend. No playing track is read yet.
pub fn start(log: &DebugLog) -> Result<Started, StartError> {
    let labels = SharedLabels::default();
    let audio = audio::pipewire::open().map_err(|error| StartError::Audio(error.to_string()))?;
    log.log(|| "audio: the PipeWire backend is open".to_owned());
    let backend = engine::backend(audio, log, &labels, Box::new(NoTrack));
    Ok(Started {
        backend,
        media: Media::new(labels, Box::new(NoTrack)),
    })
}

/// `backend` in place of the PipeWire one, for a preview.
#[cfg(feature = "mock-backend")]
pub fn start_with(_log: &DebugLog, backend: Box<dyn crate::ui_model::Backend>) -> Started {
    Started {
        backend,
        media: Media::new(SharedLabels::default(), Box::new(NoTrack)),
    }
}

/// No data folder yet.
pub fn data_base() -> Option<PathBuf> {
    None
}

/// Neither the receivers nor the now-playing card get a track: nothing
/// reads the desktop's media players yet.
struct NoTrack;

impl TrackReader for NoTrack {
    fn follow(&mut self, _follow: Option<Follow>) {}

    fn take_reading(&mut self) -> Option<Reading> {
        None
    }

    fn icon(&mut self) -> Option<Arc<[u8]>> {
        None
    }
}

impl MediaControls for NoTrack {
    fn follow(&mut self, _source: Option<CardSource>) {}

    fn take_reading(&mut self) -> Option<Option<CardTrack>> {
        None
    }

    fn send(&mut self, _command: Transport) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_reads_a_track() {
        assert_eq!(TrackReader::take_reading(&mut NoTrack), None);
        assert_eq!(MediaControls::take_reading(&mut NoTrack), None);
        assert_eq!(TrackReader::icon(&mut NoTrack), None);
    }
}
