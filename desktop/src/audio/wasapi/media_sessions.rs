//! The media sessions Windows lists (the system media transport controls,
//! `Windows.Media.Control`) as a [`SessionList`] for the shared reading
//! thread ([`crate::audio::media_sessions`]). Every wait on an app is a WinRT
//! operation, waited for at most until its deadline and then cancelled.

use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::Instant;

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
};
use windows::Storage::Streams::{
    Buffer, DataReader, IRandomAccessStreamWithContentType, InputStreamOptions,
};
use windows::Win32::Foundation::{E_FAIL, ERROR_TIMEOUT};
use windows::core::{Interface, RuntimeType};
use windows_future::{IAsyncInfo, IAsyncOperation};

use super::com::Apartment;
use super::media;
use crate::audio::media_sessions::{
    APP_WAIT, MediaCardReader, MediaSessionReader, SessionList, THUMBNAIL_LIMIT, spawn,
};
use crate::audio::now_playing::{MediaSession, POLL_INTERVAL, RawTimeline};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{CardArtwork, Transport};

/// Starts the thread that reads Windows' sessions for both readers.
pub fn read_media_sessions(log: DebugLog) -> (MediaSessionReader, MediaCardReader) {
    spawn(WindowsSessions::open, POLL_INTERVAL, log)
}

struct WindowsSessions {
    manager: Option<SessionManager>,
    /// Declared last, so the session objects are released before COM.
    _apartment: Apartment,
}

impl WindowsSessions {
    fn open() -> Result<WindowsSessions, String> {
        let apartment = Apartment::enter()
            .map_err(|error| format!("COM couldn't start on the reading thread: {error}"))?;
        Ok(WindowsSessions {
            manager: None,
            _apartment: apartment,
        })
    }

    fn listed(&mut self) -> windows::core::Result<Vec<(MediaSession, (Session, MediaProperties))>> {
        let manager = match &self.manager {
            Some(manager) => manager.clone(),
            None => {
                let manager = SessionManager::RequestAsync()?.join()?;
                self.manager = Some(manager.clone());
                manager
            }
        };
        let list = manager.GetSessions()?;
        let sessions: Vec<Session> = (0..list.Size()?)
            .filter_map(|index| list.GetAt(index).ok())
            .collect();
        Ok(describe_each(
            sessions,
            |session| session.TryGetMediaPropertiesAsync(),
            |session, properties| {
                let described = media::describe(&session, &properties)?;
                Ok((described, (session, properties)))
            },
        ))
    }
}

impl SessionList for WindowsSessions {
    type Session = (Session, MediaProperties);

    fn list(&mut self) -> Result<Vec<(MediaSession, Self::Session)>, String> {
        self.listed().map_err(|error| error.to_string())
    }

    fn thumbnail(
        &self,
        (_, properties): &Self::Session,
        deadline: Instant,
    ) -> Result<Option<Vec<u8>>, String> {
        let Ok(thumbnail) = properties.Thumbnail() else {
            return Ok(None);
        };
        thumbnail
            .OpenReadAsync()
            .and_then(|open| read_thumbnail(open, deadline))
            .map_err(|error| error.to_string())
    }

    fn timeline(&self, (session, _): &Self::Session) -> Result<RawTimeline, String> {
        let read = || -> windows::core::Result<RawTimeline> {
            let timeline = session.GetTimelineProperties()?;
            Ok(RawTimeline {
                start: timeline.StartTime()?.Duration,
                end: timeline.EndTime()?.Duration,
                position: timeline.Position()?.Duration,
                updated: timeline.LastUpdatedTime()?.UniversalTime,
            })
        };
        read().map_err(|error| error.to_string())
    }

    fn send(
        &self,
        (session, _): &Self::Session,
        command: Transport,
        deadline: Instant,
    ) -> Result<bool, String> {
        let operation = match command {
            Transport::Previous => session.TrySkipPreviousAsync(),
            Transport::PlayPause => session.TryTogglePlayPauseAsync(),
            Transport::Next => session.TrySkipNextAsync(),
        };
        operation
            .and_then(|operation| within(operation, deadline))
            .map_err(|error| error.to_string())
    }

    fn reset(&mut self) {
        self.manager = None;
    }

    fn fit_image(&self, image: &[u8]) -> Result<Option<Vec<u8>>, String> {
        media::fit_image(image).map_err(|error| error.to_string())
    }

    fn card_artwork(&self, image: &[u8]) -> Result<Option<CardArtwork>, String> {
        media::card_artwork(image).map_err(|error| error.to_string())
    }

    fn icon(&self, log: &DebugLog) -> Option<Arc<[u8]>> {
        media::fitted_icon(log)
    }
}

/// Each session described from its properties. They are asked for all at
/// once and waited for together for at most [`APP_WAIT`]; a session whose
/// app doesn't answer in time, or answers with an error, is left out.
fn describe_each<S, P, T>(
    sessions: Vec<S>,
    properties: impl Fn(&S) -> windows::core::Result<IAsyncOperation<P>>,
    describe: impl Fn(S, P) -> windows::core::Result<T>,
) -> Vec<T>
where
    P: RuntimeType + 'static,
{
    let asked: Vec<(S, IAsyncOperation<P>)> = sessions
        .into_iter()
        .filter_map(|session| {
            let operation = properties(&session).ok()?;
            Some((session, operation))
        })
        .collect();
    let deadline = Instant::now() + APP_WAIT;
    asked
        .into_iter()
        .filter_map(|(session, operation)| {
            within(operation, deadline)
                .and_then(|answer| describe(session, answer))
                .ok()
        })
        .collect()
}

/// The thumbnail's bytes, read within `deadline`.
fn read_thumbnail(
    open: IAsyncOperation<IRandomAccessStreamWithContentType>,
    deadline: Instant,
) -> windows::core::Result<Option<Vec<u8>>> {
    let stream = within(open, deadline)?;
    let size = stream.Size()?;
    if size > THUMBNAIL_LIMIT {
        return Err(windows::core::Error::new(
            E_FAIL,
            format!("the thumbnail has {size} bytes"),
        ));
    }
    let size = size as usize;
    let mut image = Vec::with_capacity(size);
    while image.len() < size {
        let wanted = (size - image.len()) as u32;
        let buffer = Buffer::Create(wanted)?;
        let filled = within(
            stream.ReadAsync(&buffer, wanted, InputStreamOptions::None)?,
            deadline,
        )?;
        let start = image.len();
        image.resize(start + filled.Length()? as usize, 0);
        if image.len() == start {
            break;
        }
        DataReader::FromBuffer(&filled)?.ReadBytes(&mut image[start..])?;
    }
    Ok((!image.is_empty()).then_some(image))
}

/// The result of `operation`, waited for until `deadline`. Past it the
/// operation is cancelled and the wait fails, so an app that never answers
/// costs the thread no more than that.
fn within<O, T>(operation: O, deadline: Instant) -> windows::core::Result<T>
where
    O: Interface + IntoFuture<Output = windows::core::Result<T>>,
{
    let info: IAsyncInfo = operation.cast()?;
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(operation.into_future());
    loop {
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            return result;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            // An app may ignore the cancel; the thread moves on either way.
            let _ = info.Cancel();
            return Err(windows::core::Error::new(
                ERROR_TIMEOUT.to_hresult(),
                "the app didn't answer in time",
            ));
        }
        thread::park_timeout(left);
    }
}

/// Wakes the waiting thread when an operation completes.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Condvar, Mutex, PoisonError};
    use std::time::Duration;

    use windows::Storage::Streams::RandomAccessStreamReference;
    use windows::core::HSTRING;

    use super::*;
    use crate::audio::now_playing::{Follow, TrackReader};
    use crate::now_playing_card::{CardSource, MediaControls};

    /// What hung operations wait on; opened when the test ends, so the
    /// thread pool's threads finish, and given up after `GATE_LIMIT` even if
    /// a test never ends.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
    }

    const GATE_LIMIT: Duration = Duration::from_secs(30);

    impl Gate {
        fn wait(&self) {
            let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
            drop(
                self.opened
                    .wait_timeout_while(open, GATE_LIMIT, |open| !*open),
            );
        }
    }

    struct OpensOnDrop(Arc<Gate>);

    impl Drop for OpensOnDrop {
        fn drop(&mut self) {
            *self.0.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            self.0.opened.notify_all();
        }
    }

    fn hung<T: RuntimeType + 'static>(gate: &Arc<Gate>) -> IAsyncOperation<T> {
        let gate = gate.clone();
        IAsyncOperation::spawn(move || {
            gate.wait();
            Err(E_FAIL.into())
        })
    }

    #[test]
    fn a_thumbnail_is_read_whole() {
        let _apartment = Apartment::enter().unwrap();
        let open = RandomAccessStreamReference::CreateFromStream(
            &media::memory_stream(media::ICON_PNG).unwrap(),
        )
        .unwrap()
        .OpenReadAsync()
        .unwrap();
        let image = read_thumbnail(open, Instant::now() + APP_WAIT).unwrap();
        assert_eq!(image.as_deref(), Some(media::ICON_PNG));
    }

    #[test]
    fn a_hung_thumbnail_is_given_up_at_the_deadline() {
        let _apartment = Apartment::enter().unwrap();
        let gate = Arc::new(Gate::default());
        let _open = OpensOnDrop(gate.clone());
        let started = Instant::now();
        assert!(read_thumbnail(hung(&gate), started + APP_WAIT).is_err());
        let took = started.elapsed();
        assert!(took >= APP_WAIT && took < APP_WAIT * 2, "took {took:?}");
    }

    #[test]
    fn a_hung_button_holds_the_thread_no_longer_than_the_wait() {
        let _apartment = Apartment::enter().unwrap();
        let gate = Arc::new(Gate::default());
        let _open = OpensOnDrop(gate.clone());
        let started = Instant::now();
        assert!(within(hung::<bool>(&gate), started + APP_WAIT).is_err());
        let took = started.elapsed();
        assert!(took >= APP_WAIT && took < APP_WAIT * 2, "took {took:?}");
    }

    #[test]
    fn a_session_whose_properties_hang_is_left_out_in_time() {
        let _apartment = Apartment::enter().unwrap();
        let gate = Arc::new(Gate::default());
        let _open = OpensOnDrop(gate.clone());
        let started = Instant::now();
        let described = describe_each(
            vec![0, 1, 2],
            |&session| {
                let gate = gate.clone();
                Ok(IAsyncOperation::spawn(move || {
                    if session == 1 {
                        gate.wait();
                    }
                    Ok(HSTRING::from(format!("session {session}")))
                }))
            },
            |session, name: HSTRING| Ok((session, name.to_string())),
        );
        let took = started.elapsed();
        assert!(took >= APP_WAIT && took < APP_WAIT * 2, "took {took:?}");
        let names = |session: i32| (session, format!("session {session}"));
        assert_eq!(described, vec![names(0), names(2)]);
    }

    /// Reads this PC's media sessions; play something first. Run with
    /// `cargo test -- --ignored --nocapture reads_this_pcs`.
    #[test]
    #[ignore = "reads this PC's media sessions"]
    fn reads_this_pcs_media_sessions() {
        let (mut track, mut card) = read_media_sessions(DebugLog::default());
        track.follow(Some(Follow::AnyApp));
        card.follow(Some(CardSource::AnyApp));
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut reading, mut shown) = (None, None);
        while reading.is_none() || shown.is_none() {
            assert!(Instant::now() < deadline, "no reading");
            reading = reading.or_else(|| track.take_reading());
            shown = shown.or_else(|| card.take_reading());
            thread::sleep(Duration::from_millis(50));
        }
        let (reading, shown) = (reading.unwrap(), shown.unwrap());
        let icon = track
            .icon()
            .expect("the icon is made before the first reading");
        println!("icon: {} bytes", icon.len());
        match reading.track {
            Some(track) => {
                let artwork = track.artwork.map(|jpeg| jpeg.len());
                println!("{:?}, artwork: {artwork:?} bytes", track.session);
                let progress = track
                    .timeline
                    .and_then(|timeline| timeline.progress_at(Instant::now()));
                println!("timeline: {:?}, progress now: {progress:?}", track.timeline);
            }
            None => println!("no media session counts"),
        }
        if let Some(shown) = shown {
            println!("card: {} / {}", shown.title, shown.subtitle());
        }
    }
}
