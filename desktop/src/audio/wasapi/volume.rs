//! Each output's own volume and mute in Windows (`IAudioEndpointVolume`,
//! spec S194, S195, S197): reading them, following their changes, and the
//! fix the user clicks.
//!
//! Every one of these calls can hang while Windows audio does (still waking
//! up, say), and the thread that asks for them is the UI thread, so a thread
//! of their own (in the multithreaded apartment) makes them, and the UI
//! thread waits for each answer as long as an output thread's start at most
//! ([`VolumeWatcher`]). Reads and writes activate the endpoint's volume
//! control afresh there. Change listeners need a home that outlives a call,
//! so the same thread registers and unregisters them on request and keeps
//! them until asked to drop them or until the watcher goes. Windows calls a
//! listener on a system thread; it only raises the endpoint's flag, which
//! [`VolumeWatcher::take_changed`] collects on the thread that polls, so
//! several notifications in between count once. Nothing here runs on an
//! audio thread.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::AUDIO_VOLUME_NOTIFICATION_DATA;
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::core::HSTRING;
use windows_core::implement;

use super::com::{
    Apartment, Awaited, READY_TIMEOUT, Ready, Running, StuckThreads, WINDOWS_AUDIO, Worker,
    awaited, device_error, spawn_worker_within,
};
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

/// What the listener thread is asked to do.
enum Command {
    /// Register a listener for the endpoint that raises `flag`, and answer.
    Watch {
        id: String,
        flag: Arc<AtomicBool>,
        reply: Reply<()>,
    },
    /// Unregister the endpoint's listener.
    Unwatch(String),
    /// Read the endpoint's mute and master volume, and answer.
    Read {
        id: String,
        reply: Reply<EndpointVolume>,
    },
    /// Set the endpoint's master volume, 0 to 1, and answer.
    SetLevel {
        id: String,
        level: f32,
        reply: Reply<()>,
    },
    /// Unmute the endpoint, and answer.
    Unmute { id: String, reply: Reply<()> },
}

/// Where the listener thread answers a call. The call counts as returned
/// once this goes, answered or not, which ends its stuck count if the caller
/// gave it up.
struct Reply<T> {
    sender: mpsc::Sender<Result<T, BackendError>>,
    _call: Running,
}

impl<T> Reply<T> {
    fn send(
        &self,
        answer: Result<T, BackendError>,
    ) -> Result<(), mpsc::SendError<Result<T, BackendError>>> {
        self.sender.send(answer)
    }
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

/// Reads, follows and sets the volume of the endpoints it is asked to, all
/// through a thread of its own, which also holds the listeners. Dropping it
/// unregisters them all.
///
/// Each call waits for the thread's answer as long as an output thread's
/// start at most ([`READY_TIMEOUT`]). One that doesn't come in time is given
/// up ([`BackendError::NotResponding`]): a read shows no device volume for
/// now, a write may still happen, and a listener registered late is dropped
/// as soon as its call returns. Until that call returns the thread is stuck,
/// so further calls fail at once instead of each waiting again. A call given
/// up counts as stuck ([`WINDOWS_AUDIO`]) until it returns: Windows audio
/// hangs, and the other audio threads' drops wait only a grace meanwhile.
///
/// Dropping it waits for the thread to end as a [`Worker`] does:
/// unregistering a listener answers nothing, and can hang too.
pub(super) struct VolumeWatcher {
    commands: Option<mpsc::Sender<Command>>,
    /// The flag of each endpoint followed.
    flags: BTreeMap<String, Arc<AtomicBool>>,
    /// How long an answer is waited for.
    answer_timeout: Duration,
    /// Where a call given up is counted.
    stuck: &'static StuckThreads,
    /// The call given up last, until it returns.
    late: Option<Awaited>,
    thread: Worker,
}

impl VolumeWatcher {
    /// The watcher whose listener thread runs `body`, waiting
    /// `answer_timeout` at most for the thread's start and for each answer,
    /// and counting the thread or a call given up in `stuck`.
    fn start_with(
        answer_timeout: Duration,
        stuck: &'static StuckThreads,
        body: impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static,
    ) -> Result<VolumeWatcher, BackendError> {
        let (commands, received) = mpsc::channel();
        let (thread, ()) =
            spawn_worker_within("audiosync-volumes", answer_timeout, stuck, move |ready| {
                body(&received, ready);
            })?;
        Ok(VolumeWatcher {
            commands: Some(commands),
            flags: BTreeMap::new(),
            answer_timeout,
            stuck,
            late: None,
            thread,
        })
    }

    /// Sends the thread the request `request` makes with its reply sender,
    /// and waits for the answer, for the answer timeout at most. Returns
    /// [`BackendError::NotResponding`] when the answer doesn't come in time,
    /// or at once while the thread is still in a call given up before.
    fn ask<T: Send + 'static>(
        &mut self,
        request: impl FnOnce(Reply<T>) -> Command,
    ) -> Result<T, BackendError> {
        if self.stuck() {
            return Err(BackendError::NotResponding);
        }
        let gone = BackendError::System {
            call: "the volume listener thread",
            code: E_FAIL.0,
        };
        let commands = self.commands.as_ref().ok_or_else(|| gone.clone())?;
        let (sender, answer) = mpsc::channel();
        let (call, end) = awaited(self.stuck);
        let reply = Reply {
            sender,
            _call: call,
        };
        commands.send(request(reply)).map_err(|_| gone.clone())?;
        match answer.recv_timeout(self.answer_timeout) {
            Ok(answered) => answered,
            Err(RecvTimeoutError::Disconnected) => Err(gone),
            Err(RecvTimeoutError::Timeout) => {
                if end.let_go() {
                    self.late = Some(end);
                }
                Err(BackendError::NotResponding)
            }
        }
    }

    /// Whether the listener thread is still in the call given up last: its
    /// answer hasn't come yet.
    fn stuck(&mut self) -> bool {
        let still = self.late.as_ref().is_some_and(Awaited::pending);
        if !still {
            self.late = None;
        }
        still
    }

    /// Registers a change listener for the endpoint `id` ([`Self::ask`]);
    /// the endpoint isn't followed when that fails.
    pub(super) fn watch(&mut self, id: &str) -> Result<(), BackendError> {
        self.unwatch(id);
        let flag = Arc::new(AtomicBool::new(false));
        let watched = self.ask(|reply| Command::Watch {
            id: id.to_owned(),
            flag: Arc::clone(&flag),
            reply,
        });
        if watched == Err(BackendError::NotResponding)
            && let Some(commands) = &self.commands
        {
            // The thread handles its requests in order, so it drops the
            // listener right after it registers it, if it ever does; with
            // none registered, this drops nothing.
            let _ = commands.send(Command::Unwatch(id.to_owned()));
        }
        watched?;
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

    /// The endpoint's mute and master volume, the Sound settings slider
    /// ([`Self::ask`]): an error when its volume control can't be activated
    /// or read, and both parts are unknown then (S194).
    pub(super) fn read(&mut self, id: &str) -> Result<EndpointVolume, BackendError> {
        self.ask(|reply| Command::Read {
            id: id.to_owned(),
            reply,
        })
    }

    /// Sets the endpoint's master volume, 0 to 1 ([`Self::ask`]): the fix
    /// the user clicked.
    pub(super) fn set_level(&mut self, id: &str, level: f32) -> Result<(), BackendError> {
        self.ask(|reply| Command::SetLevel {
            id: id.to_owned(),
            level,
            reply,
        })
    }

    /// Unmutes the endpoint ([`Self::ask`]): the fix the user clicked.
    pub(super) fn unmute(&mut self, id: &str) -> Result<(), BackendError> {
        self.ask(|reply| Command::Unmute {
            id: id.to_owned(),
            reply,
        })
    }
}

/// The listener thread, started on first use and kept until AudioSync quits.
///
/// Its start makes Windows calls too, which can hang, and is waited for as
/// long as an answer. A start that doesn't answer in time is given up, and
/// that thread ends by itself once its call returns ([`Ready::ok`]); until
/// then further starts fail at once instead of each waiting again, so the
/// outputs after the first don't each hold the UI thread.
#[derive(Default)]
pub(super) struct Volumes {
    watcher: Option<VolumeWatcher>,
    /// The start given up last, until its thread ends.
    late_start: Option<Awaited>,
}

impl Volumes {
    /// The watcher, when its thread runs.
    pub(super) fn running(&mut self) -> Option<&mut VolumeWatcher> {
        self.watcher.as_mut()
    }

    /// The watcher, its thread started now when it doesn't run yet.
    pub(super) fn watcher(&mut self) -> Result<&mut VolumeWatcher, BackendError> {
        self.watcher_with(READY_TIMEOUT, &WINDOWS_AUDIO, listen)
    }

    /// [`Volumes::watcher`], whose thread runs `body`, is waited for
    /// `timeout` at most and counts in `stuck` when let go.
    /// [`BackendError::NotResponding`] when the start doesn't answer in time,
    /// or at once while a start given up before still hangs.
    fn watcher_with(
        &mut self,
        timeout: Duration,
        stuck: &'static StuckThreads,
        body: impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static,
    ) -> Result<&mut VolumeWatcher, BackendError> {
        let watcher = match self.watcher.take() {
            Some(watcher) => watcher,
            None => {
                if self.late_start.as_ref().is_some_and(Awaited::pending) {
                    return Err(BackendError::NotResponding);
                }
                // Only looked at here: the thread itself counts as stuck
                // once its start is given up.
                let (running, start) = awaited(stuck);
                let started = VolumeWatcher::start_with(timeout, stuck, move |commands, ready| {
                    // Goes when the thread ends, whichever way it does.
                    let _running = running;
                    body(commands, ready);
                });
                // A thread that couldn't start has ended already; one that
                // didn't answer in time ends once its call returns.
                self.late_start = started.is_err().then_some(start);
                started?
            }
        };
        Ok(self.watcher.insert(watcher))
    }
}

impl Drop for VolumeWatcher {
    fn drop(&mut self) {
        // The thread ends, unregistering every listener, once the channel
        // closes. One still in a call given up before isn't waited for at
        // all; any other is waited for as a Worker is, since unregistering
        // can hang too (the requests queued before this one included). A
        // thread that doesn't end in time is left to finish once its calls
        // return: waiting for it would hold AudioSync's exit.
        self.commands = None;
        if self.stuck() {
            self.thread.let_go();
        } else {
            self.thread.finish();
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

    use super::super::com::STUCK_GRACE;
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

    /// Waits for a word on `gate`, as a Windows call that hangs until
    /// Windows audio answers; an error once the test sends no more.
    fn wait_on(gate: &Mutex<mpsc::Receiver<()>>) -> Result<(), BackendError> {
        gate.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv()
            .map_err(|_| BackendError::NotResponding)
    }

    /// A fake listener, dropped as the real one unregisters: at once, or,
    /// with a `gate`, once a word comes on it, as an unregistration that
    /// hangs until Windows audio answers.
    struct FakeRegistration {
        id: String,
        done: Done,
        gate: Option<Arc<Mutex<mpsc::Receiver<()>>>>,
    }

    impl Drop for FakeRegistration {
        fn drop(&mut self) {
            if let Some(gate) = &self.gate {
                let _ = wait_on(gate);
            }
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
    /// what it did in `done`; with `unregistering_hangs`, dropping one of
    /// their listeners waits the same way.
    struct Hanging {
        gate: Arc<Mutex<mpsc::Receiver<()>>>,
        done: Done,
        unregistering_hangs: bool,
    }

    impl Hanging {
        fn call(&self, entry: String) -> Result<(), BackendError> {
            wait_on(&self.gate)?;
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
                gate: self.unregistering_hangs.then(|| Arc::clone(&self.gate)),
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
        serving(Hanging {
            gate: Arc::new(Mutex::new(gate)),
            done,
            unregistering_hangs: false,
        })
    }

    /// [`hanging`], whose listeners' unregistration waits for a word on
    /// `gate` too.
    fn hanging_unregistration(
        gate: mpsc::Receiver<()>,
        done: Done,
    ) -> impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static {
        serving(Hanging {
            gate: Arc::new(Mutex::new(gate)),
            done,
            unregistering_hangs: true,
        })
    }

    /// A listener thread that starts at once and makes its calls on
    /// `endpoints`.
    fn serving(
        endpoints: Hanging,
    ) -> impl FnOnce(&mpsc::Receiver<Command>, Ready) + Send + 'static {
        move |commands, ready| {
            if ready.ok(()) {
                serve(commands, &endpoints);
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

    /// Reads this PC's default output through its volume control and checks
    /// the result against the double-audio snapshot, which reads the same
    /// two values. CI runners have no audio endpoint, so it is opt-in; on a
    /// PC run `cargo test -- --ignored --exact
    /// audio::wasapi::volume::tests::reads_the_default_outputs_volume_and_mute`.
    // covers: S194
    #[test]
    #[ignore = "reads this PC's default output; CI runners have none"]
    fn reads_the_default_outputs_volume_and_mute() {
        let _apartment = Apartment::enter().expect("COM initializes");
        let snapshot = super::super::routing::endpoint_audio().expect("the outputs are listed");
        let default = snapshot
            .iter()
            .find(|endpoint| endpoint.is_default)
            .expect("this PC has a default output");
        let enumerator = super::super::devices::enumerator().expect("an enumerator");
        let read = Windows(enumerator)
            .read(&default.id)
            .expect("the default output's volume control can be read");
        println!("{:?}: {read:?}", default.name);
        assert!((0.0..=1.0).contains(&read.level), "{read:?}");
        assert_eq!(Some(read), default.volume);
    }

    #[test]
    fn a_volume_listener_that_doesnt_answer_in_time_is_given_up_and_its_late_registration_undone() {
        static STUCK: StuckThreads = StuckThreads::new();
        let (gate, hold) = mpsc::channel();
        let done = Done::default();
        let watcher = VolumeWatcher::start_with(TIMEOUT, &STUCK, hanging(hold, done.clone()))
            .expect("it starts");
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
        static STUCK: StuckThreads = StuckThreads::new();
        let (gate, hold) = mpsc::channel();
        let done = Done::default();
        let watcher = VolumeWatcher::start_with(TIMEOUT, &STUCK, hanging(hold, done.clone()))
            .expect("it starts");
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
    fn a_volume_thread_that_hangs_unregistering_a_listener_doesnt_hold_quitting() {
        static STUCK: StuckThreads = StuckThreads::new();
        let (gate, hold) = mpsc::channel();
        let done = Done::default();
        let mut watcher =
            VolumeWatcher::start_with(TIMEOUT, &STUCK, hanging_unregistration(hold, done.clone()))
                .expect("it starts");
        gate.send(()).expect("the thread waits");
        assert_eq!(watcher.watch("speakers"), Ok(()));
        // A read hangs and is given up, then answers after all: the thread
        // is free again.
        let (mut watcher, read) = within(move || {
            let mut watcher = watcher;
            let read = watcher.read("speakers");
            (watcher, read)
        });
        assert_eq!(read, Err(BackendError::NotResponding));
        gate.send(()).expect("the thread waits");
        wait_until_answered(&mut watcher);
        done.wait_for(&["+speakers", "read speakers"]);
        // Windows audio hangs again as the outputs stop following their
        // volumes and AudioSync quits: the unregistration, which answers
        // nothing, holds the thread, and the watcher goes after a bounded
        // wait instead of waiting for it.
        watcher.unwatch("speakers");
        within(move || drop(watcher));
        // Once Windows audio answers, the thread still unregisters.
        gate.send(()).expect("the thread waits");
        done.wait_for(&["+speakers", "read speakers", "-speakers"]);

        // The same when the listener is only dropped as the thread ends.
        let (gate, hold) = mpsc::channel();
        let done = Done::default();
        let mut watcher =
            VolumeWatcher::start_with(TIMEOUT, &STUCK, hanging_unregistration(hold, done.clone()))
                .expect("it starts");
        gate.send(()).expect("the thread waits");
        assert_eq!(watcher.watch("headphones"), Ok(()));
        within(move || drop(watcher));
        gate.send(()).expect("the thread waits");
        done.wait_for(&["+headphones", "-headphones"]);
    }

    /// A thread like the capture's, whose answers are waited for `timeout`
    /// at most, and which hangs stopping its stream until `hung` closes.
    fn hanging_capture(
        stuck: &'static StuckThreads,
        timeout: Duration,
        hung: mpsc::Receiver<()>,
    ) -> Worker {
        let (worker, ()) = spawn_worker_within("audiosync-test", timeout, stuck, move |ready| {
            if ready.ok(()) {
                let _ = hung.recv();
            }
        })
        .expect("it starts");
        worker
    }

    #[test]
    fn a_volume_call_given_up_counts_as_stuck_until_it_returns() {
        static STUCK: StuckThreads = StuckThreads::new();
        let (gate, hold) = mpsc::channel();
        let done = Done::default();
        let watcher = VolumeWatcher::start_with(TIMEOUT, &STUCK, hanging(hold, done.clone()))
            .expect("it starts");
        let (watcher, read) = within(move || {
            let mut watcher = watcher;
            let read = watcher.read("speakers");
            (watcher, read)
        });
        assert_eq!(read, Err(BackendError::NotResponding));
        // Windows audio hangs: the other audio threads' drops wait only a
        // grace from now on.
        assert!(STUCK.any(), "the call counts as stuck");

        // AudioSync quits meanwhile: the capture, which hangs stopping its
        // stream in the same Windows audio, is waited for only a grace,
        // however long its own answer timeout, and the volume thread, still
        // in the call given up, not at all.
        let (capture_answer, capture_hung) = mpsc::channel();
        let capture = hanging_capture(&STUCK, LIMIT * 2, capture_hung);
        let quitting = Instant::now();
        drop(capture);
        drop(watcher);
        let waited = quitting.elapsed();
        assert!(waited < STUCK_GRACE * 4, "quitting waited {waited:?}");

        // Windows audio answers: the call returns, and every thread ends.
        gate.send(()).expect("the thread waits");
        done.wait_for(&["read speakers"]);
        drop(capture_answer);
        assert!(STUCK.none_within(LIMIT), "nothing is stuck anymore");
    }

    #[test]
    fn while_another_audio_thread_is_stuck_the_volume_thread_is_waited_for_only_a_grace() {
        static STUCK: StuckThreads = StuckThreads::new();
        // The capture hung stopping its stream and was let go.
        let (capture_answer, capture_hung) = mpsc::channel();
        drop(hanging_capture(&STUCK, TIMEOUT, capture_hung));
        assert!(STUCK.any(), "the capture counts as stuck");
        // The volume thread, whose answers are waited for long, hangs
        // unregistering a listener as AudioSync quits: waited for only a
        // grace.
        let (gate, hold) = mpsc::channel();
        let done = Done::default();
        let mut watcher = VolumeWatcher::start_with(
            LIMIT * 2,
            &STUCK,
            hanging_unregistration(hold, done.clone()),
        )
        .expect("it starts");
        gate.send(()).expect("the thread waits");
        assert_eq!(watcher.watch("speakers"), Ok(()));
        let quitting = Instant::now();
        drop(watcher);
        let waited = quitting.elapsed();
        assert!(
            waited >= STUCK_GRACE / 2 && waited < STUCK_GRACE * 4,
            "the drop waited {waited:?}"
        );
        // Windows audio answers: both end by themselves.
        gate.send(()).expect("the thread waits");
        done.wait_for(&["+speakers", "-speakers"]);
        drop(capture_answer);
        assert!(STUCK.none_within(LIMIT), "nothing is stuck anymore");
    }

    #[test]
    fn a_volume_thread_that_doesnt_start_in_time_holds_no_later_start() {
        static STUCK: StuckThreads = StuckThreads::new();
        let (gate, hold) = mpsc::channel();
        // Windows audio hangs while the listener thread sets up: given up
        // after the wait.
        let (mut volumes, started) = within(move || {
            let mut volumes = Volumes::default();
            let started = volumes
                .watcher_with(TIMEOUT, &STUCK, hanging_start(hold))
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
            .watcher_with(TIMEOUT, &STUCK, move |_, ready| {
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
            match volumes.watcher_with(TIMEOUT, &STUCK, idle()).map(|_| ()) {
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
        let mut volumes = Volumes::default();
        let watcher = volumes.watcher().expect("the listener thread starts");
        for endpoint in &listing.endpoints {
            let before = watcher.read(&endpoint.id);
            println!("{:?}: {before:?}", endpoint.name);
            if let Ok(volume) = before {
                assert!((0.0..=1.0).contains(&volume.level), "{volume:?}");
            }
            watcher.watch(&endpoint.id).expect("a listener registers");
            assert_eq!(watcher.read(&endpoint.id), before, "nothing changed");
        }
        println!("changed meanwhile: {:?}", watcher.take_changed());
        for endpoint in &listing.endpoints {
            watcher.unwatch(&endpoint.id);
        }
        drop(volumes);
    }
}
