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
mod backdrop;
#[cfg(test)]
mod contrast;
#[cfg(any(windows, test))]
mod data_folder;
mod debug_log;
mod guide;
mod os_version;
#[cfg(any(windows, test))]
mod placement;
mod platform;
#[cfg(any(windows, test))]
mod settings;
#[cfg(any(windows, test))]
mod single_instance;
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

/// With the debug log on, `app` or `system` here opens the double-audio
/// guide at launch for a made-up notice (`guide::preview`).
const PREVIEW_GUIDE_VAR: &str = "AUDIOSYNC_PREVIEW_GUIDE";

/// With the debug log on, `light` or `dark` here shows every window in that
/// appearance, whatever Windows uses (`view::App::preview_theme`).
const PREVIEW_THEME_VAR: &str = "AUDIOSYNC_PREVIEW_THEME";

/// With the debug log on, `code` or `wrong` here opens the PIN dialog of an
/// Apple TV pairing at launch for a made-up Apple TV, as when it shows a PIN
/// or after a wrong one (`view::App::preview_pin`). Nothing is paired.
const PREVIEW_PIN_VAR: &str = "AUDIOSYNC_PREVIEW_PIN";

fn main() -> ExitCode {
    // Before the log, which a launch empties, and before any audio, tray or
    // window: a second launch only hands over to the running copy.
    #[cfg(windows)]
    let launched = std::time::Instant::now();
    #[cfg(windows)]
    let (instance, waited_for) = match single_instance::acquire() {
        single_instance::Acquired::First(instance) => (instance, None),
        single_instance::Acquired::Second => {
            // A copy that is quitting doesn't answer; once it's gone, this
            // launch starts instead, as on the Mac.
            let (outcome, first) = single_instance::second_launch(
                single_instance::hand_over,
                || match single_instance::acquire() {
                    single_instance::Acquired::First(instance) => Some(instance),
                    single_instance::Acquired::Second => None,
                },
                || launched.elapsed(),
                std::thread::sleep,
            );
            match first {
                Some(instance) => (instance, Some(outcome)),
                None => {
                    let log = DebugLog::appending_from_env();
                    log.log(|| outcome.log_line(std::process::id()));
                    log.flush(LOG_FLUSH_AT_EXIT);
                    return ExitCode::SUCCESS;
                }
            }
        }
    };
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

    #[cfg(windows)]
    if let Some(outcome) = waited_for {
        log.log(|| outcome.took_over_line());
    }
    #[cfg(windows)]
    match &instance {
        Ok(_) => log.log(|| "single instance: this is the only copy in the session".to_owned()),
        Err(error) => {
            log.log(|| format!("single instance: a second launch can't reach this copy ({error})"))
        }
    }

    match run(
        &log,
        #[cfg(windows)]
        instance.as_ref().ok(),
    ) {
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

/// What the windows show and drive: the Windows audio backend, with the
/// AirPlay sender, through the adapter that keeps the UI's own state. Both
/// log to `log`.
#[cfg(windows)]
fn backend(log: &DebugLog) -> Result<Box<dyn ui_model::Backend>, StartError> {
    let mut audio = audio::wasapi::open().map_err(StartError::Audio)?;
    log.log(|| "audio: the WASAPI backend is open".to_owned());
    audio.set_debug_log(log.clone());
    start_airplay(&mut audio, log);
    let settings_file = settings::SettingsFile::default_path().map_or_else(
        settings::SettingsFile::in_memory,
        settings::SettingsFile::at,
    );
    Ok(Box::new(adapter::AudioAdapter::with_settings(
        audio,
        platform::notice_time,
        log.clone(),
        settings_file,
    )))
}

/// Starts the AirPlay sender and its discovery, and hands both to the audio
/// backend, with the way to start a new one when the user retries after it
/// stopped. Without a sender the app runs on with local outputs only;
/// without discovery no AirPlay speaker is listed.
#[cfg(windows)]
fn start_airplay(audio: &mut audio::wasapi::WasapiBackend, log: &DebugLog) {
    let Some((link, feed)) = start_sender(log) else {
        return;
    };
    audio.set_airplay_with_pairings(link, feed, open_pairings(log));
    let log = log.clone();
    audio.set_airplay_starter(Box::new(move || start_sender(&log)));
}

/// A new AirPlay sender browsing for receivers, and the feed for its input;
/// `None` when it couldn't start.
#[cfg(windows)]
fn start_sender(
    log: &DebugLog,
) -> Option<(
    Box<dyn audio::airplay::AirPlayLink>,
    Option<audio::airplay::FeedSlot>,
)> {
    use audio::airplay::{FeedSlot, SenderLink};
    let (link, input) = match SenderLink::start() {
        Ok(started) => started,
        Err(error) => {
            log.log(|| format!("AirPlay: the sender couldn't start: {error}"));
            return None;
        }
    };
    match link.start_discovery() {
        Ok(()) => log.log(|| "AirPlay: browsing for receivers".to_owned()),
        Err(error) => log.log(|| format!("AirPlay: discovery couldn't start: {error}")),
    }
    Some((Box::new(link), Some(FeedSlot::new(input))))
}

/// The paired Apple TVs' credentials, from the file in the app's data
/// folder; kept for this run only when Windows names no such folder.
#[cfg(windows)]
fn open_pairings(log: &DebugLog) -> audio::pairings::PairingStore {
    use audio::pairings::{PairingStore, Sealer};
    let Some(path) = PairingStore::default_path() else {
        log.log(|| {
            "AirPlay pairings: no app data folder; pairings last until AudioSync quits".to_owned()
        });
        return PairingStore::in_memory();
    };
    let (store, loaded) = PairingStore::open(path, Sealer::current_user());
    log.log(|| format!("AirPlay pairings: {loaded}"));
    store
}

/// Off Windows, example speakers to work on the UI with.
#[cfg(not(windows))]
fn backend(log: &DebugLog) -> Result<Box<dyn ui_model::Backend>, StartError> {
    log.log(|| "audio: example speakers from the mock backend; nothing plays".to_owned());
    Ok(Box::new(ui_model::MockBackend::new()))
}

#[cfg(windows)]
thread_local! {
    /// The app and its log, for a second launch's request to show the main
    /// window.
    static RUNNING_APP: std::cell::RefCell<Option<(std::rc::Weak<view::App>, DebugLog)>> =
        const { std::cell::RefCell::new(None) };
}

/// A second launch asked for the main window: shown as by the tray's Open.
#[cfg(windows)]
fn show_main_for_second_launch() {
    let running = RUNNING_APP.with(|cell| {
        let cell = cell.borrow();
        let (app, log) = cell.as_ref()?;
        Some((app.upgrade()?, log.clone()))
    });
    if let Some((app, log)) = running {
        log.log(|| "a second launch asked for the main window: shown".to_owned());
        app.show_main();
    }
}

/// Shows the main window and runs the UI until the user exits.
fn run(
    log: &DebugLog,
    #[cfg(windows)] instance: Option<&single_instance::Instance>,
) -> Result<(), StartError> {
    let app = view::App::new(backend(log)?, log.clone())?;

    #[cfg(windows)]
    if let Some(instance) = instance {
        RUNNING_APP.with(|cell| {
            *cell.borrow_mut() = Some((std::rc::Rc::downgrade(&app), log.clone()));
        });
        instance.listen(show_main_for_second_launch);
    }

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

    // To look at both appearances without changing the Windows setting, only
    // while the debug log is on.
    if log.enabled()
        && let Some(dark) =
            std::env::var(PREVIEW_THEME_VAR)
                .ok()
                .and_then(|theme| match theme.as_str() {
                    "light" => Some(false),
                    "dark" => Some(true),
                    _ => None,
                })
    {
        log.log(|| {
            format!(
                "theme: {} for every window ({PREVIEW_THEME_VAR})",
                if dark { "dark" } else { "light" }
            )
        });
        app.preview_theme(dark);
    }
    app.show_main();
    log.log(|| "main window: shown".to_owned());
    // To look at the double-audio guide without streaming: a made-up notice,
    // only while the debug log is on.
    if log.enabled()
        && let Some(message) = std::env::var(PREVIEW_GUIDE_VAR)
            .ok()
            .as_deref()
            .and_then(guide::preview)
    {
        log.log(|| format!("guide: opened for a made-up notice ({PREVIEW_GUIDE_VAR})"));
        app.show_guide_for(&message);
    }
    // To look at the PIN dialog without an Apple TV: a made-up pairing, only
    // while the debug log is on.
    if log.enabled()
        && let Some(wrong) =
            std::env::var(PREVIEW_PIN_VAR)
                .ok()
                .and_then(|pin| match pin.as_str() {
                    "code" => Some(false),
                    "wrong" => Some(true),
                    _ => None,
                })
    {
        log.log(|| format!("PIN dialog: opened for a made-up Apple TV ({PREVIEW_PIN_VAR})"));
        app.preview_pin(wrong);
    }
    slint::run_event_loop_until_quit()?;
    log.log(|| "the event loop ended".to_owned());
    // Changes since the last poll, and where the main window is.
    app.save_settings();
    Ok(())
}

#[cfg(test)]
mod tests {
    /// A quit first waits for the outputs' fade-outs, at most the cap of
    /// S186, and then for the AirPlay sender's teardown and the debug log's
    /// flush. Together they stay within the 5 s budget.
    #[test]
    fn the_fade_cap_and_the_quit_waits_fit_in_5_s() {
        use audiosync_airplay::session::TEARDOWN_TIMEOUT;
        use audiosync_core::policy::session::QUIT_BUDGET;
        use audiosync_core::render::STOP_FADE_CAP;

        let fade = STOP_FADE_CAP.as_secs_f64();
        assert_eq!(fade, 0.5);
        assert_eq!(QUIT_BUDGET, 5.0);
        assert!(
            fade + TEARDOWN_TIMEOUT.as_secs_f64() + super::LOG_FLUSH_AT_EXIT.as_secs_f64()
                <= QUIT_BUDGET
        );
    }

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
        let guide = include_str!("../ui/guide.slint");
        assert!(flyout.contains("forward-focus: focus;") && flyout.contains("focus := FocusScope"));
        for window in [equalizer, guide] {
            assert!(
                window.contains("forward-focus: keys;") && window.contains("keys := FocusScope")
            );
        }
        assert!(include_str!("tray.rs").contains("app.flyout.invoke_take_focus();"));
        assert!(include_str!("view.rs").contains("self.equalizer.invoke_take_focus();"));
        assert!(include_str!("view.rs").contains("self.guide.invoke_take_focus();"));
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
        // A notice row reports when either of its buttons has the focus.
        let body = handler_body(
            widgets,
            "export component NoticeStack",
            "focus-changed(focused)",
        );
        assert!(body.contains("row.dismiss-focused = focused;"), "{body}");
        let body = handler_body(
            widgets,
            "text: Strings.notice-show-how;",
            "changed has-focus",
        );
        assert!(
            body.contains("row.guide-focused = self.has-focus;"),
            "{body}"
        );
        for flag in ["dismiss-focused", "guide-focused"] {
            let body = handler_body(
                widgets,
                "export component NoticeStack",
                &format!("changed {flag}"),
            );
            assert!(
                body.contains(
                    "root.button-focus-changed(self.dismiss-focused || self.guide-focused);"
                ),
                "{flag}: {body}"
            );
        }

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
                "button-focus-changed(focused)",
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
