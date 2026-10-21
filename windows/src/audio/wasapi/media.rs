//! The media sessions Windows lists (the system media transport controls,
//! `Windows.Media.Control`), read for track info ([`TrackReader`]) on a
//! thread of its own.
//!
//! The thread sleeps until track info follows a source, then reads every
//! [`POLL_INTERVAL`]: the sessions, the one that counts ([`pick`]) and its
//! thumbnail, fitted as a JPEG ([`fit_jpeg`]) through the WinRT imaging
//! codecs and kept per track, so a track's artwork is made once. Its calls
//! may wait on the apps behind the sessions; nothing else waits on the
//! thread, which only meets the UI thread under a lock held for a copy.
//! Errors are logged once until reading works again, and count as no track.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use windows::Foundation::PropertyType;
use windows::Foundation::PropertyValue;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapEncoder, BitmapInterpolationMode, BitmapPixelFormat,
    BitmapPropertySet, BitmapTransform, BitmapTypedValue, ColorManagementMode, ExifOrientationMode,
};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::{
    Buffer, DataReader, DataWriter, IRandomAccessStream, InMemoryRandomAccessStream,
    InputStreamOptions,
};
use windows::core::{HSTRING, Interface};

use super::com::Apartment;
use crate::audio::now_playing::{
    Follow, MediaSession, POLL_INTERVAL, Reading, Track, TrackReader, fit_jpeg, pick,
};
use crate::debug_log::DebugLog;

/// AudioSync's icon, which receivers show while no track's artwork does.
const ICON_PNG: &[u8] = include_bytes!("../../../res/audiosync-256.png");

/// Artwork kept for at most this many tracks.
const ARTWORK_CACHE: usize = 32;

/// The JPEG encoder's quality option.
const IMAGE_QUALITY: &str = "ImageQuality";

#[derive(Default)]
struct State {
    follow: Option<Follow>,
    /// Counts follow changes, so a reading made for an earlier one is
    /// dropped.
    generation: u64,
    reading: Option<Reading>,
    icon: Option<Arc<[u8]>>,
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

/// Reads Windows' media sessions on a thread of its own.
pub struct MediaSessionReader {
    shared: Arc<Shared>,
}

impl MediaSessionReader {
    /// Starts the reader's thread, which reads nothing until told to
    /// follow a source. A thread that can't start is logged, and the
    /// reader never reads.
    pub fn start(log: DebugLog) -> MediaSessionReader {
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        let thread_log = log.clone();
        let spawned = thread::Builder::new()
            .name("audiosync-now-playing".to_owned())
            .spawn(move || run(&thread_shared, &thread_log));
        if let Err(error) = spawned {
            log.log(|| format!("now playing: the reading thread couldn't start: {error}"));
        }
        MediaSessionReader { shared }
    }
}

impl TrackReader for MediaSessionReader {
    fn follow(&mut self, follow: Option<Follow>) {
        let mut state = self.shared.lock();
        if state.follow != follow {
            state.follow = follow;
            state.generation += 1;
            state.reading = None;
            self.shared.wake.notify_all();
        }
    }

    fn take_reading(&mut self) -> Option<Reading> {
        self.shared.lock().reading.take()
    }

    fn icon(&mut self) -> Option<Arc<[u8]>> {
        self.shared.lock().icon.clone()
    }
}

impl Drop for MediaSessionReader {
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
            log.log(|| format!("now playing: COM couldn't start on the reading thread: {error}"));
            return;
        }
    };
    let icon = match fit_png(ICON_PNG) {
        Ok(Some(jpeg)) => Some(Arc::from(jpeg)),
        Ok(None) => {
            log.log(|| "now playing: AudioSync's icon doesn't fit as artwork".to_owned());
            None
        }
        Err(error) => {
            log.log(|| format!("now playing: AudioSync's icon couldn't be encoded: {error}"));
            None
        }
    };
    shared.lock().icon = icon;
    let mut sessions = Sessions::default();
    loop {
        let (follow, generation) = {
            let state = shared
                .wake
                .wait_while(shared.lock(), |state| state.follow.is_none() && !state.quit)
                .unwrap_or_else(PoisonError::into_inner);
            match &state.follow {
                Some(follow) if !state.quit => (follow.clone(), state.generation),
                _ => return,
            }
        };
        let track = sessions.read(&follow, log);
        let mut state = shared.lock();
        if state.generation == generation {
            state.reading = Some(Reading { track });
        }
        let _ = shared
            .wake
            .wait_timeout_while(state, POLL_INTERVAL, |state| {
                state.generation == generation && !state.quit
            })
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// The reading thread's own state.
#[derive(Default)]
struct Sessions {
    manager: Option<SessionManager>,
    /// Fitted artwork by track; `None` for a thumbnail that couldn't be.
    artwork: HashMap<String, Option<Arc<[u8]>>>,
    /// The last reading failed, and said so.
    failing: bool,
    /// The last track read, for the log.
    last: Option<MediaSession>,
}

impl Sessions {
    fn read(&mut self, follow: &Follow, log: &DebugLog) -> Option<Track> {
        match self.try_read(follow, log) {
            Ok(track) => {
                if self.failing {
                    self.failing = false;
                    log.log(|| "now playing: reading the media sessions works again".to_owned());
                }
                let session = track.as_ref().map(|track| &track.session);
                if session != self.last.as_ref() {
                    self.last = session.cloned();
                    log.log(|| match session {
                        Some(session) => format!(
                            "now playing: read \"{}\" by \"{}\" from {} ({})",
                            session.title,
                            session.artist,
                            session.app_id,
                            if session.playing { "playing" } else { "paused" }
                        ),
                        None => "now playing: no media session counts".to_owned(),
                    });
                }
                track
            }
            Err(error) => {
                if !self.failing {
                    self.failing = true;
                    log.log(|| format!("now playing: reading the media sessions failed: {error}"));
                }
                self.manager = None;
                None
            }
        }
    }

    fn try_read(
        &mut self,
        follow: &Follow,
        log: &DebugLog,
    ) -> windows::core::Result<Option<Track>> {
        let manager = match &self.manager {
            Some(manager) => manager.clone(),
            None => {
                let manager = SessionManager::RequestAsync()?.join()?;
                self.manager = Some(manager.clone());
                manager
            }
        };
        let list = manager.GetSessions()?;
        let mut found: Vec<(MediaSession, MediaProperties)> = Vec::new();
        for index in 0..list.Size()? {
            // One app's session that fails to answer leaves the others.
            if let Ok(entry) = list.GetAt(index).and_then(|session| describe(&session)) {
                found.push(entry);
            }
        }
        let sessions: Vec<MediaSession> =
            found.iter().map(|(session, _)| session.clone()).collect();
        let Some(index) = pick(&sessions, follow) else {
            return Ok(None);
        };
        let (session, properties) = found.swap_remove(index);
        let artwork = self.artwork(&session, &properties, log);
        Ok(Some(Track { session, artwork }))
    }

    /// The track's artwork, fitted once per track. A session without a
    /// thumbnail may get one a moment later, so that is asked again.
    fn artwork(
        &mut self,
        session: &MediaSession,
        properties: &MediaProperties,
        log: &DebugLog,
    ) -> Option<Arc<[u8]>> {
        let key = format!("{}|{}|{}", session.app_id, session.title, session.album);
        if let Some(cached) = self.artwork.get(&key) {
            return cached.clone();
        }
        let Ok(thumbnail) = properties.Thumbnail() else {
            return None;
        };
        let fitted = thumbnail
            .OpenReadAsync()
            .and_then(|operation| operation.join())
            .and_then(|stream| stream.cast::<IRandomAccessStream>())
            .and_then(|stream| fit_stream(&stream));
        let artwork = match fitted {
            Ok(Some(jpeg)) => Some(Arc::from(jpeg)),
            Ok(None) => {
                log.log(|| {
                    format!(
                        "now playing: the artwork of \"{}\" doesn't fit",
                        session.title
                    )
                });
                None
            }
            Err(error) => {
                log.log(|| {
                    format!(
                        "now playing: the artwork of \"{}\" couldn't be read: {error}",
                        session.title
                    )
                });
                None
            }
        };
        if self.artwork.len() >= ARTWORK_CACHE {
            self.artwork.clear();
        }
        self.artwork.insert(key, artwork.clone());
        artwork
    }
}

/// A session as track info sees it, and its media properties.
pub(super) fn describe(
    session: &Session,
) -> windows::core::Result<(MediaSession, MediaProperties)> {
    let properties = session.TryGetMediaPropertiesAsync()?.join()?;
    let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == PlaybackStatus::Playing;
    let described = MediaSession {
        app_id: session.SourceAppUserModelId()?.to_string(),
        title: properties.Title()?.to_string(),
        artist: properties.Artist()?.to_string(),
        album: properties.AlbumTitle()?.to_string(),
        playing,
    };
    Ok((described, properties))
}

/// A PNG as fitted artwork.
fn fit_png(png: &[u8]) -> windows::core::Result<Option<Vec<u8>>> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::new()?;
    writer.WriteBytes(png)?;
    stream.WriteAsync(&writer.DetachBuffer()?)?.join()?;
    stream.Seek(0)?;
    fit_stream(&stream.cast()?)
}

/// The image in `stream` as fitted artwork.
fn fit_stream(stream: &IRandomAccessStream) -> windows::core::Result<Option<Vec<u8>>> {
    let decoder = BitmapDecoder::CreateAsync(stream)?.join()?;
    let (width, height) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
    let mut failure = None;
    let fitted = fit_jpeg(width, height, |width, height, quality| match encode_scaled(
        &decoder, width, height, quality,
    ) {
        Ok(jpeg) => Some(jpeg),
        Err(error) => {
            failure = Some(error);
            None
        }
    });
    match failure {
        Some(error) if fitted.is_none() => Err(error),
        _ => Ok(fitted),
    }
}

/// The decoded image scaled to `width` × `height`, drawn on black (JPEG has
/// no transparency) and encoded as a JPEG at `quality`.
fn encode_scaled(
    decoder: &BitmapDecoder,
    width: u32,
    height: u32,
    quality: f32,
) -> windows::core::Result<Vec<u8>> {
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(width)?;
    transform.SetScaledHeight(height)?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    // Premultiplied pixels are the image over black once alpha is dropped.
    let pixels = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )?
        .join()?
        .DetachPixelData()?;
    let output = InMemoryRandomAccessStream::new()?;
    let options = BitmapPropertySet::new()?;
    options.Insert(
        &HSTRING::from(IMAGE_QUALITY),
        &BitmapTypedValue::Create(&PropertyValue::CreateSingle(quality)?, PropertyType::Single)?,
    )?;
    let encoder = BitmapEncoder::CreateWithEncodingOptionsAsync(
        BitmapEncoder::JpegEncoderId()?,
        &output,
        &options,
    )?
    .join()?;
    encoder.SetPixelData(
        BitmapPixelFormat::Bgra8,
        BitmapAlphaMode::Ignore,
        width,
        height,
        96.0,
        96.0,
        &pixels,
    )?;
    encoder.FlushAsync()?.join()?;
    read_all(&output)
}

fn read_all(stream: &InMemoryRandomAccessStream) -> windows::core::Result<Vec<u8>> {
    let size = u32::try_from(stream.Size()?).unwrap_or(u32::MAX);
    stream.Seek(0)?;
    let buffer = Buffer::Create(size)?;
    let filled = stream
        .ReadAsync(&buffer, size, InputStreamOptions::None)?
        .join()?;
    let mut data = vec![0; filled.Length()? as usize];
    DataReader::FromBuffer(&filled)?.ReadBytes(&mut data)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::audio::now_playing::ARTWORK_LIMIT;

    /// Reads this PC's media sessions; play something first. Run with
    /// `cargo test -- --ignored --nocapture reads_this_pcs`.
    #[test]
    #[ignore = "reads this PC's media sessions"]
    fn reads_this_pcs_media_sessions() {
        let mut reader = MediaSessionReader::start(DebugLog::default());
        reader.follow(Some(Follow::AnyApp));
        let deadline = Instant::now() + Duration::from_secs(20);
        let reading = loop {
            if let Some(reading) = reader.take_reading() {
                break reading;
            }
            assert!(Instant::now() < deadline, "no reading");
            thread::sleep(Duration::from_millis(50));
        };
        let icon = reader
            .icon()
            .expect("the icon is made before the first reading");
        assert!(icon.len() <= ARTWORK_LIMIT && icon.starts_with(&[0xFF, 0xD8]));
        println!("icon: {} bytes", icon.len());
        match reading.track {
            Some(track) => {
                println!("{:?}", track.session);
                if let Some(jpeg) = &track.artwork {
                    assert!(jpeg.len() <= ARTWORK_LIMIT && jpeg.starts_with(&[0xFF, 0xD8]));
                }
                let artwork = track.artwork.map(|jpeg| jpeg.len());
                println!("artwork: {artwork:?} bytes");
            }
            None => println!("no media session counts"),
        }
    }
}
