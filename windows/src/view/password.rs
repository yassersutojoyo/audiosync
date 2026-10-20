//! The password dialog of an AirPlay speaker protected by a password. It
//! opens when the snapshot asks for a password and closes when it no longer
//! does: once the password is typed, cancelled, or the speaker was switched
//! off. A speaker that turns the password down asks again, and the dialog
//! opens again saying so. The field is cleared whenever the dialog closes,
//! so the password stays only with the backend.

use std::rc::Rc;

use slint::{ComponentHandle, SharedString};

use super::{App, with_app};
use crate::ui_model::PasswordPrompt;

impl App {
    pub(super) fn connect_password_dialog(self: &Rc<Self>) {
        self.follow_frame(|app| app.password_dialog.window(), "password dialog");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.password_dialog
            .on_confirmed(move |password| with_app(&w, |app| app.confirm_password(&password)));
        let w = weak.clone();
        self.password_dialog
            .on_cancelled(move || with_app(&w, App::cancel_password));
        let w = weak;
        self.password_dialog.window().on_close_requested(move || {
            with_app(&w, App::cancel_password);
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// Opens, updates or closes the password dialog for the speaker that
    /// asks for its password.
    pub(super) fn show_password_prompt(&self, prompt: Option<&PasswordPrompt>) {
        let dialog = &self.password_dialog;
        let Some(prompt) = prompt else {
            if self.password_for.borrow_mut().take().is_some() {
                let _ = dialog.hide();
                dialog.set_password(SharedString::new());
                self.log.log(|| "password dialog: closed".to_owned());
            }
            return;
        };
        let opening = self.password_for.borrow().as_deref() != Some(prompt.speaker_id.as_str());
        let turned_down = prompt.wrong && !dialog.get_wrong();
        dialog.set_speaker(prompt.name.as_str().into());
        dialog.set_wrong(prompt.wrong);
        if opening {
            *self.password_for.borrow_mut() = Some(prompt.speaker_id.clone());
            dialog.set_password(SharedString::new());
            if let Err(error) = dialog.show() {
                self.show_window_error(&error);
                return;
            }
            dialog.invoke_prepare();
            crate::platform::bring_to_front(dialog.window());
            self.log
                .log(|| format!("password dialog: opened for {}", prompt.name));
        } else if turned_down {
            dialog.set_password(SharedString::new());
            dialog.invoke_prepare();
        }
    }

    /// Pair: the password goes to the backend, which closes the prompt.
    fn confirm_password(&self, password: &str) {
        if password.is_empty() {
            return;
        }
        let id = self.password_for.borrow().clone();
        self.password_dialog.set_password(SharedString::new());
        if let Some(id) = id {
            self.update(|b| b.submit_password(&id, password));
        }
    }

    /// Cancel, Escape or the close button: the speaker stays off.
    fn cancel_password(&self) {
        let id = self.password_for.borrow_mut().take();
        let _ = self.password_dialog.hide();
        self.password_dialog.set_password(SharedString::new());
        if let Some(id) = id {
            self.update(|b| b.cancel_password(&id));
        }
    }
}
