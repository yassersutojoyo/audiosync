//! Windows' media sessions for the Now Playing card ([`MediaControls`]): read
//! on a thread of its own every [`POLL_INTERVAL`] while the card follows a
//! source, and driven through the session's own Previous, Play or Pause and
//! Next.
//!
//! The session that counts is picked as for receivers ([`pick`]). A button
//! goes to the session that counts when the thread takes it, and the
//! sessions are read again [`AFTER_COMMAND`] later, so the card shows the
//! new state without waiting for the next poll. Artwork is decoded once per
//! track into the card's square tile ([`ARTWORK_SIDE`], [`cover_size`]).
//! Like the receivers' reader, nothing else waits on the thread: the UI
//! thread meets it only under a lock held for a copy.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapBounds, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat,
    BitmapTransform, ColorManagementMode, ExifOrientationMode,
};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
};
use windows::Storage::Streams::IRandomAccessStream;
use windows::core::Interface;

use super::com::Apartment;
use super::media::describe;
use crate::audio::now_playing::{Follow, MediaSession, POLL_INTERVAL, pick};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{
    ARTWORK_SIDE, ArtworkCache, CardArtwork, CardSource, CardTrack, MediaControls, Transport,
    cover_size,
};

/// How soon the sessions are read again after a button: long enough for
/// most apps to have changed their state.
const AFTER_COMMAND: Duration = Duration::from_millis(400);

#[derive(Default)]
struct State {
    source: Option<CardSource>,
    /// Counts source changes, so a reading made for an earlier one is
    /// dropped.
    generation: u64,
    reading: Option<Option<CardTrack>>,
    commands: Vec<Transport>,
    quit: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Reads and drives Windows' media sessions for the card.
pub struct MediaCardReader {
    shared: Arc<Shared>,
}

impl MediaCardReader {
    /// Starts the reader's thread, which reads nothing until the card
    /// follows a source. A thread that can't start is logged, and the card
    /// never shows.
    pub fn start(log: DebugLog) -> MediaCardReader {
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        let thread_log = log.clone();
        let spawned = thread::Builder::new()
            .name("audiosync-now-playing-card".to_owned())
            .spawn(move || run(&thread_shared, &thread_log));
        if let Err(error) = spawned {
            log.log(|| format!("now playing card: the reading thread couldn't start: {error}"));
        }
        MediaCardReader { shared }
    }
}

impl MediaControls for MediaCardReader {
    fn follow(&mut self, source: Option<CardSource>) {
        let mut state = self.shared.lock();
        if state.source != source {
            state.source = source;
            state.generation += 1;
            state.reading = None;
            state.commands.clear();
            self.shared.wake.notify_all();
        }
    }

    fn take_reading(&mut self) -> Option<Option<CardTrack>> {
        self.shared.lock().reading.take()
    }

    fn send(&mut self, command: Transport) {
        self.shared.lock().commands.push(command);
        self.shared.wake.notify_all();
    }
}

impl Drop for MediaCardReader {
    // Not joined: the thread may be waiting on an app's session.
    fn drop(&mut self) {
        self.shared.lock().quit = true;
        self.shared.wake.notify_all();
    }
}

fn run(shared: &Shared, log: &DebugLog) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => {
            log.log(|| format!("now playing card: COM couldn't start on its thread: {error}"));
            return;
        }
    };
    let mut sessions = CardSessions::default();
    loop {
        let (follow, generation, commands) = {
            let mut state = shared
                .wake
                .wait_while(shared.lock(), |state| state.source.is_none() && !state.quit)
                .unwrap_or_else(PoisonError::into_inner);
            match state.source.clone() {
                Some(source) if !state.quit => (
                    follow_for(source),
                    state.generation,
                    std::mem::take(&mut state.commands),
                ),
                _ => return,
            }
        };
        for command in &commands {
            sessions.send(&follow, *command, log);
        }
        let track = sessions.read(&follow, log);
        let mut state = shared.lock();
        if state.generation == generation {
            state.reading = Some(track);
        }
        let wait = if commands.is_empty() {
            POLL_INTERVAL
        } else {
            AFTER_COMMAND
        };
        let _ = shared
            .wake
            .wait_timeout_while(state, wait, |state| {
                state.generation == generation && state.commands.is_empty() && !state.quit
            })
            .unwrap_or_else(PoisonError::into_inner);
    }
}

fn follow_for(source: CardSource) -> Follow {
    match source {
        CardSource::AnyApp => Follow::AnyApp,
        CardSource::App { executable } => Follow::App { executable },
    }
}

/// The reading thread's own state.
#[derive(Default)]
struct CardSessions {
    manager: Option<SessionManager>,
    /// Card artwork by track.
    artwork: ArtworkCache,
    /// The last reading failed, and said so.
    failing: bool,
    /// The last track read, for the log.
    last: Option<MediaSession>,
}

impl CardSessions {
    fn read(&mut self, follow: &Follow, log: &DebugLog) -> Option<CardTrack> {
        let picked = match self.picked(follow) {
            Ok(picked) => {
                if self.failing {
                    self.failing = false;
                    log.log(|| {
                        "now playing card: reading the media sessions works again".to_owned()
                    });
                }
                picked
            }
            Err(error) => {
                if !self.failing {
                    self.failing = true;
                    log.log(|| {
                        format!("now playing card: reading the media sessions failed: {error}")
                    });
                }
                self.manager = None;
                None
            }
        };
        let described = picked.as_ref().map(|(_, session, _)| session);
        if described != self.last.as_ref() {
            self.last = described.cloned();
            log.log(|| match described {
                Some(session) => format!(
                    "now playing card: shows \"{}\" from {} ({})",
                    session.title,
                    session.app_id,
                    if session.playing { "playing" } else { "paused" }
                ),
                None => "now playing card: no media session counts".to_owned(),
            });
        }
        let (_, session, properties) = picked?;
        let artwork = self.artwork(&session, &properties, log);
        Some(CardTrack {
            app_id: session.app_id,
            title: session.title,
            artist: session.artist,
            playing: session.playing,
            artwork,
        })
    }

    /// The session that counts for `follow`, if one does.
    fn picked(
        &mut self,
        follow: &Follow,
    ) -> windows::core::Result<Option<(Session, MediaSession, MediaProperties)>> {
        let manager = match &self.manager {
            Some(manager) => manager.clone(),
            None => {
                let manager = SessionManager::RequestAsync()?.join()?;
                self.manager = Some(manager.clone());
                manager
            }
        };
        let list = manager.GetSessions()?;
        let mut found = Vec::new();
        for index in 0..list.Size()? {
            // One app's session that fails to answer leaves the others.
            if let Ok(session) = list.GetAt(index)
                && let Ok((described, properties)) = describe(&session)
            {
                found.push((session, described, properties));
            }
        }
        let described: Vec<MediaSession> = found.iter().map(|(_, d, _)| d.clone()).collect();
        Ok(pick(&described, follow).map(|index| found.swap_remove(index)))
    }

    /// Sends `command` to the session that counts now.
    fn send(&mut self, follow: &Follow, command: Transport, log: &DebugLog) {
        let sent = self.picked(follow).and_then(|picked| {
            let Some((session, described, _)) = picked else {
                return Ok(None);
            };
            let operation = match command {
                Transport::Previous => session.TrySkipPreviousAsync()?,
                Transport::PlayPause => session.TryTogglePlayPauseAsync()?,
                Transport::Next => session.TrySkipNextAsync()?,
            };
            Ok(Some((described.app_id, operation.join()?)))
        });
        log.log(|| match &sent {
            Ok(Some((app_id, true))) => format!("now playing card: {command:?} sent to {app_id}"),
            Ok(Some((app_id, false))) => {
                format!("now playing card: {app_id} turned down {command:?}")
            }
            Ok(None) => format!("now playing card: {command:?} found no session that counts"),
            Err(error) => format!("now playing card: {command:?} couldn't be sent: {error}"),
        });
    }

    /// The track's artwork for the card, decoded once per track. A missing
    /// thumbnail may come a moment later and a failed read may be passing,
    /// so both are asked again at the next reading ([`ArtworkCache`]).
    fn artwork(
        &mut self,
        session: &MediaSession,
        properties: &MediaProperties,
        log: &DebugLog,
    ) -> Option<CardArtwork> {
        let key = format!("{}|{}|{}", session.app_id, session.title, session.album);
        if let Some(made) = self.artwork.get(&key) {
            return made;
        }
        let Ok(thumbnail) = properties.Thumbnail() else {
            return None;
        };
        let decoded = thumbnail
            .OpenReadAsync()
            .and_then(|operation| operation.join())
            .and_then(|stream| stream.cast::<IRandomAccessStream>())
            .and_then(|stream| card_artwork(&stream));
        let (artwork, error) = self.artwork.keep(&key, decoded);
        if let Some(error) = error {
            log.log(|| {
                format!(
                    "now playing card: the artwork of \"{}\" couldn't be read: {error}",
                    session.title
                )
            });
        }
        artwork
    }
}

/// The image in `stream` scaled to cover the card's tile and cropped to it;
/// `None` for an empty image.
fn card_artwork(stream: &IRandomAccessStream) -> windows::core::Result<Option<CardArtwork>> {
    let decoder = BitmapDecoder::CreateAsync(stream)?.join()?;
    let (width, height) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
    if width == 0 || height == 0 {
        return Ok(None);
    }
    let side = ARTWORK_SIDE;
    let (scaled_width, scaled_height) = cover_size(width, height, side);
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(scaled_width)?;
    transform.SetScaledHeight(scaled_height)?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    transform.SetBounds(BitmapBounds {
        X: (scaled_width - side) / 2,
        Y: (scaled_height - side) / 2,
        Width: side,
        Height: side,
    })?;
    let pixels = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Rgba8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )?
        .join()?
        .DetachPixelData()?;
    if pixels.len() != (side * side * 4) as usize {
        return Ok(None);
    }
    Ok(Some(CardArtwork {
        side,
        rgba: Arc::from(&pixels[..]),
    }))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    /// Reads this PC's media sessions for the card; play something first.
    /// Run with `cargo test -- --ignored --nocapture reads_this_pcs_card`.
    #[test]
    #[ignore = "reads this PC's media sessions"]
    fn reads_this_pcs_card_track() {
        let mut reader = MediaCardReader::start(DebugLog::default());
        reader.follow(Some(CardSource::AnyApp));
        let deadline = Instant::now() + Duration::from_secs(20);
        let reading = loop {
            if let Some(reading) = reader.take_reading() {
                break reading;
            }
            assert!(Instant::now() < deadline, "no reading");
            thread::sleep(Duration::from_millis(50));
        };
        match reading {
            Some(track) => {
                println!(
                    "{} / {} playing {}",
                    track.title,
                    track.subtitle(),
                    track.playing
                );
                if let Some(artwork) = &track.artwork {
                    assert_eq!(
                        artwork.rgba.len(),
                        (artwork.side * artwork.side * 4) as usize
                    );
                }
            }
            None => println!("no media session counts"),
        }
    }
}
