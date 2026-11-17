//! Noticing an audio session created, or changing state, on any active render
//! endpoint, so the double-audio check and the app's move run at once when an
//! app starts, resumes or leaves an output (S470, S471) instead of at the next
//! interval.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{S_OK, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AudioSessionDisconnectReason, AudioSessionState, DEVICE_STATE_ACTIVE, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows_core::{ComObject, GUID, IUnknownImpl, Interface, PCWSTR, implement};

use super::com::{
    Apartment, Event, READY_TIMEOUT, Ready, WINDOWS_AUDIO, Worker, spawn_worker_within,
    system_error,
};
use super::devices::enumerator;
use super::processes::ProcessWatch;
use crate::audio::backend::BackendError;

/// Follows the render endpoints active when it starts; a thread of its own
/// holds the registrations, as [`super::devices::DeviceWatcher`] does.
pub(super) struct SessionWatcher {
    created: Arc<AtomicBool>,
    stop: Arc<Event>,
    thread: Worker,
}

impl SessionWatcher {
    pub(super) fn start() -> Result<SessionWatcher, BackendError> {
        let created = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(Event::manual_reset()?);
        let (thread, ()) = {
            let created = Arc::clone(&created);
            let stop = Arc::clone(&stop);
            spawn_worker_within(
                "audiosync-sessions",
                READY_TIMEOUT,
                &WINDOWS_AUDIO,
                move |ready| {
                    watch(&created, &stop, ready);
                },
            )?
        };
        Ok(SessionWatcher {
            created,
            stop,
            thread,
        })
    }

    /// Whether a session was created or changed state since the last call.
    pub(super) fn take_created(&self) -> bool {
        self.created.swap(false, Ordering::AcqRel)
    }
}

impl Drop for SessionWatcher {
    fn drop(&mut self) {
        // The thread unregisters its listeners and ends.
        self.stop.set();
        self.thread.finish();
    }
}

/// The watcher thread: registers on every active render endpoint, waits for
/// the stop request, unregisters. An endpoint that fails is left out.
fn watch(created: &Arc<AtomicBool>, stop: &Event, ready: Ready) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => return ready.fail(error),
    };
    let enumerator = match enumerator() {
        Ok(enumerator) => enumerator,
        Err(error) => return ready.fail(error),
    };
    // SAFETY: a call on the live enumerator.
    let devices = match unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) } {
        Ok(devices) => devices,
        Err(error) => {
            return ready.fail(system_error(
                "IMMDeviceEnumerator::EnumAudioEndpoints",
                &error,
            ));
        }
    };
    let object = ComObject::new(SessionListener {
        created: Arc::clone(created),
        wake: wake_registered,
        followed: Mutex::default(),
    });
    let followed = &object.followed;
    let listener: IAudioSessionNotification = object.to_interface();
    let events = match listener.cast::<IAudioSessionEvents>() {
        Ok(events) => events,
        Err(error) => return ready.fail(system_error("IAudioSessionEvents", &error)),
    };
    let mut registered = Vec::new();
    // SAFETY: calls on live COM objects this thread owns; each manager keeps
    // the listener registered until it is unregistered below.
    unsafe {
        for index in 0..devices.GetCount().unwrap_or(0) {
            let Ok(manager) = devices
                .Item(index)
                .and_then(|device| device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None))
            else {
                continue;
            };
            // Windows sends no notices to a manager whose sessions were never
            // listed.
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                continue;
            };
            for index in 0..sessions.GetCount().unwrap_or(0) {
                if let Ok(session) = sessions.GetSession(index) {
                    follow(&events, followed, &session);
                }
            }
            if manager.RegisterSessionNotification(&listener).is_ok() {
                registered.push(manager);
            }
        }
    }
    if !ready.ok(()) {
        return;
    }
    // Sessions of apps that exited are let go here, never in a callback,
    // so the list doesn't grow while the app runs for days.
    // SAFETY: waits on an event that outlives this thread's use of it.
    while unsafe { WaitForSingleObject(stop.raw(), PRUNE_EVERY_MS) } == WAIT_TIMEOUT {
        let gone = followed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_gone(process_gone);
        unfollow(&events, gone);
    }
    for manager in registered {
        // SAFETY: unregisters the listener registered on this manager.
        let _ = unsafe { manager.UnregisterSessionNotification(&listener) };
    }
    let sessions = followed.lock().unwrap_or_else(|e| e.into_inner()).stop();
    unfollow(&events, sessions);
}

const PRUNE_EVERY_MS: u32 = 10_000;

static WAKE: OnceLock<fn()> = OnceLock::new();

/// Has `wake` called on Windows' thread when a session is created or changes
/// state and none was waiting to be taken, so the outputs the app plays on
/// are held out, and those it left handed back, then rather than at the next
/// poll (S470). The first call wins.
#[cfg_attr(test, allow(dead_code))]
pub fn on_new_session(wake: fn()) {
    let _ = WAKE.set(wake);
}

fn wake_registered() {
    if let Some(wake) = WAKE.get() {
        wake();
    }
}

/// Registers `events` for the state changes of `session`, kept in `followed`
/// for the unregistering at the end.
fn follow(
    events: &IAudioSessionEvents,
    followed: &Mutex<Followed<IAudioSessionControl>>,
    session: &IAudioSessionControl,
) {
    followed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .follow(|| {
            // SAFETY: registers a live listener on a live session.
            unsafe { session.RegisterAudioSessionNotification(events) }
                .ok()
                .map(|()| session.clone())
        });
}

/// Unregisters `events` from each of `sessions` and lets them go.
fn unfollow(events: &IAudioSessionEvents, sessions: Vec<IAudioSessionControl>) {
    for session in sessions {
        // SAFETY: unregisters the listener registered on this session.
        let _ = unsafe { session.UnregisterAudioSessionNotification(events) };
    }
}

/// Whether the process of `session` has exited. A session of the system
/// sounds or one whose process can't be read is kept.
fn process_gone(session: &IAudioSessionControl) -> bool {
    let Ok(session) = session.cast::<IAudioSessionControl2>() else {
        return false;
    };
    // SAFETY: calls on a live session.
    if unsafe { session.IsSystemSoundsSession() } == S_OK {
        return false;
    }
    // SAFETY: as above.
    unsafe { session.GetProcessId() }
        .is_ok_and(|pid| matches!(ProcessWatch::open(pid), Err(BackendError::SourceGone)))
}

/// The sessions followed for their state changes, until the watcher stops.
struct Followed<S> {
    sessions: Vec<S>,
    stopped: bool,
}

impl<S> Default for Followed<S> {
    fn default() -> Self {
        Followed {
            sessions: Vec::new(),
            stopped: false,
        }
    }
}

impl<S> Followed<S> {
    /// Keeps the session `register` gives; registers nothing once stopped,
    /// when nothing would unregister it.
    fn follow(&mut self, register: impl FnOnce() -> Option<S>) {
        if self.stopped {
            return;
        }
        if let Some(session) = register() {
            self.sessions.push(session);
        }
    }

    /// Takes out the sessions `gone` says are over.
    fn take_gone(&mut self, gone: impl Fn(&S) -> bool) -> Vec<S> {
        let (over, kept) = std::mem::take(&mut self.sessions)
            .into_iter()
            .partition(|session| gone(session));
        self.sessions = kept;
        over
    }

    /// Takes out every session, and keeps none after.
    fn stop(&mut self) -> Vec<S> {
        self.stopped = true;
        std::mem::take(&mut self.sessions)
    }
}

/// Raises the flag when a session is created or changes state: a paused app
/// that plays on resumes its old session, and an app moved to another output
/// leaves its session on the old one. Windows calls it on its own thread
/// and asks it to return quickly, so it only marks, wakes and registers.
#[implement(IAudioSessionNotification, IAudioSessionEvents)]
struct SessionListener {
    created: Arc<AtomicBool>,
    wake: fn(),
    followed: Mutex<Followed<IAudioSessionControl>>,
}

impl SessionListener {
    /// One wake per flag raised: the first taker handles every session since.
    fn mark(&self) {
        if !self.created.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }
}

impl IAudioSessionNotification_Impl for SessionListener_Impl {
    fn OnSessionCreated(
        &self,
        session: windows_core::Ref<IAudioSessionControl>,
    ) -> windows_core::Result<()> {
        self.mark();
        if let Ok(session) = session.ok() {
            follow(&self.to_interface(), &self.followed, session);
        }
        Ok(())
    }
}

impl IAudioSessionEvents_Impl for SessionListener_Impl {
    fn OnStateChanged(&self, _: AudioSessionState) -> windows_core::Result<()> {
        self.mark();
        Ok(())
    }

    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(
        &self,
        _: f32,
        _: windows_core::BOOL,
        _: *const GUID,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnChannelVolumeChanged(
        &self,
        _: u32,
        _: *const f32,
        _: u32,
        _: *const GUID,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> windows_core::Result<()> {
        self.mark();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::{
        AudioSessionStateActive, AudioSessionStateExpired, AudioSessionStateInactive,
        DisconnectReasonDeviceRemoval, IAudioSessionEvents,
    };
    use windows_core::Interface;

    // covers: S471
    #[test]
    fn a_session_turning_active_counts_as_a_new_one_and_wakes_once() {
        static WOKEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        fn woken() {
            WOKEN.fetch_add(1, Ordering::AcqRel);
        }
        let created = Arc::new(AtomicBool::new(false));
        let listener: IAudioSessionNotification = SessionListener {
            created: Arc::clone(&created),
            wake: woken,
            followed: Mutex::default(),
        }
        .into();
        let events: IAudioSessionEvents = listener.cast().expect("follows session states");
        // SAFETY: calls on a live listener this test owns.
        unsafe { events.OnStateChanged(AudioSessionStateActive) }.unwrap();
        assert!(created.load(Ordering::Acquire));
        assert_eq!(WOKEN.load(Ordering::Acquire), 1);
        // SAFETY: as above.
        unsafe { events.OnStateChanged(AudioSessionStateActive) }.unwrap();
        assert_eq!(
            WOKEN.load(Ordering::Acquire),
            1,
            "not taken yet: no second wake"
        );
    }

    // covers: S470
    #[test]
    fn a_session_turning_inactive_expiring_or_disconnecting_wakes_the_check() {
        static WOKEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        fn woken() {
            WOKEN.fetch_add(1, Ordering::AcqRel);
        }
        let created = Arc::new(AtomicBool::new(false));
        let listener: IAudioSessionNotification = SessionListener {
            created: Arc::clone(&created),
            wake: woken,
            followed: Mutex::default(),
        }
        .into();
        let events: IAudioSessionEvents = listener.cast().expect("follows session states");
        // SAFETY: calls on a live listener this test owns.
        unsafe { events.OnStateChanged(AudioSessionStateInactive) }.unwrap();
        assert!(created.swap(false, Ordering::AcqRel), "inactive");
        // SAFETY: as above.
        unsafe { events.OnStateChanged(AudioSessionStateExpired) }.unwrap();
        assert!(created.swap(false, Ordering::AcqRel), "expired");
        // SAFETY: as above.
        unsafe { events.OnSessionDisconnected(DisconnectReasonDeviceRemoval) }.unwrap();
        assert!(created.swap(false, Ordering::AcqRel), "disconnected");
        assert_eq!(WOKEN.load(Ordering::Acquire), 3);
    }

    // covers: S471
    #[test]
    fn a_session_whose_process_is_gone_is_let_go_and_none_is_kept_after_the_stop() {
        let mut followed = Followed::default();
        followed.follow(|| Some(1));
        followed.follow(|| Some(2));
        followed.follow(|| None);
        assert_eq!(followed.take_gone(|&pid| pid == 1), [1]);
        assert_eq!(followed.stop(), [2]);
        followed.follow(|| panic!("registered after the stop"));
        assert!(followed.stop().is_empty());
    }
}
