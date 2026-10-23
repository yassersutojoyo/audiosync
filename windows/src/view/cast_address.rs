//! Add by Address…: the dialog that lists a Google Cast speaker by its
//! address when discovery can't find it (spec C18). An address that is
//! turned down keeps it open with the reason; it closes once the speaker is
//! added, or on Cancel, Escape or the close button.

use std::rc::Rc;

use slint::{ComponentHandle, SharedString};

use super::{App, with_app};
use crate::ui_model::CastAddressError;

impl App {
    pub(super) fn connect_cast_address_dialog(self: &Rc<Self>) {
        self.follow_frame(
            |app| app.cast_address_dialog.window(),
            "cast address dialog",
        );
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.cast_address_dialog
            .on_confirmed(move |text| with_app(&w, |app| app.confirm_cast_address(&text)));
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

    /// Add or Enter: the backend checks the address and lists the speaker.
    fn confirm_cast_address(&self, text: &str) {
        let mut result = Ok(());
        self.update(|b| result = b.add_cast_address(text));
        let Err(error) = result else {
            self.close_cast_address();
            return;
        };
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
        let _ = self.cast_address_dialog.hide();
        self.cast_address_dialog.set_address(SharedString::new());
        self.cast_address_dialog.set_error(SharedString::new());
        self.cast_address_dialog.set_refusals(0);
    }
}
