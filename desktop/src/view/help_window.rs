//! The AudioSync Guide (`ui/help.slint`): how to use the Windows app, opened
//! from the gear's AudioSync Guide (F1). There is one Guide window, made with the
//! App; opening it again brings that window to the front at the top. Its
//! Show How opens the double-audio notice's own guide, and shows only while
//! that notice does.

use std::rc::Rc;

use slint::ComponentHandle;

use super::{App, guide_place_to_show, with_app};
use crate::guide;
use crate::ui_model::{AppState, WindowPlace};

impl App {
    pub(super) fn connect_help(self: &Rc<Self>) {
        self.follow_frame(&self.help(), "AudioSync Guide");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.help()
            .on_show_how_requested(move || with_app(&w, App::show_help_double_audio));
        let w = weak.clone();
        self.help().on_close_requested(move || {
            with_app(&w, |app| {
                app.ui.borrow_mut().guide_place = app.guide_place();
                // A window that fails to hide stays open; the user can still close it.
                let _ = app.help().hide();
            })
        });
        let w = weak;
        self.help().window().on_close_requested(move || {
            with_app(&w, |app| {
                app.ui.borrow_mut().guide_place = app.guide_place();
                let _ = app.help().hide();
            });
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// Opens the Guide, or brings it to the front when it is open.
    pub fn show_help(&self) {
        let opening = !self.help.is_visible();
        if opening {
            self.place_guide();
        }
        if let Err(error) = self.help().show() {
            self.show_window_error(&error);
            return;
        }
        if opening {
            self.help().invoke_scroll_to_top();
            self.log.log(|| "AudioSync Guide: opened".to_owned());
        }
        self.help().invoke_take_focus();
        crate::platform::bring_to_front(self.help().window());
    }

    /// Where the Guide is while it shows, else where it was last.
    pub(super) fn guide_place(&self) -> Option<WindowPlace> {
        let Some(help) = self.help.built().filter(|help| help.window().is_visible()) else {
            return self.ui.borrow().guide_place;
        };
        let window = help.window();
        let position = window.position();
        Some(WindowPlace {
            x: position.x,
            y: position.y,
        })
    }

    /// Puts the Guide where it was last time, as the Mac does, back on a
    /// monitor when that place is off every one.
    fn place_guide(&self) {
        let Some(place) = self.ui.borrow().guide_place else {
            return;
        };
        let on_a_monitor = crate::platform::is_on_a_monitor(place.x, place.y);
        // The primary monitor holds the point (0, 0).
        let (x, y) = if on_a_monitor {
            (place.x, place.y)
        } else {
            (0, 0)
        };
        let monitor = crate::platform::monitor_at(f64::from(x), f64::from(y));
        let help = self.help();
        let window = help.window();
        let size = window.size().to_logical(window.scale_factor());
        // Before its first showing the window may have no size yet: the Guide's own.
        let size = if size.width > 0.0 {
            (f64::from(size.width), f64::from(size.height))
        } else {
            (640.0, 600.0)
        };
        if let Some(place) = guide_place_to_show(place, size, on_a_monitor, monitor) {
            window.set_position(slint::PhysicalPosition::new(place.x, place.y));
        }
    }

    /// Show How in the Guide: the guide of the double-audio notice showing now.
    fn show_help_double_audio(&self) {
        let id = guide::notice_with_guide(&self.snapshot().notices)
            .and_then(|id| i32::try_from(id).ok());
        if let Some(id) = id {
            self.show_guide(id);
        }
    }

    /// Shows Show How in the Guide only while it has a guide to open.
    pub(super) fn refresh_help(&self, state: &AppState) {
        let Some(help) = self.help.built() else {
            return;
        };
        help.set_show_how_shown(guide::notice_with_guide(&state.notices).is_some());
    }
}

#[cfg(test)]
mod tests {
    use slint::platform::Key;

    use super::super::tests::{app_over, draw_frames, hold_clock, leaked, let_notice_close, press};
    use crate::ui_model::{MockBackend, NoticeMessage, SourceName};

    fn heard_twice() -> NoticeMessage {
        NoticeMessage::HeardTwice {
            source: Some(SourceName::Named("Player".to_owned())),
            mute: vec!["TV".to_owned()],
            shared: Vec::new(),
            default_output: None,
        }
    }

    #[test]
    fn help_opens_one_guide_window_that_escape_closes() {
        let app = app_over(MockBackend::new());
        let window = leaked(app.help());
        assert!(!window.is_visible());
        app.main.invoke_help_requested();
        draw_frames();
        assert!(window.is_visible(), "AudioSync Guide opens it");
        assert!(!app.guide.is_visible(), "not the double-audio guide");
        // The one window, scrolled down, is brought to the front as it is.
        press(window, Key::PageDown);
        let scrolled = app.help().get_scrolled();
        app.main.invoke_help_requested();
        assert!(window.is_visible());
        assert_eq!(app.help().get_scrolled(), scrolled, "the same window");
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape closes it");

        app.main.invoke_help_requested();
        assert!(window.is_visible(), "it opens again after Escape");
        app.help().invoke_close_requested();
        assert!(!window.is_visible(), "Close closes it");
    }

    #[test]
    fn the_keyboard_scrolls_the_guide() {
        let app = app_over(MockBackend::new());
        let window = leaked(app.help());
        app.main.invoke_help_requested();
        draw_frames();
        let scrolled = || app.help().get_scrolled();
        assert_eq!(scrolled(), 0.0, "it opens at the top");
        press(window, Key::DownArrow);
        let line = scrolled();
        assert!(line > 0.0, "Down scrolls a line");
        press(window, Key::PageDown);
        let page = scrolled();
        assert!(page - line > line, "Page Down scrolls more than a line");
        press(window, Key::End);
        let bottom = scrolled();
        assert!(bottom >= page);
        press(window, Key::DownArrow);
        assert_eq!(scrolled(), bottom, "not past the end");
        press(window, Key::UpArrow);
        assert!(scrolled() < bottom, "Up scrolls back");
        press(window, Key::Home);
        assert_eq!(scrolled(), 0.0, "Home goes to the top");
        press(window, Key::PageUp);
        assert_eq!(scrolled(), 0.0, "not past the top");

        press(window, Key::End);
        press(window, Key::Escape);
        app.main.invoke_help_requested();
        assert_eq!(scrolled(), 0.0, "it opens at the top again");
    }

    #[test]
    fn show_how_in_the_guide_follows_the_double_audio_notice() {
        let mut backend = MockBackend::new();
        backend.post("app:player", heard_twice(), "09.41");
        let app = app_over(backend);
        app.main.invoke_help_requested();
        assert!(app.help().get_show_how_shown(), "the notice is showing");
        app.help().invoke_show_how_requested();
        assert!(
            app.guide.is_visible(),
            "Show How opens the double-audio guide"
        );
        assert!(app.help.is_visible(), "the Guide stays open");

        let id = app.snapshot().notices[0].id;
        hold_clock();
        app.main
            .invoke_notice_dismissed(i32::try_from(id).expect("a small ID"));
        let_notice_close();
        assert!(
            !app.help().get_show_how_shown(),
            "no notice, no Show How in the Guide"
        );
    }

    #[test]
    fn show_how_in_the_guide_opens_nothing_without_the_notice() {
        let app = app_over(MockBackend::new());
        assert!(app.snapshot().notices.is_empty());
        app.main.invoke_help_requested();
        assert!(!app.help().get_show_how_shown());
        app.help().invoke_show_how_requested();
        assert!(!app.guide.is_visible());
    }
}
