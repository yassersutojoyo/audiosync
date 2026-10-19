//! Each output's own volume and mute in Windows (`IAudioEndpointVolume`,
//! spec S194, S195, S197): reading them, following their changes, and the
//! fix the user clicks.
//!
//! Reads and writes activate the endpoint's volume control afresh on the
//! calling thread, like the double-audio snapshot. Change listeners need a
//! home that outlives a call, so a thread of its own (in the multithreaded
//! apartment) registers and unregisters them on request and keeps them until
//! asked to drop them or until the watcher goes. Windows calls a listener on
//! a system thread; it only raises the endpoint's flag, which
//! [`VolumeWatcher::take_changed`] collects on the thread that polls, so
//! several notifications in between count once. Nothing here runs on an
//! audio thread.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::Win32::Media::Audio::AUDIO_VOLUME_NOTIFICATION_DATA;
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::core::HSTRING;
use windows_core::implement;

use super::com::{Apartment, Ready, device_error, spawn_ready};
use super::devices::enumerator;
use crate::audio::backend::BackendError;
use crate::audio::routing::EndpointVolume;

/// The volume control of the endpoint `id`. COM must be initialized on the
/// calling thread.
fn control(
    enumerator: &IMMDeviceEnumerator,
    id: &str,
) -> Result<IAudioEndpointVolume, BackendError> {
    // SAFETY: calls on a live enumerator and the device it returns.
    unsafe {
        let device = enumerator
            .GetDevice(&HSTRING::from(id))
            .map_err(|error| device_error("IMMDeviceEnumerator::GetDevice", &error))?;
        device
            .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            .map_err(|error| device_error("IMMDevice::Activate(IAudioEndpointVolume)", &error))
    }
}

/// The volume control of the endpoint `id`, on the calling thread.
fn with_control<T>(
    id: &str,
    call: impl FnOnce(&IAudioEndpointVolume) -> Result<T, BackendError>,
) -> Result<T, BackendError> {
    let _apartment = Apartment::enter()?;
    let control = control(&enumerator()?, id)?;
    call(&control)
}

/// The endpoint's mute and master volume (the Sound settings slider), or
/// `None` when its volume control can't be activated or read: then both are
/// unknown (S194).
pub(super) fn read(id: &str) -> Option<EndpointVolume> {
    with_control(id, |control| {
        // SAFETY: two reads on a live volume control.
        unsafe {
            let muted = control
                .GetMute()
                .map_err(|error| device_error("IAudioEndpointVolume::GetMute", &error))?
                .as_bool();
            let level = control.GetMasterVolumeLevelScalar().map_err(|error| {
                device_error("IAudioEndpointVolume::GetMasterVolumeLevelScalar", &error)
            })?;
            Ok(EndpointVolume { muted, level })
        }
    })
    .ok()
}

/// Sets the endpoint's master volume, 0 to 1: the fix the user clicked.
pub(super) fn set_level(id: &str, level: f32) -> Result<(), BackendError> {
    with_control(id, |control| {
        // SAFETY: one write on a live volume control; no event context.
        unsafe { control.SetMasterVolumeLevelScalar(level, std::ptr::null()) }.map_err(|error| {
            device_error("IAudioEndpointVolume::SetMasterVolumeLevelScalar", &error)
        })
    })
}

/// Unmutes the endpoint: the fix the user clicked.
pub(super) fn unmute(id: &str) -> Result<(), BackendError> {
    with_control(id, |control| {
        // SAFETY: one write on a live volume control; no event context.
        unsafe { control.SetMute(false, std::ptr::null()) }
            .map_err(|error| device_error("IAudioEndpointVolume::SetMute", &error))
    })
}

/// What the listener thread is asked to do.
enum Command {
    /// Register a listener for the endpoint that raises `flag`, and answer.
    Watch {
        id: String,
        flag: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<(), BackendError>>,
    },
    /// Unregister the endpoint's listener.
    Unwatch(String),
}

/// Follows the volume of the endpoints it is asked to, through listeners
/// that a thread of its own holds. Dropping it unregisters them all.
pub(super) struct VolumeWatcher {
    commands: Option<mpsc::Sender<Command>>,
    /// The flag of each endpoint followed.
    flags: BTreeMap<String, Arc<AtomicBool>>,
    thread: Option<JoinHandle<()>>,
}

impl VolumeWatcher {
    pub(super) fn start() -> Result<VolumeWatcher, BackendError> {
        let (commands, received) = mpsc::channel();
        let (thread, ()) = spawn_ready("audiosync-volumes", move |ready| {
            listen(&received, ready);
        })?;
        Ok(VolumeWatcher {
            commands: Some(commands),
            flags: BTreeMap::new(),
            thread: Some(thread),
        })
    }

    /// Registers a change listener for the endpoint `id`; waits for the
    /// listener thread's answer.
    pub(super) fn watch(&mut self, id: &str) -> Result<(), BackendError> {
        self.unwatch(id);
        let flag = Arc::new(AtomicBool::new(false));
        let (reply, answer) = mpsc::channel();
        let gone = BackendError::System {
            call: "the volume listener thread",
            code: windows::Win32::Foundation::E_FAIL.0,
        };
        let commands = self.commands.as_ref().ok_or_else(|| gone.clone())?;
        commands
            .send(Command::Watch {
                id: id.to_owned(),
                flag: Arc::clone(&flag),
                reply,
            })
            .map_err(|_| gone.clone())?;
        answer.recv().map_err(|_| gone)??;
        self.flags.insert(id.to_owned(), flag);
        Ok(())
    }

    /// Unregisters the endpoint's listener, if it has one.
    pub(super) fn unwatch(&mut self, id: &str) {
        if self.flags.remove(id).is_some()
            && let Some(commands) = &self.commands
        {
            let _ = commands.send(Command::Unwatch(id.to_owned()));
        }
    }

    /// The endpoints whose listener was called since the last call.
    pub(super) fn take_changed(&self) -> Vec<String> {
        self.flags
            .iter()
            .filter(|(_, flag)| flag.swap(false, Ordering::AcqRel))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

impl Drop for VolumeWatcher {
    fn drop(&mut self) {
        // The thread ends, unregistering every listener, once the channel
        // closes.
        self.commands = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The listener thread: registers and unregisters listeners on request, and
/// unregisters the rest when the watcher goes.
fn listen(commands: &mpsc::Receiver<Command>, ready: Ready) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => return ready.fail(error),
    };
    let enumerator = match enumerator() {
        Ok(enumerator) => enumerator,
        Err(error) => return ready.fail(error),
    };
    ready.ok(());
    let mut registrations: BTreeMap<String, Registration> = BTreeMap::new();
    while let Ok(command) = commands.recv() {
        match command {
            Command::Watch { id, flag, reply } => {
                let registered = Registration::new(&enumerator, &id, flag);
                let answer = registered.as_ref().map(|_| ()).map_err(Clone::clone);
                if let Ok(registration) = registered {
                    registrations.insert(id, registration);
                }
                let _ = reply.send(answer);
            }
            Command::Unwatch(id) => {
                registrations.remove(&id);
            }
        }
    }
    drop(registrations);
}

/// A registered listener on one endpoint's volume control, unregistered on
/// drop.
struct Registration {
    control: IAudioEndpointVolume,
    listener: IAudioEndpointVolumeCallback,
}

impl Registration {
    fn new(
        enumerator: &IMMDeviceEnumerator,
        id: &str,
        flag: Arc<AtomicBool>,
    ) -> Result<Registration, BackendError> {
        let control = control(enumerator, id)?;
        let listener: IAudioEndpointVolumeCallback = VolumeListener { flag }.into();
        // SAFETY: the listener stays alive and registered until this value
        // unregisters it on drop.
        unsafe { control.RegisterControlChangeNotify(&listener) }.map_err(|error| {
            device_error("IAudioEndpointVolume::RegisterControlChangeNotify", &error)
        })?;
        Ok(Registration { control, listener })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: unregisters the listener registered on this control.
        let _ = unsafe { self.control.UnregisterControlChangeNotify(&self.listener) };
    }
}

/// Raises its endpoint's flag when Windows reports a change of the mute or
/// a volume. Windows calls it on a system thread and asks it to return
/// quickly, so it doesn't read the data it is given: the poll reads both
/// parts afresh (S202). `#[implement]` objects are agile, like the device
/// listener.
#[implement(IAudioEndpointVolumeCallback)]
struct VolumeListener {
    flag: Arc<AtomicBool>,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeListener_Impl {
    fn OnNotify(&self, _data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows_core::Result<()> {
        self.flag.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads this PC's endpoint volumes and follows them for a moment,
    /// silently and changing nothing. Ignored by default since it needs
    /// Windows audio; run it on a PC with
    /// `cargo test real_endpoint_volumes -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the PC's own audio endpoints"]
    fn real_endpoint_volumes_are_read_and_followed_without_changing_them() {
        let listing = super::super::devices::render_endpoints().expect("the endpoints are listed");
        let mut watcher = VolumeWatcher::start().expect("the listener thread starts");
        for endpoint in &listing.endpoints {
            let before = read(&endpoint.id);
            println!("{:?}: {before:?}", endpoint.name);
            if let Some(volume) = before {
                assert!((0.0..=1.0).contains(&volume.level), "{volume:?}");
            }
            watcher.watch(&endpoint.id).expect("a listener registers");
            assert_eq!(read(&endpoint.id), before, "nothing changed");
        }
        println!("changed meanwhile: {:?}", watcher.take_changed());
        for endpoint in &listing.endpoints {
            watcher.unwatch(&endpoint.id);
        }
        drop(watcher);
    }
}
