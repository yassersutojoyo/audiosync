//! The backend behind the windows. One that may move to another thread
//! starts streaming there, so the windows keep animating while the capture
//! and the outputs open.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::ui_model::{AppState, Backend, PowerControl};

enum Held {
    #[cfg_attr(
        not(any(test, feature = "mock-backend")),
        allow(dead_code, reason = "only the previews' mock backend stays here")
    )]
    Here(Box<dyn Backend>),
    Movable(Box<dyn Backend + Send>),
    /// Starting streaming on its own thread, which hands it back.
    Away(Receiver<Box<dyn Backend + Send>>),
}

pub struct Engine {
    held: RefCell<Held>,
    /// What the windows show while the backend is away.
    shown: RefCell<Option<AppState>>,
    /// The latest switch position asked for while the backend was away.
    asked: Cell<Option<(bool, PowerControl)>>,
}

fn present(held: &Held) -> &(dyn Backend + 'static) {
    match held {
        Held::Here(backend) => &**backend,
        Held::Movable(backend) => &**backend,
        Held::Away(_) => unreachable!("the backend came back"),
    }
}

fn present_mut(held: &mut Held) -> &mut (dyn Backend + 'static) {
    match held {
        Held::Here(backend) => &mut **backend,
        Held::Movable(backend) => &mut **backend,
        Held::Away(_) => unreachable!("the backend came back"),
    }
}

impl Engine {
    /// A backend that stays on the UI thread.
    #[cfg_attr(
        not(any(test, feature = "mock-backend")),
        allow(dead_code, reason = "only the previews' mock backend stays here")
    )]
    pub fn here(backend: Box<dyn Backend>) -> Engine {
        Engine::holding(Held::Here(backend))
    }

    pub fn movable(backend: Box<dyn Backend + Send>) -> Engine {
        Engine::holding(Held::Movable(backend))
    }

    fn holding(held: Held) -> Engine {
        Engine {
            held: RefCell::new(held),
            shown: RefCell::default(),
            asked: Cell::new(None),
        }
    }

    /// Waits for the backend to come back from a start.
    pub fn borrow(&self) -> Ref<'_, dyn Backend + 'static> {
        self.come_back(true);
        Ref::map(self.held.borrow(), present)
    }

    /// Waits for the backend to come back from a start.
    pub fn borrow_mut(&self) -> RefMut<'_, dyn Backend + 'static> {
        self.come_back(true);
        RefMut::map(self.held.borrow_mut(), present_mut)
    }

    /// The backend unless it is away starting, or already borrowed.
    pub fn try_borrow_mut(&self) -> Option<RefMut<'_, dyn Backend + 'static>> {
        if !self.come_back(false) {
            return None;
        }
        let held = self.held.try_borrow_mut().ok()?;
        Some(RefMut::map(held, present_mut))
    }

    /// Swaps in another backend, as a test's unplugged speaker does.
    #[cfg(test)]
    pub fn replace(&self, backend: Box<dyn Backend>) {
        self.come_back(true);
        *self.held.borrow_mut() = Held::Here(backend);
    }

    /// Whether the backend is here, not away starting.
    pub fn is_here(&self) -> bool {
        self.come_back(false)
    }

    /// The backend's state, or while it starts, the state it had with the
    /// switch where the user put it.
    pub fn snapshot(&self) -> AppState {
        if !self.come_back(false)
            && let Some(shown) = self.shown.borrow().as_ref()
        {
            return shown.clone();
        }
        self.borrow().snapshot()
    }

    /// Switches streaming on or off. A movable backend starts on its own
    /// thread; returns true then, and [`Engine::is_here`] tells when it is
    /// back. A switch while it is away waits for its return, and only the
    /// last one counts.
    pub fn set_running(&self, on: bool, by: PowerControl) -> bool {
        if !self.come_back(false) {
            self.asked.set(Some((on, by)));
            if let Some(shown) = self.shown.borrow_mut().as_mut() {
                shown.running = on;
            }
            return false;
        }
        let movable = matches!(*self.held.borrow(), Held::Movable(_));
        if !(on && movable) {
            self.borrow_mut().set_running_by(on, by);
            return false;
        }
        let (back, returned) = mpsc::channel();
        let Held::Movable(mut backend) = self.held.replace(Held::Away(returned)) else {
            unreachable!("checked above");
        };
        let mut shown = backend.snapshot();
        shown.running = true;
        *self.shown.borrow_mut() = Some(shown);
        std::thread::spawn(move || {
            backend.set_running(true);
            let _ = back.send(backend);
        });
        true
    }

    /// Takes the backend back from its start, waiting for it with `wait`,
    /// then applies a switch-off asked meanwhile. Whether it is here.
    fn come_back(&self, wait: bool) -> bool {
        let backend = {
            // Borrowed means here: an away backend is only borrowed in here.
            let Ok(held) = self.held.try_borrow() else {
                return true;
            };
            let Held::Away(back) = &*held else {
                return true;
            };
            let received = if wait {
                back.recv().map_err(|_| TryRecvError::Disconnected)
            } else {
                back.try_recv()
            };
            match received {
                Ok(backend) => backend,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => panic!("the streaming start panicked"),
            }
        };
        *self.held.borrow_mut() = Held::Movable(backend);
        self.shown.take();
        // Held, not away, during the stop: its COM calls can pump messages
        // whose handlers ask for the backend and must find it busy.
        if let Some((false, by)) = self.asked.take() {
            present_mut(&mut self.held.borrow_mut()).set_running_by(false, by);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Duration;

    use super::*;
    use crate::audio::backend::AudioBackend as _;

    thread_local! {
        static STOPPING: RefCell<Option<Rc<Engine>>> = const { RefCell::new(None) };
        static BUSY_WHILE_STOPPING: Cell<Option<bool>> = const { Cell::new(None) };
    }

    /// An output's stop can pump window messages on the UI thread, whose
    /// handlers ask the engine for the backend again.
    fn ask_for_the_backend() {
        STOPPING.with(|engine| {
            if let Some(engine) = engine.borrow().as_ref() {
                let here = engine.is_here();
                let busy = engine.try_borrow_mut().is_none();
                BUSY_WHILE_STOPPING.set(Some(here && busy));
            }
        });
    }

    #[test]
    fn a_switch_off_during_the_start_stops_while_the_windows_ask_for_the_backend() {
        let mut audio = crate::audio::mock::MockBackend::empty();
        audio.connect_output("speakers", "Speakers");
        audio.add_app(2000, "Spotify.exe");
        audio
            .set_output_enabled("speakers", true)
            .expect("switched on");
        audio.slow_capture_starts(Duration::from_millis(200));
        audio.on_output_stop(ask_for_the_backend);
        let adapter = crate::adapter::AudioAdapter::new(
            audio,
            || "09.41".to_owned(),
            crate::debug_log::DebugLog::default(),
        );
        let engine = Rc::new(Engine::movable(Box::new(adapter)));
        STOPPING.with(|stopping| *stopping.borrow_mut() = Some(Rc::clone(&engine)));
        assert!(engine.set_running(true, PowerControl::Switch));
        assert!(!engine.set_running(false, PowerControl::Switch));
        assert!(!engine.borrow().snapshot().running);
        STOPPING.with(|stopping| stopping.borrow_mut().take());
        assert_eq!(BUSY_WHILE_STOPPING.get(), Some(true));
    }
}
