//! Settings… from the main window's gear (`ui/settings.slint`). It shows the
//! main window's settings and hands each change to the main window's own
//! callback, the one the old File, Speakers, View and Help menus called.

use std::rc::Rc;

use slint::ComponentHandle;

use super::{App, with_app};

impl App {
    pub(super) fn connect_settings(self: &Rc<Self>) {
        self.follow_frame(|app| app.settings.window(), "Settings");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.main
            .on_settings_requested(move || with_app(&w, App::show_settings));
        let s = &self.settings;
        let main = self.main.as_weak();
        macro_rules! forward {
            ($($on:ident => $invoke:ident($($arg:ident),*)),+ $(,)?) => {$(
                let m = main.clone();
                s.$on(move |$($arg),*| {
                    if let Some(main) = m.upgrade() {
                        main.$invoke($($arg),*);
                    }
                });
            )+};
        }
        forward!(
            on_appearance_chosen => invoke_appearance_chosen(choice),
            on_launch_at_login_toggled => invoke_launch_at_login_toggled(on),
            on_resume_after_sleep_toggled => invoke_resume_after_sleep_toggled(on),
            on_switch_source_on_quit_toggled => invoke_switch_source_on_quit_toggled(on),
            on_volume_keys_toggled => invoke_volume_keys_toggled(on),
            on_show_virtual_devices_toggled => invoke_show_virtual_devices_toggled(on),
            on_speaker_unhide_requested => invoke_speaker_unhide_requested(id),
            on_check_updates_automatically_toggled => invoke_check_updates_automatically_toggled(on),
            on_check_for_updates_requested => invoke_check_for_updates_requested(),
            on_setup_requested => invoke_setup_requested(),
        );
        let w = weak.clone();
        s.on_close_requested(move || with_app(&w, App::hide_settings));
        s.on_exit_requested(|| {
            let _ = slint::quit_event_loop();
        });
        let w = weak;
        s.window().on_close_requested(move || {
            with_app(&w, App::hide_settings);
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// Opens Settings, or brings it to the front when it is open.
    pub fn show_settings(&self) {
        self.show_settings_state();
        if let Err(error) = self.settings.show() {
            self.show_window_error(&error);
            return;
        }
        self.fit_settings_room();
        self.settings.invoke_take_focus();
        crate::platform::bring_to_front(self.settings.window());
    }

    /// Caps Settings at the work area of the monitor it is on, again each time
    /// it moves to another monitor or that monitor's scale changes.
    pub(super) fn fit_settings_room(&self) {
        let at = self.settings.window().position();
        if let Some((work_area, scale)) = crate::platform::monitor_at(at.x.into(), at.y.into()) {
            // The margin leaves room for the title bar and a gap above the taskbar.
            self.settings
                .set_room((work_area.height / scale - 80.0).max(200.0) as f32);
        }
    }

    fn hide_settings(&self) {
        // A window that fails to hide stays open; the user can still close it.
        let _ = self.settings.hide();
    }

    /// Copies the main window's settings, after each refresh.
    pub(super) fn show_settings_state(&self) {
        let (m, s) = (&self.main, &self.settings);
        s.set_appearance(m.get_appearance());
        s.set_launch_at_login(m.get_launch_at_login());
        s.set_resume_after_sleep(m.get_resume_after_sleep());
        s.set_switch_source_on_quit(m.get_switch_source_on_quit());
        s.set_volume_keys_available(m.get_volume_keys_available());
        s.set_volume_keys(m.get_volume_keys());
        s.set_show_virtual_devices(m.get_show_virtual_devices());
        s.set_hidden_speakers(m.get_hidden_speakers());
        s.set_updater_available(m.get_updater_available());
        s.set_check_updates_automatically(m.get_check_updates_automatically());
    }
}
