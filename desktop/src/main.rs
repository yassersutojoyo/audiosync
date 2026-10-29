//! AudioSync for Windows 11 (in development).
//!
//! The Windows 11 check at startup, then the main window, the equalizer, the
//! tray icon with its flyout, and the About dialog, laid out like the macOS
//! app. The data comes through the `Backend` trait in `ui_model`, which
//! `os::start` opens. On Windows that is the adapter over the WASAPI audio
//! backend, which captures and plays for real; on Linux it is the adapter
//! over PipeWire, which captures the whole system's sound for the network
//! speakers. On other hosts, where the UI is worked on, the `mock-backend`
//! feature gives a mock with example
//! speakers: nothing plays and no audio device is opened.
//!
//! With `AUDIOSYNC_DEBUG=1` in the environment, the app writes what it does
//! to `%TEMP%\audiosync-debug.log` (`debug_log`).

// No console window next to the app; other systems ignore the attribute.
// Test builds keep the console so their output stays visible.
#![cfg_attr(not(test), windows_subsystem = "windows")]

#[cfg_attr(
    not(any(os_backend, test)),
    expect(
        dead_code,
        reason = "without an audio backend nothing drives the adapter"
    )
)]
mod adapter;
#[cfg_attr(
    not(any(os_backend, test)),
    expect(
        dead_code,
        reason = "without an audio backend the UI uses only its types"
    )
)]
#[cfg_attr(
    all(target_os = "linux", not(test)),
    expect(
        dead_code,
        reason = "the WASAPI capture formats, stream mixer and endpoint listing serve the Windows driver only"
    )
)]
mod audio;
mod backdrop;
#[cfg(test)]
mod contrast;
mod data_folder;
mod debug_log;
mod guide;
mod launch_at_login;
mod network_profile;
mod now_playing_card;
mod os;
mod os_version;
mod placement;
mod platform;
mod remote;
mod settings;
#[cfg(any(os_backend, target_os = "linux", test))]
mod single_instance;
mod speaker_names;
// Generated from brand/tokens.json: rustfmt leaves it as the generator wrote it.
#[cfg(test)]
#[rustfmt::skip]
mod tokens;
mod ui_model;
#[cfg(windows)]
mod updater;
mod view;

use std::process::ExitCode;
use std::time::Duration;

use slint::ComponentHandle;

use debug_log::DebugLog;

slint::include_modules!();

/// The app's id on Linux: the Wayland app id and X11 window class, and the
/// name of its .desktop and AppStream files.
#[cfg(any(target_os = "linux", test))]
const APP_ID: &str = "io.github.yassersutojoyo.audiosync";

/// How long the app waits, as it stops, for the debug log to reach its file.
const LOG_FLUSH_AT_EXIT: Duration = Duration::from_secs(1);

/// With the debug log on, `app` or `system` here opens the double-audio
/// guide at launch for a made-up notice (`guide::preview`).
const PREVIEW_GUIDE_VAR: &str = "AUDIOSYNC_PREVIEW_GUIDE";

/// With the debug log on, `1` here opens the AudioSync Guide at launch, to
/// look at it without the menu.
const PREVIEW_HELP_VAR: &str = "AUDIOSYNC_PREVIEW_HELP";

/// With the debug log on, `light` or `dark` here shows every window in that
/// appearance, whatever Windows uses, without saving it
/// (`view::App::apply_appearance`).
const PREVIEW_THEME_VAR: &str = "AUDIOSYNC_PREVIEW_THEME";

/// With the debug log on, `code` or `wrong` here opens the PIN dialog of an
/// Apple TV pairing at launch for a made-up Apple TV, as when it shows a PIN
/// or after a wrong one (`view::App::preview_pin`). Nothing is paired.
const PREVIEW_PIN_VAR: &str = "AUDIOSYNC_PREVIEW_PIN";

/// With the debug log on, `1` here shows example speakers in zones instead
/// of the real ones, one zone collapsed (`ui_model::MockBackend::zones_preview`).
/// Nothing plays.
#[cfg(feature = "mock-backend")]
const PREVIEW_ZONES_VAR: &str = "AUDIOSYNC_PREVIEW_ZONES";

/// With the debug log on, `list`, `left-off` or `empty` here shows example
/// speakers with that state of the Presets card instead of the real ones
/// (`ui_model::MockBackend::presets_preview`). Nothing plays.
#[cfg(feature = "mock-backend")]
const PREVIEW_PRESETS_VAR: &str = "AUDIOSYNC_PREVIEW_PRESETS";

fn main() -> ExitCode {
    // After Restart Now the quitting copy still holds the single instance;
    // claiming it first would hand this launch over to that copy.
    #[cfg(windows)]
    let updater::relaunch::Claimed { instance, note } =
        updater::relaunch::claim_after(std::env::args_os(), os::claim_instance);
    #[cfg(not(windows))]
    let (instance, note) = (os::claim_instance(), None::<String>);
    let Some(instance) = instance else {
        if let Some(note) = note {
            let log = DebugLog::appending_from_env();
            log.log(|| note);
            log.flush(LOG_FLUSH_AT_EXIT);
        }
        return ExitCode::SUCCESS;
    };
    #[cfg(windows)]
    let relaunched = updater::relaunch::marked_parent(std::env::args_os()).is_some();
    #[cfg(not(windows))]
    let relaunched = false;
    let log = DebugLog::from_env(relaunched);
    log.log(|| {
        format!(
            "AudioSync for {} {} (audiosync-core {}), process {}; times are UTC",
            platform::host_name(std::env::consts::OS),
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

    instance.log_state(&log);
    if let Some(note) = note {
        log.log(|| note);
    }

    match run(&log, &instance) {
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
    /// The audio backend couldn't open, for the system's reason.
    #[cfg_attr(
        all(not(os_backend), feature = "mock-backend"),
        expect(dead_code, reason = "the mock backend always opens")
    )]
    Audio(String),
}

impl StartError {
    /// What the error box says.
    fn message(&self) -> String {
        match self {
            // A window system that failed can't give the UI's strings.
            StartError::Window(error) => format!("AudioSync could not start: {error}"),
            // The UI's words, with the system's reason as the detail.
            StartError::Audio(error) => AboutDialog::new()
                .map(|dialog| {
                    let strings = dialog.global::<Strings>();
                    strings.invoke_audio_failed(error.as_str().into()).into()
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

/// Shows the main window and runs the UI until the user exits.
fn run(log: &DebugLog, instance: &os::Instance) -> Result<(), StartError> {
    #[cfg(feature = "mock-backend")]
    let zones_preview = log.enabled() && std::env::var(PREVIEW_ZONES_VAR).as_deref() == Ok("1");
    #[cfg(feature = "mock-backend")]
    let os::Started { backend, media } = if zones_preview {
        log.log(|| {
            format!("audio: example speakers in zones from the mock backend; nothing plays ({PREVIEW_ZONES_VAR})")
        });
        os::start_with(log, Box::new(ui_model::MockBackend::zones_preview()))
    } else if let Some(which) = log
        .enabled()
        .then(|| std::env::var(PREVIEW_PRESETS_VAR).ok())
        .flatten()
        .as_deref()
        .and_then(ui_model::PresetsPreview::from_value)
    {
        log.log(|| {
            format!("audio: example speakers with presets ({which:?}) from the mock backend; nothing plays ({PREVIEW_PRESETS_VAR})")
        });
        os::start_with(log, Box::new(ui_model::MockBackend::presets_preview(which)))
    } else {
        os::start(log)?
    };
    #[cfg(not(feature = "mock-backend"))]
    let os::Started { backend, media } = os::start(log)?;
    let app = view::App::new(backend, log.clone())?;
    // Needs the platform the windows above set up, and must come before the
    // first one shows: Wayland matches windows to the .desktop file by it.
    #[cfg(target_os = "linux")]
    if let Err(error) = slint::set_xdg_app_id(APP_ID) {
        log.log(|| format!("app id: not set ({error})"));
    }
    #[cfg(feature = "mock-backend")]
    if zones_preview {
        app.set_zone_expanded("zone:upstairs", false);
    }
    media.attach(&app);
    instance.listen(&app, log);

    // With a tray icon, closing the main window keeps AudioSync running in
    // the notification area; without one, it quits.
    let tray = os::create_tray(&app, log);

    // Windows may end the process once this handler returns, before the event loop ends.
    #[cfg(windows)]
    platform::on_session_end({
        let app = std::rc::Rc::downgrade(&app);
        let log = log.clone();
        Box::new(move || {
            let Some(app) = app.upgrade() else {
                return;
            };
            let saved = app.at_session_end();
            log.log(|| {
                format!(
                    "Windows session ends: settings {}",
                    if saved { "saved" } else { "not saved" }
                )
            });
            log.flush(LOG_FLUSH_AT_EXIT);
        })
    });

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
        app.apply_appearance(if dark {
            ui_model::Appearance::Dark
        } else {
            ui_model::Appearance::Light
        });
    }
    let tray_available = tray.is_some();
    let background = launch_at_login::started_in_background(std::env::args_os());
    if launch_at_login::opens_main_window(background, tray_available) {
        app.show_main();
        log.log(|| "main window: shown".to_owned());
    } else {
        log.log(|| {
            format!(
                "main window: not shown ({} at sign-in; the tray icon opens it)",
                launch_at_login::BACKGROUND_FLAG
            )
        });
    }
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
    if log.enabled() && std::env::var(PREVIEW_HELP_VAR).as_deref() == Ok("1") {
        log.log(|| format!("AudioSync Guide: opened at launch ({PREVIEW_HELP_VAR})"));
        app.show_help();
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
    app.updater_at_exit();
    // Changes since the last poll, and where the main window is.
    app.save_settings();
    app.stop_remote_at_exit();
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The Wayland app id, the .desktop file's window class and name, and
    /// the AppStream id and launchable must agree, or the desktop shows the
    /// window without its icon and name.
    #[test]
    fn the_linux_desktop_files_name_the_app_id() {
        let desktop = include_str!("../linux/io.github.yassersutojoyo.audiosync.desktop");
        let metainfo = include_str!("../linux/io.github.yassersutojoyo.audiosync.metainfo.xml");
        for line in [
            format!("StartupWMClass={}", super::APP_ID),
            format!("Icon={}", super::APP_ID),
            "Exec=audiosync".to_owned(),
        ] {
            assert!(desktop.lines().any(|l| l == line), "{line}");
        }
        assert!(metainfo.contains(&format!("<id>{}</id>", super::APP_ID)));
        assert!(metainfo.contains(&format!(
            "<launchable type=\"desktop-id\">{}.desktop</launchable>",
            super::APP_ID
        )));
        let build = include_str!("../../scripts/build_appimage.sh");
        let id_line = format!("id={}", super::APP_ID);
        assert!(build.lines().any(|l| l == id_line));
    }

    /// A quit first waits for the outputs' fade-outs, at most the cap of
    /// S186, and then for the AirPlay sender's teardown, while the Google
    /// Cast host shuts down on a thread of its own; then the debug log
    /// flushes. Together they stay within the 5 s budget.
    #[test]
    fn the_fade_cap_and_the_quit_waits_fit_in_5_s() {
        use audiosync_airplay::session::TEARDOWN_TIMEOUT;
        use audiosync_cast::host::SHUTDOWN_MARGIN;
        use audiosync_core::policy::session::QUIT_BUDGET;
        use audiosync_core::render::STOP_FADE_CAP;

        let fade = STOP_FADE_CAP.as_secs_f64();
        assert_eq!(fade, 0.5);
        assert_eq!(QUIT_BUDGET, 5.0);
        let cast = (audiosync_cast::client::QUIT_BUDGET + SHUTDOWN_MARGIN).as_secs_f64();
        let airplay = fade + TEARDOWN_TIMEOUT.as_secs_f64();
        assert!(airplay.max(cast) + super::LOG_FLUSH_AT_EXIT.as_secs_f64() <= QUIT_BUDGET);
    }

    /// The About dialog, opened from the top-level menu, names AudioSync's
    /// copyright and license and credits Slint with its AboutSlint widget; a
    /// UI change must not drop them.
    #[test]
    fn the_about_dialog_keeps_the_slint_attribution() {
        let about = include_str!("../ui/about.slint");
        let main = include_str!("../ui/main.slint");
        assert!(
            about.contains("AboutSlint {"),
            "the About dialog shows AboutSlint"
        );
        assert!(
            about.contains("text: Strings.about-copyright;")
                && about.contains("text: Strings.about-license;"),
            "the About dialog shows the copyright and the license"
        );
        assert!(
            main.contains("MenuBar {") && main.contains("root.about-requested();"),
            "the top-level menu opens the About dialog"
        );
    }

    /// The Mac's menu shortcuts and what each is on Windows: the `Strings`
    /// title of the menu item, its `@keys`, and how the docs write it. The
    /// Mac's Hide (Command-H), Hide Others (Option-Command-H), Minimize
    /// (Command-M) and AudioSync window (Command-0) have none: Windows has no
    /// hiding an app, minimizing is the title bar's (Windows+Down), and the
    /// menu bar is in the main window itself. Its Edit menu is the text
    /// fields' own Ctrl keys.
    const MENU_SHORTCUTS: &[(&str, &str, &str, &str)] = &[
        (
            "Apply Typed Delay, Command-S",
            "menu-apply-delay",
            "Control + S",
            "Ctrl+S",
        ),
        (
            "Close Window, Command-W",
            "menu-close-window",
            "Control + W",
            "Ctrl+W",
        ),
        (
            "Quit AudioSync, Command-Q",
            "menu-exit",
            "Control + Q",
            "Ctrl+Q",
        ),
        (
            "New Zone from Active Speakers, Command-N",
            "new-zone",
            "Control + N",
            "Ctrl+N",
        ),
        ("Refresh Devices, Command-R", "refresh-devices", "F5", "F5"),
        (
            "Equalizer, Option-Command-E",
            "equalizer",
            "Control + E",
            "Ctrl+E",
        ),
        ("AudioSync Guide, Command-?", "help-title", "F1", "F1"),
    ];

    /// Keys Windows itself takes, so no menu item may use them.
    const RESERVED_BY_WINDOWS: &[&str] = &[
        "Alt + F4",
        "Alt + Tab",
        "Alt + Space",
        "Control + Escape",
        "Control + Alt + Delete",
        "F10",
        "F12",
    ];

    #[test]
    fn each_mac_menu_shortcut_has_its_windows_key_in_the_menu_and_the_docs() {
        let main = include_str!("../ui/main.slint");
        let docs = include_str!("../../docs/WINDOWS_UI.md");
        let menu = &main[main.find("MenuBar {").expect("the menu bar is there")..];
        let mut seen = std::collections::BTreeSet::new();
        for (mac, title, keys, written) in MENU_SHORTCUTS {
            let item = menu
                .find(&format!("title: Strings.{title};"))
                .unwrap_or_else(|| panic!("{mac}: the menu has its item"));
            let rest = &menu[item..];
            let end = rest.find('}').expect("the item ends");
            assert!(
                rest[..end].contains(&format!("shortcut: @keys({keys});")),
                "{mac}: the item shows {keys}"
            );
            assert!(seen.insert(*keys), "{keys} is used once");
            assert!(
                !RESERVED_BY_WINDOWS.contains(keys),
                "{keys} is Windows' own"
            );
            assert!(
                docs.contains(&format!("| {written} |")),
                "{mac}: docs/WINDOWS_UI.md lists {written} under Keyboard"
            );
        }
        let shown = menu.matches("shortcut: @keys(").count();
        assert_eq!(
            shown,
            MENU_SHORTCUTS.len(),
            "every menu shortcut is in the table"
        );
    }

    const CTRL_W: &str = "event.text == Key.Escape || (event.modifiers.control && event.text.to-lowercase() == \"w\")";
    const CTRL_Q: &str = "if event.modifiers.control && event.text.to-lowercase() == \"q\" {\n                root.exit-requested();";

    /// A checkout on Windows can have CRLF line endings; the snippets above
    /// are written with LF.
    fn lf(source: &str) -> String {
        source.replace("\r\n", "\n")
    }

    /// The windows without a menu bar: the equalizer, both Guides and About.
    fn windows_without_a_menu_bar() -> [(&'static str, &'static str, &'static str); 4] {
        [
            (
                "equalizer",
                include_str!("../ui/equalizer.slint"),
                "root.close-requested();",
            ),
            (
                "AudioSync Guide",
                include_str!("../ui/help.slint"),
                "root.close-requested();",
            ),
            (
                "double-audio guide",
                include_str!("../ui/guide.slint"),
                "root.close-requested();",
            ),
            (
                "About dialog",
                include_str!("../ui/about.slint"),
                "root.closed();",
            ),
        ]
    }

    /// Command-W closes whichever window is in front on the Mac; on Windows
    /// the windows without a menu bar take Ctrl+W themselves, as Escape, and
    /// a dialog cancels on it.
    #[test]
    fn ctrl_w_closes_the_windows_without_a_menu_bar() {
        for (name, source, close) in windows_without_a_menu_bar() {
            let source = lf(source);
            let keys = &source[source.find("keys := FocusScope {").expect("its keys")..];
            let first = &keys[..keys.find(close).expect("it closes")];
            assert!(
                first.contains(CTRL_W),
                "the {name} closes on Escape and Ctrl+W"
            );
        }
        let dialogs = lf(include_str!("../ui/dialogs.slint"));
        assert_eq!(
            dialogs.matches("if event.text == Key.Escape").count(),
            dialogs
                .matches(&format!(
                    "if {CTRL_W} {{\n                root.cancelled();"
                ))
                .count(),
            "every dialog that cancels on Escape cancels on Ctrl+W"
        );
    }

    /// Command-Q quits from any window on the Mac, so Ctrl+Q does from the
    /// windows without a menu bar too.
    #[test]
    fn ctrl_q_exits_from_the_windows_without_a_menu_bar() {
        let view = include_str!("view.rs");
        for (name, source, _) in windows_without_a_menu_bar() {
            let source = lf(source);
            let keys = &source[source.find("keys := FocusScope {").expect("its keys")..];
            assert!(keys.contains(CTRL_Q), "the {name} exits on Ctrl+Q");
        }
        for window in ["equalizer", "help", "guide", "about"] {
            assert!(
                view.contains(&format!("self.{window}.on_exit_requested(quit);")),
                "the {window} window's Ctrl+Q quits"
            );
        }
    }

    /// `platform::repaint_frame` sends muda's private `MENU_UPDATE_THEME`
    /// (201), which only muda 0.19.3 is known to take; Slint asks for ^0.19.
    #[test]
    fn slint_draws_the_menu_bar_with_muda_0_19_3() {
        let lock = include_str!("../Cargo.lock");
        let winit = &lock[lock
            .find("name = \"i-slint-backend-winit\"")
            .expect("Slint's winit backend is locked")..];
        let winit = &winit[..winit.find("[[package]]").unwrap_or(winit.len())];
        assert!(
            winit.contains("\"muda 0.19.3\""),
            "Slint's muda moved off 0.19.3: check that its MENU_UPDATE_THEME is still \
             message 201 with dark 0 and light 1 before changing MUDA_MENU_BAR_THEME \
             in platform.rs"
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
            stack.contains("for notice[index] in root.notices:")
                && stack.contains("if root.notices.length > 1:")
                && !stack.contains("visible:"),
            "the stack lists every notice it gets and no hidden ones"
        );
    }

    /// A dismiss button names its notice in a few words: after a dismiss the
    /// next notice's button takes the focus, and Narrator then says which
    /// notice it is without reading the whole notice again.
    #[test]
    fn a_notice_dismiss_button_names_its_notice() {
        let widgets = include_str!("../ui/widgets.slint");
        let stack = &widgets[widgets
            .find("export component NoticeStack")
            .expect("NoticeStack is there")..];
        assert!(stack.contains("label: Strings.dismiss-notice(notice.title);"));
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
                "for zone in root.zones: VerticalLayout {",
                "focus-changed(focused)",
                "zone-focused",
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
