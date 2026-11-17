//! Noticing an audio session created or turning active on any active render
//! endpoint, so the double-audio check and the app's move run at once when an
//! app starts or resumes playing (S471) instead of at the next interval.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Media::Audio::{
    AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateActive, DEVICE_STATE_ACTIVE,
    IAudioSessionControl, IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
use windows_core::{ComObject, GUID, IUnknownImpl, Interface, PCWSTR, implement};

use super::com::{
    Apartment, Event, READY_TIMEOUT, Ready, WINDOWS_AUDIO, Worker, spawn_worker_within,
    system_error,
};
use super::devices::enumerator;
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

    /// Whether a session was created or turned active since the last call.
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
    // SAFETY: waits on an event that outlives this thread's use of it.
    unsafe { WaitForSingleObject(stop.raw(), INFINITE) };
    for manager in registered {
        // SAFETY: unregisters the listener registered on this manager.
        let _ = unsafe { manager.UnregisterSessionNotification(&listener) };
    }
    let sessions = std::mem::take(&mut *followed.lock().unwrap_or_else(|e| e.into_inner()));
    for session in sessions {
        // SAFETY: unregisters the listener registered on this session.
        let _ = unsafe { session.UnregisterAudioSessionNotification(&events) };
    }
}

/// Registers `events` for the state changes of `session`, kept in `followed`
/// for the unregistering at the end.
fn follow(
    events: &IAudioSessionEvents,
    followed: &Mutex<Vec<IAudioSessionControl>>,
    session: &IAudioSessionControl,
) {
    // SAFETY: registers a live listener on a live session.
    if unsafe { session.RegisterAudioSessionNotification(events) }.is_ok() {
        followed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(session.clone());
    }
}

/// Raises the flag when a session is created or turns active: a paused app
/// that plays on resumes its old session. Windows calls it on its own thread
/// and asks it to return quickly, so it only marks and registers.
#[implement(IAudioSessionNotification, IAudioSessionEvents)]
struct SessionListener {
    created: Arc<AtomicBool>,
    followed: Mutex<Vec<IAudioSessionControl>>,
}

impl IAudioSessionNotification_Impl for SessionListener_Impl {
    fn OnSessionCreated(
        &self,
        session: windows_core::Ref<IAudioSessionControl>,
    ) -> windows_core::Result<()> {
        self.created.store(true, Ordering::Release);
        if let Ok(session) = session.ok() {
            follow(&self.to_interface(), &self.followed, session);
        }
        Ok(())
    }
}

impl IAudioSessionEvents_Impl for SessionListener_Impl {
    fn OnStateChanged(&self, state: AudioSessionState) -> windows_core::Result<()> {
        if state == AudioSessionStateActive {
            self.created.store(true, Ordering::Release);
        }
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::{
        AudioSessionStateActive, AudioSessionStateInactive, IAudioSessionEvents,
    };
    use windows_core::Interface;

    // covers: S471
    #[test]
    fn a_session_turning_active_counts_as_a_new_one() {
        let created = Arc::new(AtomicBool::new(false));
        let listener: IAudioSessionNotification = SessionListener {
            created: Arc::clone(&created),
            followed: Mutex::default(),
        }
        .into();
        let events: IAudioSessionEvents = listener.cast().expect("follows session states");
        // SAFETY: calls on a live listener this test owns.
        unsafe { events.OnStateChanged(AudioSessionStateInactive) }.unwrap();
        assert!(!created.load(Ordering::Acquire), "a pause is no start");
        // SAFETY: as above.
        unsafe { events.OnStateChanged(AudioSessionStateActive) }.unwrap();
        assert!(created.load(Ordering::Acquire));
    }
}
