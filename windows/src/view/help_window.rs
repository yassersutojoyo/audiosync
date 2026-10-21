//! The AudioSync Guide (`ui/help.slint`): how to use the Windows app, opened
//! from Help > AudioSync Guide (F1). There is one Guide window, made with the
//! App; opening it again brings that window to the front at the top. Its
//! Show How opens the double-audio notice's own guide, and shows only while
//! that notice does.

use std::rc::Rc;

use slint::ComponentHandle;

use super::{App, with_app};
use crate::guide;
use crate::ui_model::AppState;

impl App {
    pub(super) fn connect_help(self: &Rc<Self>) {
        self.follow_frame(|app| app.help.window(), "AudioSync Guide");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.main
            .on_help_requested(move || with_app(&w, App::show_help));
        let w = weak.clone();
        self.help
            .on_show_how_requested(move || with_app(&w, App::show_help_double_audio));
        let w = weak.clone();
        self.help.on_close_requested(move || {
            with_app(&w, |app| {
                // A window that fails to hide stays open; the user can still close it.
                let _ = app.help.hide();
            })
        });
        let w = weak;
        self.help.window().on_close_requested(move || {
            with_app(&w, |app| {
                let _ = app.help.hide();
            });
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// Opens the Guide, or brings it to the front when it is open.
    pub fn show_help(&self) {
        let opening = !self.help.window().is_visible();
        if let Err(error) = self.help.show() {
            self.show_window_error(&error);
            return;
        }
        if opening {
            self.help.invoke_scroll_to_top();
            self.log.log(|| "AudioSync Guide: opened".to_owned());
        }
        self.help.invoke_take_focus();
        crate::platform::bring_to_front(self.help.window());
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
        self.help
            .set_show_how_shown(guide::notice_with_guide(&state.notices).is_some());
    }
}

#[cfg(test)]
mod tests {
    use slint::ComponentHandle;
    use slint::platform::Key;

    use super::super::tests::{app_over, draw_frames, press};
    use crate::ui_model::{MockBackend, NoticeMessage};

    fn heard_twice() -> NoticeMessage {
        NoticeMessage::HeardTwice {
            source: Some("Player".to_owned()),
            mute: vec!["TV".to_owned()],
            shared: Vec::new(),
            default_output: None,
        }
    }

    #[test]
    fn help_opens_one_guide_window_that_escape_closes() {
        let app = app_over(MockBackend::new());
        let window = app.help.window();
        assert!(!window.is_visible());
        app.main.invoke_help_requested();
        draw_frames();
        assert!(window.is_visible(), "Help > AudioSync Guide opens it");
        assert!(
            !app.guide.window().is_visible(),
            "not the double-audio guide"
        );
        // The one window, scrolled down, is brought to the front as it is.
        press(window, Key::PageDown);
        let scrolled = app.help.get_scrolled();
        app.main.invoke_help_requested();
        assert!(window.is_visible());
        assert_eq!(app.help.get_scrolled(), scrolled, "the same window");
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape closes it");

        app.main.invoke_help_requested();
        assert!(window.is_visible(), "it opens again after Escape");
        app.help.invoke_close_requested();
        assert!(!window.is_visible(), "Close closes it");
    }

    #[test]
    fn the_keyboard_scrolls_the_guide() {
        let app = app_over(MockBackend::new());
        let window = app.help.window();
        app.main.invoke_help_requested();
        draw_frames();
        let scrolled = || app.help.get_scrolled();
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
        assert!(app.help.get_show_how_shown(), "the notice is showing");
        app.help.invoke_show_how_requested();
        assert!(
            app.guide.window().is_visible(),
            "Show How opens the double-audio guide"
        );
        assert!(app.help.window().is_visible(), "the Guide stays open");

        let id = app.snapshot().notices[0].id;
        app.main
            .invoke_notice_dismissed(i32::try_from(id).expect("a small ID"));
        assert!(
            !app.help.get_show_how_shown(),
            "no notice, no Show How in the Guide"
        );
    }

    #[test]
    fn show_how_in_the_guide_opens_nothing_without_the_notice() {
        let app = app_over(MockBackend::new());
        assert!(app.snapshot().notices.is_empty());
        app.main.invoke_help_requested();
        assert!(!app.help.get_show_how_shown());
        app.help.invoke_show_how_requested();
        assert!(!app.guide.window().is_visible());
    }
}
