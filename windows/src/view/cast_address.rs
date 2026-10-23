//! Add by Address…: the dialog that lists a Google Cast speaker by its
//! address when discovery can't find it (spec C18). An address that is
//! turned down keeps it open with the reason; it closes once the speaker is
//! added, or on Cancel, Escape or the close button. The check of whether
//! something answers runs on its own thread, so the windows keep responding.

use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::Duration;

use slint::{ComponentHandle, SharedString, TimerMode};

use super::{App, with_app};
use crate::audio::cast_address::CastAddress;
use crate::ui_model::CastAddressError;

/// The checked address and whether something answered there.
pub(super) type CheckAnswer = (CastAddress, bool);

/// How often the dialog looks for the answer of a running check.
const CHECK_POLL: Duration = Duration::from_millis(50);

impl App {
    pub(super) fn connect_cast_address_dialog(self: &Rc<Self>) {
        self.follow_frame(
            |app| app.cast_address_dialog.window(),
            "cast address dialog",
        );
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.cast_address_dialog
            .on_confirmed(move |text| with_app(&w, |app| app.confirm_cast_address(&w, &text)));
        self.cast_address_dialog
            .on_blank(|text| text.trim().is_empty());
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
        dialog.set_address(SharedString::new());
        dialog.set_error(SharedString::new());
        dialog.set_refusals(0);
        if let Err(error) = dialog.show() {
            self.show_window_error(&error);
            return;
        }
        dialog.invoke_prepare();
        crate::platform::bring_to_front(dialog.window());
        self.log.log(|| "cast address dialog: opened".to_owned());
    }

    /// Add or Enter: the backend takes the address, a thread checks that
    /// something answers there, and the speaker is listed once it does.
    fn confirm_cast_address(&self, weak: &Weak<App>, text: &str) {
        if self.cast_check.borrow().is_some() {
            return;
        }
        let check = self.backend.borrow_mut().check_cast_address(text);
        let check = match check {
            Ok(check) => check,
            Err(error) => return self.turn_down_cast_address(error),
        };
        let (sender, receiver) = mpsc::channel();
        let started = thread::Builder::new()
            .name("cast address check".to_owned())
            .spawn(move || {
                let answered = check.run();
                // The dialog may have been cancelled: then nobody listens.
                let _ = sender.send((check.address, answered));
            });
        if started.is_err() {
            return self.turn_down_cast_address(CastAddressError::NotFound);
        }
        *self.cast_check.borrow_mut() = Some(receiver);
        self.cast_address_dialog.set_checking(true);
        let weak = weak.clone();
        self.cast_check_timer
            .start(TimerMode::Repeated, CHECK_POLL, move || {
                with_app(&weak, App::take_cast_check)
            });
    }

    /// Lists the speaker once the check answered, or says why not.
    pub(super) fn take_cast_check(&self) {
        let received = match self.cast_check.borrow().as_ref() {
            Some(receiver) => receiver.try_recv(),
            None => return,
        };
        let answer = match received {
            Ok(answer) => Some(answer),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        self.stop_cast_check();
        let mut result = Err(CastAddressError::NotFound);
        if let Some((address, answered)) = answer {
            self.update(|b| result = b.add_checked_cast_address(address, answered));
        }
        match result {
            Ok(()) => self.close_cast_address(),
            Err(error) => self.turn_down_cast_address(error),
        }
    }

    fn stop_cast_check(&self) {
        self.cast_check_timer.stop();
        self.cast_check.borrow_mut().take();
        self.cast_address_dialog.set_checking(false);
    }

    /// Keeps the dialog open with why `error` turned the address down.
    fn turn_down_cast_address(&self, error: CastAddressError) {
        let strings = self.strings();
        let reason = match error {
            CastAddressError::Invalid => strings.get_cast_address_invalid(),
            CastAddressError::NotFound => strings.get_cast_address_not_found(),
        };
        let dialog = &self.cast_address_dialog;
        dialog.set_error(reason);
        dialog.set_refusals(dialog.get_refusals() % 2 + 1);
        dialog.invoke_prepare();
    }

    fn close_cast_address(&self) {
        self.stop_cast_check();
        let _ = self.cast_address_dialog.hide();
        self.cast_address_dialog.set_address(SharedString::new());
        self.cast_address_dialog.set_error(SharedString::new());
        self.cast_address_dialog.set_refusals(0);
    }
}
