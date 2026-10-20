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
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
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

use super::com::{Apartment, READY_TIMEOUT, Ready, device_error, spawn_ready_within};
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
    /// Read the endpoint's mute and master volume, and answer.
    Read {
        id: String,
        reply: mpsc::Sender<Result<EndpointVolume, BackendError>>,
    },
    /// Set the endpoint's master volume, 0 to 1, and answer.
    SetLevel {
        id: String,
        level: f32,
        reply: mpsc::Sender<Result<(), BackendError>>,
    },
    /// Unmute the endpoint, and answer.
    Unmute {
        id: String,
        reply: mpsc::Sender<Result<(), BackendError>>,
    },
}

/// The Windows calls the listener thread makes on the endpoints: through
/// its own enumerator ([`Windows`]), or a fake in the tests.
trait Endpoints {
    /// A registered change listener, unregistered on drop.
    type Listener;

    /// Registers a change listener on the endpoint `id` that raises `flag`.
    fn listen(&self, id: &str, flag: Arc<AtomicBool>) -> Result<Self::Listener, BackendError>;

    /// The endpoint's mute and master volume; an error when its volume
    /// control can't be activated or read (S194).
    fn read(&self, id: &str) -> Result<EndpointVolume, BackendError>;

    /// Sets the endpoint's master volume, 0 to 1.
    fn set_level(&self, id: &str, level: f32) -> Result<(), BackendError>;

    /// Unmutes the endpoint.
    fn unmute(&self, id: &str) -> Result<(), BackendError>;
}

/// The endpoints as Windows has them, through the listener thread's own
/// enumerator.
struct Windows(IMMDeviceEnumerator);

impl Endpoints for Windows {
    type Listener = Registration;

    fn listen(&self, id: &str, flag: Arc<AtomicBool>) -> Result<Registration, BackendError> {
        Registration::new(&self.0, id, flag)
    }

    fn read(&self, id: &str) -> Result<EndpointVolume, BackendError> {
        let control = control(&self.0, id)?;
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
    }

    fn set_level(&self, id: &str, level: f32) -> Result<(), BackendError> {
        let control = control(&self.0, id)?;
        // SAFETY: one write on a live volume control; no event context.
        unsafe { control.SetMasterVolumeLevelScalar(level, std::ptr::null()) }.map_err(|error| {
            device_error("IAudioEndpointVolume::SetMasterVolumeLevelScalar", &error)
        })
    }

    fn unmute(&self, id: &str) -> Result<(), BackendError> {
        let control = control(&self.0, id)?;
        // SAFETY: one write on a live volume control; no event context.
        unsafe { control.SetMute(false, std::ptr::null()) }
            .map_err(|error| device_error("IAudioEndpointVolume::SetMute", &error))
    }
}

/// Follows the volume of the endpoints it is asked to, through listeners
/// that a thread of its own holds. Dropping it unregisters them all.
///
/// A registration is a Windows call that can hang (Windows audio still
/// waking up, say), and the thread that asks for it is the UI thread, so it
/// waits for the answer as long as an output thread's start at most
/// ([`READY_TIMEOUT`]). One that doesn't come in time is given up: the
/// endpoint isn't followed for now, and the listener thread drops that
/// listener as soon as its call returns. Until then the thread is stuck, so
/// further registrations fail at once instead of each waiting again.
pub(super) struct VolumeWatcher {
    commands: Option<mpsc::Sender<Command>>,
    /// The flag of each endpoint followed.
    flags: BTreeMap<String, Arc<AtomicBool>>,
    /// How long a registration's answer is waited for.
    answer_timeout: Duration,
    /// Where the answer to the registration given up last comes, until it
    /// does.
    late: Option<mpsc::Receiver<Result<(), BackendError>>>,
    thread: Option<JoinHandle<()>>,
}

impl VolumeWatcher {
    pub(super) fn start() -> Result<VolumeWatcher, BackendError> {
        VolumeWatcher::start_with(READY_TIMEOUT, listen)
    }

    /// The watcher whose listener thread runs `body`, waiting
    /// `answer_timeout` at most for each registration's answer.
    fn start_with(
        answer_timeout: Duration,
        body: impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static,
    ) -> Result<VolumeWatcher, BackendError> {
        let (commands, received) = mpsc::channel();
        let (thread, ()) = spawn_ready_within("audiosync-volumes", answer_timeout, move |ready| {
            body(&received, ready);
        })?;
        Ok(VolumeWatcher {
            commands: Some(commands),
            flags: BTreeMap::new(),
            answer_timeout,
            late: None,
            thread: Some(thread),
        })
    }

    /// Registers a change listener for the endpoint `id`; waits for the
    /// listener thread's answer, for the answer timeout at most. Returns
    /// [`BackendError::NotResponding`] when the answer doesn't come in time,
    /// or at once while the thread is still in a registration given up
    /// before; the endpoint isn't followed then.
    pub(super) fn watch(&mut self, id: &str) -> Result<(), BackendError> {
        self.unwatch(id);
        if self.stuck() {
            return Err(BackendError::NotResponding);
        }
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
        match answer.recv_timeout(self.answer_timeout) {
            Ok(answered) => answered?,
            Err(RecvTimeoutError::Disconnected) => return Err(gone),
            Err(RecvTimeoutError::Timeout) => {
                // The thread handles its requests in order, so it drops the
                // listener right after it registers it, if it ever does.
                let _ = commands.send(Command::Unwatch(id.to_owned()));
                self.late = Some(answer);
                return Err(BackendError::NotResponding);
            }
        }
        self.flags.insert(id.to_owned(), flag);
        Ok(())
    }

    /// Whether the listener thread is still in the registration given up
    /// last: its answer hasn't come yet.
    fn stuck(&mut self) -> bool {
        let still = self
            .late
            .as_ref()
            .is_some_and(|late| matches!(late.try_recv(), Err(TryRecvError::Empty)));
        if !still {
            self.late = None;
        }
        still
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

    /// The endpoint's mute and master volume.
    pub(super) fn read(&mut self, id: &str) -> Result<EndpointVolume, BackendError> {
        read(id).ok_or(BackendError::DeviceGone)
    }

    /// Sets the endpoint's master volume, 0 to 1: the fix the user clicked.
    pub(super) fn set_level(&mut self, id: &str, level: f32) -> Result<(), BackendError> {
        set_level(id, level)
    }

    /// Unmutes the endpoint: the fix the user clicked.
    pub(super) fn unmute(&mut self, id: &str) -> Result<(), BackendError> {
        unmute(id)
    }
}

/// The listener thread, started on first use.
#[derive(Default)]
pub(super) struct Volumes {
    watcher: Option<VolumeWatcher>,
}

impl Volumes {
    /// The watcher, when its thread runs.
    pub(super) fn running(&mut self) -> Option<&mut VolumeWatcher> {
        self.watcher.as_mut()
    }

    /// The watcher, its thread started now when it doesn't run yet.
    pub(super) fn watcher(&mut self) -> Result<&mut VolumeWatcher, BackendError> {
        self.watcher_with(READY_TIMEOUT, listen)
    }

    /// [`Volumes::watcher`], whose thread runs `body` and is waited for
    /// `timeout` at most.
    fn watcher_with(
        &mut self,
        timeout: Duration,
        body: impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static,
    ) -> Result<&mut VolumeWatcher, BackendError> {
        let watcher = match self.watcher.take() {
            Some(watcher) => watcher,
            None => VolumeWatcher::start_with(timeout, body)?,
        };
        Ok(self.watcher.insert(watcher))
    }
}

impl Drop for VolumeWatcher {
    fn drop(&mut self) {
        // The thread ends, unregistering every listener, once the channel
        // closes. One still in a Windows call is left to do that once the
        // call returns: waiting for it would hold AudioSync's exit.
        self.commands = None;
        let stuck = self.stuck();
        if let Some(thread) = self.thread.take()
            && !stuck
        {
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
    serve(commands, &Windows(enumerator));
}

/// Makes the calls asked for on `endpoints`, registering listeners and
/// dropping them on request, until the watcher goes; then drops the rest,
/// which unregisters them.
fn serve<E: Endpoints>(commands: &mpsc::Receiver<Command>, endpoints: &E) {
    let mut listeners: BTreeMap<String, E::Listener> = BTreeMap::new();
    while let Ok(command) = commands.recv() {
        match command {
            Command::Watch { id, flag, reply } => {
                let answer = endpoints.listen(&id, flag).map(|listener| {
                    listeners.insert(id, listener);
                });
                let _ = reply.send(answer);
            }
            Command::Unwatch(id) => {
                listeners.remove(&id);
            }
            Command::Read { id, reply } => {
                let _ = reply.send(endpoints.read(&id));
            }
            Command::SetLevel { id, level, reply } => {
                let _ = reply.send(endpoints.set_level(&id, level));
            }
            Command::Unmute { id, reply } => {
                let _ = reply.send(endpoints.unmute(&id));
            }
        }
    }
    drop(listeners);
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
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
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

    /// What every read of the fake endpoints answers.
    const FAKE_VOLUME: EndpointVolume = EndpointVolume {
        muted: false,
        level: 0.5,
    };

    /// Fake endpoints whose every call waits for a word on `gate`, as a
    /// Windows call that hangs until Windows audio answers, and then lists
    /// what it did in `done`.
    struct Hanging {
        gate: mpsc::Receiver<()>,
        done: Done,
    }

    impl Hanging {
        fn call(&self, entry: String) -> Result<(), BackendError> {
            self.gate.recv().map_err(|_| BackendError::NotResponding)?;
            self.done.push(entry);
            Ok(())
        }
    }

    impl Endpoints for Hanging {
        type Listener = FakeRegistration;

        fn listen(
            &self,
            id: &str,
            _flag: Arc<AtomicBool>,
        ) -> Result<FakeRegistration, BackendError> {
            self.call(format!("+{id}"))?;
            Ok(FakeRegistration {
                id: id.to_owned(),
                done: self.done.clone(),
            })
        }

        fn read(&self, id: &str) -> Result<EndpointVolume, BackendError> {
            self.call(format!("read {id}"))?;
            Ok(FAKE_VOLUME)
        }

        fn set_level(&self, id: &str, level: f32) -> Result<(), BackendError> {
            self.call(format!("level {id} {level}"))
        }

        fn unmute(&self, id: &str) -> Result<(), BackendError> {
            self.call(format!("unmute {id}"))
        }
    }

    /// A listener thread that starts at once and whose every call waits for
    /// a word on `gate` ([`Hanging`]).
    fn hanging(
        gate: mpsc::Receiver<()>,
        done: Done,
    ) -> impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static {
        move |commands, ready| {
            if ready.ok(()) {
                serve(commands, &Hanging { gate, done });
            }
        }
    }

    /// A listener thread whose start waits for a word on `gate`, as Windows
    /// audio that hangs while the thread sets up, and that serves no call.
    fn hanging_start(
        gate: mpsc::Receiver<()>,
    ) -> impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static {
        move |commands, ready| {
            let _ = gate.recv();
            if ready.ok(()) {
                while commands.recv().is_ok() {}
            }
        }
    }

    /// A listener thread that starts at once and serves no call.
    fn idle() -> impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static {
        |commands, ready| {
            if ready.ok(()) {
                while commands.recv().is_ok() {}
            }
        }
    }

    /// Waits up to [`LIMIT`] for the watcher to see the answer it gave up
    /// on.
    fn wait_until_answered(watcher: &mut VolumeWatcher) {
        let until = Instant::now() + LIMIT;
        while watcher.stuck() && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!watcher.stuck(), "the late answer came");
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
    fn a_volume_listener_that_doesnt_answer_in_time_is_given_up_and_its_late_registration_undone() {
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

    #[test]
    fn a_volume_read_or_fix_that_doesnt_answer_in_time_is_given_up_without_freezing_the_caller() {
        let (gate, hold) = mpsc::channel();
        let done = Done::default();
        let watcher =
            VolumeWatcher::start_with(TIMEOUT, hanging(hold, done.clone())).expect("it starts");
        // Windows audio hangs in a read the UI thread asked for: the call
        // returns after the wait, as no device volume for now.
        let (mut watcher, read) = within(move || {
            let mut watcher = watcher;
            let read = watcher.read("speakers");
            (watcher, read)
        });
        assert_eq!(read, Err(BackendError::NotResponding));
        // While it still hangs, the fix's writes and every other call fail
        // at once, with no second wait.
        let asked = Instant::now();
        assert_eq!(
            watcher.set_level("speakers", 0.75),
            Err(BackendError::NotResponding)
        );
        assert_eq!(watcher.unmute("speakers"), Err(BackendError::NotResponding));
        assert_eq!(watcher.read("headphones"), Err(BackendError::NotResponding));
        assert_eq!(
            watcher.watch("headphones"),
            Err(BackendError::NotResponding)
        );
        assert!(asked.elapsed() < TIMEOUT, "{:?}", asked.elapsed());
        // It answers after all; then every call goes to Windows again, on
        // the listener thread.
        gate.send(()).expect("the thread waits");
        done.wait_for(&["read speakers"]);
        wait_until_answered(&mut watcher);
        for _ in 0..3 {
            gate.send(()).expect("the thread waits");
        }
        assert_eq!(watcher.read("headphones"), Ok(FAKE_VOLUME));
        assert_eq!(watcher.set_level("headphones", 0.75), Ok(()));
        assert_eq!(watcher.unmute("headphones"), Ok(()));
        done.wait_for(&[
            "read speakers",
            "read headphones",
            "level headphones 0.75",
            "unmute headphones",
        ]);

        // A write that hangs is given up the same way.
        let (watcher, written) = within(move || {
            let mut watcher = watcher;
            let written = watcher.set_level("speakers", 0.75);
            (watcher, written)
        });
        assert_eq!(written, Err(BackendError::NotResponding));
        within(move || drop(watcher));
    }

    #[test]
    fn a_volume_thread_that_doesnt_start_in_time_holds_no_later_start() {
        let (gate, hold) = mpsc::channel();
        // Windows audio hangs while the listener thread sets up: given up
        // after the wait.
        let (mut volumes, started) = within(move || {
            let mut volumes = Volumes::default();
            let started = volumes
                .watcher_with(TIMEOUT, hanging_start(hold))
                .map(|_| ());
            (volumes, started)
        });
        assert_eq!(started, Err(BackendError::NotResponding));
        // While that start still hangs, the next output's call fails at
        // once, without a thread of its own.
        let asked = Instant::now();
        let tried = Arc::new(AtomicBool::new(false));
        let also = Arc::clone(&tried);
        let again = volumes
            .watcher_with(TIMEOUT, move |_, ready| {
                also.store(true, Ordering::Release);
                let _ = ready.ok(());
            })
            .map(|_| ());
        assert_eq!(again, Err(BackendError::NotResponding));
        assert!(asked.elapsed() < TIMEOUT, "{:?}", asked.elapsed());
        assert!(!tried.load(Ordering::Acquire), "no second start");
        // Windows audio answers: the late thread ends by itself, and the
        // next start works.
        gate.send(()).expect("the thread waits");
        let until = Instant::now() + LIMIT;
        loop {
            match volumes.watcher_with(TIMEOUT, idle()).map(|_| ()) {
                Ok(()) => break,
                Err(BackendError::NotResponding) if Instant::now() < until => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("the next start works: {error:?}"),
            }
        }
        assert!(volumes.running().is_some());
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
