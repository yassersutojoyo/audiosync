//! Noticing a new audio session on any active render endpoint, so the
//! double-audio check and the app's move run at once when an app starts to
//! play (S471) instead of at the next interval.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioSessionControl, IAudioSessionManager2, IAudioSessionNotification,
    IAudioSessionNotification_Impl, eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
use windows_core::implement;

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

    /// Whether a session was created since the last call.
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
    let listener: IAudioSessionNotification = SessionListener {
        created: Arc::clone(created),
    }
    .into();
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
            if manager.GetSessionEnumerator().is_ok()
                && manager.RegisterSessionNotification(&listener).is_ok()
            {
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
}

/// Raises the flag when a session is created. Windows calls it on its own
/// thread and asks it to return quickly, so it only marks.
#[implement(IAudioSessionNotification)]
struct SessionListener {
    created: Arc<AtomicBool>,
}

impl IAudioSessionNotification_Impl for SessionListener_Impl {
    fn OnSessionCreated(
        &self,
        _session: windows_core::Ref<IAudioSessionControl>,
    ) -> windows_core::Result<()> {
        self.created.store(true, Ordering::Release);
        Ok(())
    }
}
