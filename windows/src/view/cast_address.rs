//! Add by Address…: the dialog that lists a Google Cast speaker by its
//! address when discovery can't find it (spec C18). An address that is
//! turned down keeps it open with the reason; it closes once the speaker is
//! added, or on Cancel, Escape or the close button.

use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use slint::{ComponentHandle, SharedString, TimerMode};

use super::{App, with_app};
use crate::ui_model::CastAddressError;

/// How often the UI looks for the answer of an address check.
const CHECK_TICK: Duration = Duration::from_millis(50);

/// An address check running on a thread of its own, which takes up to 3 s.
/// Dropping it drops the answer.
pub(super) struct PendingCheck {
    text: String,
    answer: Receiver<bool>,
}

impl App {
    pub(super) fn connect_cast_address_dialog(self: &Rc<Self>) {
        self.follow_frame(
            |app| app.cast_address_dialog.window(),
            "cast address dialog",
        );
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.cast_address_dialog.on_confirmed(move |text| {
            with_app(&w, |app| app.confirm_cast_address(&text, &w));
        });
        let w = weak.clone();
        self.cast_address_dialog
            .on_cancelled(move || with_app(&w, App::close_cast_address));
        let w = weak;
        self.cast_address_dialog
            .window()
            .on_close_requested(move || {
                with_app(&w, App::close_cast_address);
                slint::CloseRequestResponse::HideWindow
            });
    }

    /// Speakers > Add by Address…, or the link under the note that no
    /// Google Cast speaker was found.
    pub(super) fn ask_cast_address(&self) {
        let dialog = &self.cast_address_dialog;
        if self.cast_check.borrow().is_some() {
            crate::platform::bring_to_front(dialog.window());
            return;
        }
        dialog.set_address(SharedString::new());
        dialog.set_error(SharedString::new());
        if let Err(error) = dialog.show() {
            self.show_window_error(&error);
            return;
        }
        dialog.invoke_prepare();
        crate::platform::bring_to_front(dialog.window());
        self.log.log(|| "cast address dialog: opened".to_owned());
    }

    /// Add or Enter: an address the backend takes is checked off the UI
    /// thread, and listed once something answers there.
    fn confirm_cast_address(&self, text: &str, weak: &Weak<App>) {
        if self.cast_check.borrow().is_some() {
            return;
        }
        let check = match self.backend.borrow_mut().check_cast_address(text) {
            Ok(check) => check,
            Err(error) => {
                self.turn_down_cast_address(error);
                return;
            }
        };
        let (sender, answer) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("cast-address-check".to_owned())
            .spawn(move || {
                let _ = sender.send(check.answers());
            });
        if spawned.is_err() {
            self.turn_down_cast_address(CastAddressError::NotFound);
            return;
        }
        *self.cast_check.borrow_mut() = Some(PendingCheck {
            text: text.to_owned(),
            answer,
        });
        let dialog = &self.cast_address_dialog;
        dialog.set_error(SharedString::new());
        dialog.set_checking(true);
        let weak = weak.clone();
        self.cast_check_timer
            .start(TimerMode::Repeated, CHECK_TICK, move || {
                with_app(&weak, |app| {
                    app.finish_cast_check();
                });
            });
    }

    /// Lists the speaker once its check found something, or says nothing
    /// answered. Returns whether the check is over (or none was running).
    pub(super) fn finish_cast_check(&self) -> bool {
        let answered = {
            let pending = self.cast_check.borrow();
            let Some(pending) = pending.as_ref() else {
                return true;
            };
            match pending.answer.try_recv() {
                Ok(answered) => answered,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => false,
            }
        };
        let Some(pending) = self.cast_check.take() else {
            return true;
        };
        self.cast_check_timer.stop();
        self.cast_address_dialog.set_checking(false);
        if !answered {
            self.turn_down_cast_address(CastAddressError::NotFound);
            return true;
        }
        let mut result = Ok(());
        self.update(|b| result = b.add_cast_address(&pending.text));
        match result {
            Ok(()) => self.close_cast_address(),
            Err(error) => self.turn_down_cast_address(error),
        }
        true
    }

    fn turn_down_cast_address(&self, error: CastAddressError) {
        let strings = self.strings();
        let reason = match error {
            CastAddressError::Invalid => strings.get_cast_address_invalid(),
            CastAddressError::NotFound => strings.get_cast_address_not_found(),
            CastAddressError::Full => strings.get_cast_address_full(),
        };
        let dialog = &self.cast_address_dialog;
        dialog.set_error(reason);
        dialog.set_refusals(dialog.get_refusals().wrapping_add(1));
        dialog.invoke_prepare();
    }

    /// Closes the dialog; a check still running is let go, and its answer
    /// lists nothing.
    fn close_cast_address(&self) {
        self.cast_check.take();
        self.cast_check_timer.stop();
        let dialog = &self.cast_address_dialog;
        let _ = dialog.hide();
        dialog.set_checking(false);
        dialog.set_address(SharedString::new());
        dialog.set_error(SharedString::new());
    }
}
