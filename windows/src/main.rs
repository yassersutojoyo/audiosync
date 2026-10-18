//! AudioSync for Windows 11 (in development).
//!
//! The Windows 11 check at startup, then the main window, the equalizer, the
//! tray icon with its flyout, and the About dialog, laid out like the macOS
//! app. The data comes through the `Backend` trait in `ui_model`. On Windows
//! that is the adapter over the WASAPI audio backend, which captures and plays
//! for real. On other hosts, where the UI is worked on, a mock with example
//! speakers stands in: nothing plays and no audio device is opened.
//!
//! With `AUDIOSYNC_DEBUG=1` in the environment, the app writes what it does
//! to `%TEMP%\audiosync-debug.log` (`debug_log`).

// No console window next to the app. Test builds keep the console so their
// output stays visible.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[cfg(any(windows, test))]
mod adapter;
#[cfg(any(windows, test))]
mod audio;
#[cfg(test)]
mod contrast;
mod debug_log;
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

use std::process::ExitCode;
use std::time::Duration;

#[cfg(windows)]
use slint::ComponentHandle;

use debug_log::DebugLog;

slint::include_modules!();

/// How long the app waits, as it stops, for the debug log to reach its file.
const LOG_FLUSH_AT_EXIT: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    let log = DebugLog::from_env();
    log.log(|| {
        format!(
            "AudioSync for Windows {} (audiosync-core {}), process {}; times are UTC",
            env!("CARGO_PKG_VERSION"),
            audiosync_core::VERSION,
            std::process::id(),
        )
    });
    log.log(|| format!("system: {}", platform::system_summary()));
    if let Some(version) = platform::windows_version()
        && !os_version::is_supported(version)
    {
        stop_with_error(&log, os_version::UNSUPPORTED_MESSAGE);
        return ExitCode::FAILURE;
    }

    match run(&log) {
        Ok(()) => {
            log.log(|| "exiting".to_owned());
            log.flush(LOG_FLUSH_AT_EXIT);
            ExitCode::SUCCESS
        }
        Err(error) => {
            stop_with_error(&log, &error.message());
            ExitCode::FAILURE
        }
    }
}

/// Logs why the app can't run, then tells the user.
fn stop_with_error(log: &DebugLog, message: &str) {
    log.log(|| format!("cannot run: {message}"));
    log.flush(LOG_FLUSH_AT_EXIT);
    platform::show_error(message);
}

/// Why AudioSync couldn't start.
#[derive(Debug)]
enum StartError {
    Window(slint::PlatformError),
    #[cfg(windows)]
    Audio(audio::backend::BackendError),
}

impl StartError {
    /// What the error box says.
    fn message(&self) -> String {
        match self {
            // A window system that failed can't give the UI's strings.
            StartError::Window(error) => format!("AudioSync could not start: {error}"),
            // The UI's words, with the system's reason as the detail.
            #[cfg(windows)]
            StartError::Audio(error) => AboutDialog::new()
                .map(|dialog| {
                    let strings = dialog.global::<Strings>();
                    strings.invoke_audio_failed(error.to_string().into()).into()
                })
                .unwrap_or_else(|_| format!("AudioSync could not start: {error}")),
        }
    }
}

impl From<slint::PlatformError> for StartError {
    fn from(error: slint::PlatformError) -> Self {
        StartError::Window(error)
    }
}

/// What the windows show and drive: the Windows audio backend, through the
/// adapter that keeps the UI's own state. Both log to `log`.
#[cfg(windows)]
fn backend(log: &DebugLog) -> Result<Box<dyn ui_model::Backend>, StartError> {
    let mut audio = audio::wasapi::open().map_err(StartError::Audio)?;
    log.log(|| "audio: the WASAPI backend is open".to_owned());
    audio.set_debug_log(log.clone());
    Ok(Box::new(adapter::AudioAdapter::new(
        audio,
        platform::notice_time,
        log.clone(),
    )))
}

/// Off Windows, example speakers to work on the UI with.
#[cfg(not(windows))]
fn backend(log: &DebugLog) -> Result<Box<dyn ui_model::Backend>, StartError> {
    log.log(|| "audio: example speakers from the mock backend; nothing plays".to_owned());
    Ok(Box::new(ui_model::MockBackend::new()))
}

/// Shows the main window and runs the UI until the user exits.
fn run(log: &DebugLog) -> Result<(), StartError> {
    let app = view::App::new(backend(log)?)?;

    // With a tray icon, closing the main window keeps AudioSync running in
    // the notification area; without one (or off Windows), it quits.
    #[cfg(windows)]
    let _tray = match tray::Tray::create(&app) {
        Ok(tray) => {
            log.log(|| "tray icon: shown".to_owned());
            app.set_tray_available(true);
            Some(tray)
        }
        Err(error) => {
            log.log(|| format!("tray icon: not shown ({error}); closing the main window quits"));
            None
        }
    };

    app.show_main();
    log.log(|| "main window: shown".to_owned());
    slint::run_event_loop_until_quit()?;
    log.log(|| "the event loop ended".to_owned());
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

    /// Slint leaves nothing focused when the focused control is removed, and
    /// Escape then stops closing the flyout. Every flyout control a refresh
    /// can remove reports its keyboard focus, take-focus forgets those
    /// reports, and a refresh that removed the focused control calls it.
    #[test]
    fn the_flyout_takes_the_focus_back_when_the_focused_row_goes_away() {
        let widgets = include_str!("../ui/widgets.slint");
        for component in [
            "export component IconButton",
            "export component SpeakerBadge",
        ] {
            let body = handler_body(widgets, component, "focus-changed-event");
            assert!(
                body.contains("root.focus-changed(self.has-focus);"),
                "{component}: {body}"
            );
        }
        let body = handler_body(
            widgets,
            "export component NoticeStack",
            "focus-changed(focused)",
        );
        assert!(
            body.contains("root.dismiss-focus-changed(focused);"),
            "{body}"
        );

        let flyout = include_str!("../ui/flyout.slint");
        let body = handler_body(flyout, "component FlyoutRow", "focus-changed(focused)");
        assert!(body.contains("root.focus-changed(focused);"), "{body}");
        let take_focus = &flyout[flyout
            .find("public function take-focus() {")
            .expect("take-focus is there")..];
        let take_focus = &take_focus[..take_focus.find('}').expect("take-focus ends")];
        assert!(take_focus.contains("focus.focus();"), "{take_focus}");
        for (element, handler, flag) in [
            (
                "NoticeStack {",
                "dismiss-focus-changed(focused)",
                "notice-focused",
            ),
            (
                "text: Strings.more-notices(",
                "changed has-focus",
                "more-notices-focused",
            ),
            (
                "for group in root.groups: FlyoutRow {",
                "focus-changed(focused)",
                "group-focused",
            ),
            (
                "for speaker in root.speakers: FlyoutRow {",
                "focus-changed(focused)",
                "speaker-focused",
            ),
        ] {
            let body = handler_body(flyout, element, handler);
            assert!(
                body.contains(&format!("root.{flag} = ")),
                "{element}: {body}"
            );
            assert!(
                take_focus.contains(&format!("root.{flag} = false;")),
                "take-focus clears {flag}"
            );
        }

        let view = include_str!("view.rs");
        let refresh = &view[view
            .find("pub fn refresh(&self)")
            .expect("refresh is there")..];
        assert!(refresh.contains("self.restore_flyout_focus(flyout_removed);"));
        assert!(view.contains("flyout.invoke_take_focus();"));
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
                    ("power := ToggleSwitch", "self.checked = root.running;"),
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
                    ("power := ToggleSwitch", "self.checked = root.running;"),
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
                    ("power := ToggleSwitch", "self.checked = root.enabled;"),
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
