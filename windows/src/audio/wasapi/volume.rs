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
use std::time::Duration;

use windows::Win32::Media::Audio::AUDIO_VOLUME_NOTIFICATION_DATA;
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::core::HSTRING;
use windows_core::implement;

use super::com::{Apartment, READY_TIMEOUT, Ready, device_error, spawn_ready};
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
        VolumeWatcher::start_with(READY_TIMEOUT, listen)
    }

    /// The watcher whose listener thread runs `body`.
    fn start_with(
        _answer_timeout: Duration,
        body: impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static,
    ) -> Result<VolumeWatcher, BackendError> {
        let (commands, received) = mpsc::channel();
        let (thread, ()) = spawn_ready("audiosync-volumes", move |ready| {
            body(&received, ready);
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
    if !ready.ok(()) {
        // Nobody waits for this listener thread anymore.
        return;
    }
    serve(commands, |id, flag| Registration::new(&enumerator, id, flag));
}

/// Registers listeners through `register` and drops them on request, until
/// the watcher goes; then drops the rest, which unregisters them.
fn serve<R>(
    commands: &mpsc::Receiver<Command>,
    mut register: impl FnMut(&str, Arc<AtomicBool>) -> Result<R, BackendError>,
) {
    let mut registrations: BTreeMap<String, R> = BTreeMap::new();
    while let Ok(command) = commands.recv() {
        match command {
            Command::Watch { id, flag, reply } => {
                let answer = register(&id, flag).map(|registration| {
                    registrations.insert(id, registration);
                });
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
    use std::sync::{Mutex, PoisonError};
    use std::thread;
    use std::time::Instant;

    use super::*;

    /// What the fake listener thread did, in order: "+id" for a listener
    /// registered, "-id" for one dropped.
    #[derive(Clone, Default)]
    struct Done(Arc<Mutex<Vec<String>>>);

    impl Done {
        fn push(&self, entry: String) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(entry);
        }

        fn list(&self) -> Vec<String> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }

        /// Waits up to [`LIMIT`] for the list to read `expected`.
        fn wait_for(&self, expected: &[&str]) {
            let until = Instant::now() + LIMIT;
            while self.list() != expected && Instant::now() < until {
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(self.list(), expected);
        }
    }

    /// A fake listener, dropped as the real one unregisters.
    struct FakeRegistration {
        id: String,
        done: Done,
    }

    impl Drop for FakeRegistration {
        fn drop(&mut self) {
            self.done.push(format!("-{}", self.id));
        }
    }

    /// A listener thread whose every registration waits for a word on
    /// `gate`, as a Windows call that hangs until Windows audio answers.
    fn hanging(
        gate: mpsc::Receiver<()>,
        done: Done,
    ) -> impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static {
        move |commands, ready| {
            if ready.ok(()) {
                serve(commands, |id, _flag| {
                    gate.recv().map_err(|_| BackendError::NotResponding)?;
                    done.push(format!("+{id}"));
                    Ok(FakeRegistration {
                        id: id.to_owned(),
                        done: done.clone(),
                    })
                });
            }
        }
    }

    /// How long the watcher waits for an answer in these tests.
    const TIMEOUT: Duration = Duration::from_millis(200);
    /// How long a test waits before it calls a call hung.
    const LIMIT: Duration = Duration::from_secs(5);

    /// Runs `call` on a thread of its own and returns what it returned,
    /// failing the test when that takes over [`LIMIT`].
    fn within<T: Send + 'static>(call: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(call());
        });
        receiver
            .recv_timeout(LIMIT)
            .expect("the call returns within the time it is given")
    }

    #[test]
    fn a_volume_listener_that_doesnt_answer_in_time_is_given_up_and_its_late_registration_undone()
    {
        let (gate, hold) = mpsc::channel();
        let done = Done::default();
        let watcher =
            VolumeWatcher::start_with(TIMEOUT, hanging(hold, done.clone())).expect("it starts");
        // Windows audio hangs in the first registration: given up after the
        // wait, as no device volume for now.
        let (mut watcher, answer) = within(move || {
            let mut watcher = watcher;
            let answer = watcher.watch("speakers");
            (watcher, answer)
        });
        assert_eq!(answer, Err(BackendError::NotResponding));
        // While it still hangs, another output fails at once, with no
        // second wait.
        let asked = Instant::now();
        assert_eq!(
            watcher.watch("headphones"),
            Err(BackendError::NotResponding)
        );
        assert!(asked.elapsed() < TIMEOUT, "{:?}", asked.elapsed());
        // It answers after all: the listener nobody waited for goes at once.
        gate.send(()).expect("the thread waits");
        done.wait_for(&["+speakers", "-speakers"]);
        assert!(watcher.take_changed().is_empty());
        // Windows audio answers again: the next registration works.
        gate.send(()).expect("the thread waits");
        assert_eq!(watcher.watch("headphones"), Ok(()));
        done.wait_for(&["+speakers", "-speakers", "+headphones"]);

        // Hung again, the watcher goes (AudioSync quits) without waiting for
        // the thread, which drops every listener once its call returns.
        assert_eq!(watcher.watch("speakers"), Err(BackendError::NotResponding));
        within(move || drop(watcher));
        gate.send(()).expect("the thread waits");
        done.wait_for(&[
            "+speakers",
            "-speakers",
            "+headphones",
            "+speakers",
            "-speakers",
            "-headphones",
        ]);
    }

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
