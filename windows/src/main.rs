//! AudioSync for Windows 11 (in development).
//!
//! The Windows 11 check at startup, then the main window, the equalizer, the
//! tray icon with its flyout, and the About dialog, laid out like the macOS
//! app. The data comes through the `Backend` trait in `ui_model`. On Windows
//! that is the adapter over the WASAPI audio backend, which captures and plays
//! for real. On other hosts, where the UI is worked on, a mock with example
//! speakers stands in: nothing plays and no audio device is opened.

// No console window next to the app. Test builds keep the console so their
// output stays visible.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[cfg(any(windows, test))]
mod adapter;
#[cfg(any(windows, test))]
mod audio;
#[cfg(test)]
mod contrast;
mod os_version;
#[cfg(any(windows, test))]
mod placement;
mod platform;
// Generated from brand/tokens.json: rustfmt leaves it as the generator wrote it.
#[cfg(test)]
#[rustfmt::skip]
mod tokens;
#[cfg(windows)]
mod tray;
mod ui_model;
mod view;

use std::fmt;
use std::process::ExitCode;

slint::include_modules!();

fn main() -> ExitCode {
    if let Some(version) = platform::windows_version()
        && !os_version::is_supported(version)
    {
        platform::show_error(os_version::UNSUPPORTED_MESSAGE);
        return ExitCode::FAILURE;
    }

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            platform::show_error(&format!("AudioSync could not start: {error}"));
            ExitCode::FAILURE
        }
    }
}

/// Why AudioSync couldn't start.
#[derive(Debug)]
enum StartError {
    Window(slint::PlatformError),
    #[cfg(windows)]
    Audio(audio::backend::BackendError),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Window(error) => error.fmt(f),
            #[cfg(windows)]
            StartError::Audio(error) => error.fmt(f),
        }
    }
}

impl From<slint::PlatformError> for StartError {
    fn from(error: slint::PlatformError) -> Self {
        StartError::Window(error)
    }
}

/// What the windows show and drive: the Windows audio backend, through the
/// adapter that keeps the UI's own state.
#[cfg(windows)]
fn backend() -> Result<Box<dyn ui_model::Backend>, StartError> {
    let audio = audio::wasapi::open().map_err(StartError::Audio)?;
    Ok(Box::new(adapter::AudioAdapter::new(
        audio,
        platform::notice_time,
    )))
}

/// Off Windows, example speakers to work on the UI with.
#[cfg(not(windows))]
fn backend() -> Result<Box<dyn ui_model::Backend>, StartError> {
    Ok(Box::new(ui_model::MockBackend::new()))
}

/// Shows the main window and runs the UI until the user exits.
fn run() -> Result<(), StartError> {
    let app = view::App::new(backend()?)?;

    // With a tray icon, closing the main window keeps AudioSync running in
    // the notification area; without one (or off Windows), it quits.
    #[cfg(windows)]
    let _tray = match tray::Tray::create(&app) {
        Ok(tray) => {
            app.set_tray_available(true);
            Some(tray)
        }
        Err(error) => {
            eprintln!("no tray icon: {error}");
            None
        }
    };

    app.show_main();
    slint::run_event_loop_until_quit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Slint's royalty-free license asks for the AboutSlint widget in an About
    /// dialog that the top-level menu opens; a UI change must not drop it.
    #[test]
    fn the_about_dialog_keeps_the_slint_attribution() {
        let about = include_str!("../ui/about.slint");
        let main = include_str!("../ui/main.slint");
        assert!(
            about.contains("AboutSlint {"),
            "the About dialog shows AboutSlint"
        );
        assert!(
            main.contains("MenuBar {") && main.contains("root.about-requested();"),
            "the top-level menu opens the About dialog"
        );
    }

    /// The statements of the first handler `handler =>` after `after` in a
    /// .slint source, up to the handler's closing brace.
    fn handler_body<'a>(source: &'a str, after: &str, handler: &str) -> &'a str {
        let from = source.find(after).expect("the element is there");
        let rest = &source[from..];
        let start = rest
            .find(&format!("{handler} => {{"))
            .expect("the handler is there");
        let body = &rest[start..];
        &body[..body.find('}').expect("the handler ends")]
    }

    /// The flyout's Dismiss All dismisses the notices (it used to open the
    /// main window), and the stack draws that row only for the rows it shows.
    #[test]
    fn dismiss_all_in_the_flyout_dismisses_the_notices() {
        let flyout = include_str!("../ui/flyout.slint");
        let body = handler_body(flyout, "NoticeStack {", "dismissed-all");
        assert!(body.contains("root.notices-dismissed-all();"), "{body}");
        assert!(!body.contains("open-app"), "{body}");

        let widgets = include_str!("../ui/widgets.slint");
        let stack = &widgets[widgets
            .find("export component NoticeStack")
            .expect("NoticeStack is there")..];
        assert!(
            stack.contains("for notice in root.notices:")
                && stack.contains("if root.notices.length > 1:")
                && !stack.contains("visible:"),
            "the stack lists every notice it gets and no hidden ones"
        );
    }

    /// Key events go to the focused item: the flyout and the equalizer focus
    /// their Escape handler when they open.
    #[test]
    fn escape_works_as_soon_as_the_flyout_or_equalizer_opens() {
        let flyout = include_str!("../ui/flyout.slint");
        let equalizer = include_str!("../ui/equalizer.slint");
        assert!(flyout.contains("forward-focus: focus;") && flyout.contains("focus := FocusScope"));
        assert!(
            equalizer.contains("forward-focus: keys;") && equalizer.contains("keys := FocusScope")
        );
        assert!(include_str!("tray.rs").contains("app.flyout.invoke_take_focus();"));
        assert!(include_str!("view.rs").contains("self.equalizer.invoke_take_focus();"));
    }

    /// A click changes a switch or combo box before the engine answers; each
    /// handler sets the control back to the state the engine reports.
    #[test]
    fn switches_and_combo_boxes_show_the_engine_state_after_a_click() {
        for (file, source, pairs) in [
            (
                "main.slint",
                include_str!("../ui/main.slint"),
                [
                    ("power := Switch", "self.checked = root.running;"),
                    (
                        "source := ComboBox",
                        "self.current-index = root.source-index;",
                    ),
                ],
            ),
            (
                "flyout.slint",
                include_str!("../ui/flyout.slint"),
                [
                    ("power := Switch", "self.checked = root.running;"),
                    (
                        "source := ComboBox",
                        "self.current-index = root.source-index;",
                    ),
                ],
            ),
            (
                "equalizer.slint",
                include_str!("../ui/equalizer.slint"),
                [
                    ("power := Switch", "self.checked = root.enabled;"),
                    (
                        "presets := ComboBox",
                        "self.current-index = root.preset-index;",
                    ),
                ],
            ),
        ] {
            for (element, reset) in pairs {
                let handler = if element.ends_with("Switch") {
                    "toggled"
                } else {
                    "selected"
                };
                let body = handler_body(source, element, handler);
                assert!(
                    body.contains(reset),
                    "{file}: {element} resets after {handler}"
                );
            }
        }
    }

    /// The contrast pairs name colors and opacities that brand/tokens.json has.
    #[test]
    fn the_contrast_pairs_name_known_tokens() {
        use crate::tokens::{COLORS, CONTRAST_PAIRS, OPACITIES};
        let color = |name: &str| COLORS.iter().any(|c| c.name == name);
        for (foreground, background) in CONTRAST_PAIRS {
            let (base, opacity) = foreground.split_once('@').unwrap_or((foreground, ""));
            assert!(color(base), "{foreground} is a color token");
            assert!(
                opacity.is_empty() || OPACITIES.iter().any(|(name, _)| *name == opacity),
                "{foreground} names a known opacity"
            );
            assert!(color(background), "{background} is a color token");
        }
        let teal = COLORS
            .iter()
            .find(|c| c.name == "teal")
            .expect("a teal token");
        assert_eq!((teal.light, teal.dark), ([28, 159, 145], [28, 159, 145]));
    }
}
