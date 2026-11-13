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
mod ambient;
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
#[cfg(all(test, windows))]
#[path = "../../cast/tests/common/crash_dump.rs"]
mod crash_dump;
mod data_folder;
mod debug_log;
mod frameless;
mod guide;
mod launch_at_login;
mod network_profile;
mod now_playing_card;
mod os;
mod os_version;
mod package;
mod placement;
mod platform;
mod remote;
mod settings;
mod setup_check;
#[cfg(any(os_backend, target_os = "linux", test))]
mod single_instance;
mod speaker_names;
mod title_bar;
// Generated from brand/tokens.json: rustfmt leaves it as the generator wrote it.
#[cfg(test)]
#[rustfmt::skip]
mod tokens;
mod ui_model;
// Linux test builds compile it only for its Linux-only tests; the window
// wires it up in release builds.
#[cfg(any(windows, target_os = "linux"))]
#[cfg_attr(all(test, not(windows)), allow(dead_code))]
mod updater;
mod view;
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows keyboard hook takes the keys")
)]
mod volume_keys;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the Slint views adopt these rules later")
)]
mod window_rules;

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

/// With the debug log on, `1` here opens Check Setup at launch, to look at it
/// without the menu.
const PREVIEW_SETUP_VAR: &str = "AUDIOSYNC_PREVIEW_SETUP";

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

/// With the debug log on, part of a local output's name here switches the
/// first output whose name holds it on at launch, as a click on its switch
/// does, so a release check streams without a click. The launch is one
/// without auto-connect (`debug_log::NO_AUTO_CONNECT_VAR`), so nothing of it
/// is saved. Meant for a virtual output such as Steam Streaming Speakers,
/// never a real one.
const PREVIEW_PLAY_VAR: &str = "AUDIOSYNC_PREVIEW_PLAY";

fn main() -> ExitCode {
    // The elevated copy of an update (spec S457) swaps files and ends; it
    // never claims the instance the waiting copy holds.
    if let Some(code) = updater::elevated::from_command_line() {
        std::process::exit(code);
    }
    // After Restart Now the quitting copy still holds the single instance;
    // claiming it first would hand this launch over to that copy.
    #[cfg(any(windows, all(target_os = "linux", not(test))))]
    let updater::relaunch::Claimed { instance, note } =
        updater::relaunch::claim_after(std::env::args_os(), os::claim_instance);
    #[cfg(not(any(windows, all(target_os = "linux", not(test)))))]
    let (instance, note) = (os::claim_instance(), None::<String>);
    let Some(instance) = instance else {
        if let Some(note) = note {
            let log = DebugLog::appending_from_env();
            log.log(|| note);
            log.flush(LOG_FLUSH_AT_EXIT);
        }
        return ExitCode::SUCCESS;
    };
    #[cfg(any(windows, all(target_os = "linux", not(test))))]
    let relaunched = updater::relaunch::marked_parent(std::env::args_os()).is_some();
    #[cfg(not(any(windows, all(target_os = "linux", not(test)))))]
    let relaunched = false;
    let log = DebugLog::from_env(relaunched);
    debug_log::log_panics(log.clone());
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
    #[cfg(windows)]
    log.log(package::this_log_line);
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
        app.set_zone_expanded("zone:upstairs", false, false);
        app.set_zone_expanded("zone:upstairs", false, true);
    }
    media.attach(&app);
    instance.listen(&app, log);

    // With a tray icon, closing the main window keeps AudioSync running in
    // the notification area; without one, it quits.
    let tray = os::create_tray(&app, log);

    // Windows may end the process once this handler returns, before the event loop ends.
    #[cfg(windows)]
    platform::on_session_end(session_ending(&app, log, "Windows session ends"));
    // A logout sends SIGTERM, and its default ends the process with no quit step.
    #[cfg(target_os = "linux")]
    platform::on_exit_signal(session_ending(&app, log, "exit signal"));
    #[cfg(windows)]
    platform::on_console_quit(quit_through_event_loop);

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
    if log.enabled() && std::env::var(PREVIEW_SETUP_VAR).as_deref() == Ok("1") {
        log.log(|| format!("Check Setup: opened at launch ({PREVIEW_SETUP_VAR})"));
        app.show_setup();
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
    if let Some(wanted) = preview_play_env(log) {
        let state = app.snapshot();
        match preview_play_output(&state.speakers, &wanted) {
            Some(output) => {
                log.log(|| format!("{} switched on at launch ({PREVIEW_PLAY_VAR})", output.name));
                app.update(|backend| backend.set_speaker_enabled(&output.id, true));
            }
            None => log.log(|| {
                format!("no local output's name holds {wanted:?}; nothing switched on ({PREVIEW_PLAY_VAR})")
            }),
        }
    }
    slint::run_event_loop_until_quit()?;
    log.log(|| "the event loop ended".to_owned());
    app.at_quit();
    Ok(())
}

/// The value of `AUDIOSYNC_PREVIEW_PLAY`, only while the debug log is on; an
/// empty one would match every output.
fn preview_play_wanted(debug: bool, value: Option<String>) -> Option<String> {
    value.filter(|wanted| debug && !wanted.is_empty())
}

/// The value of `AUDIOSYNC_PREVIEW_PLAY` this launch acts on.
fn preview_play_env(log: &DebugLog) -> Option<String> {
    preview_play_wanted(log.enabled(), std::env::var(PREVIEW_PLAY_VAR).ok())
}

/// The variable that makes this launch one without auto-connect, if one
/// does: the preview switch's output stays on for this run only.
fn auto_connect_skipped_by(
    no_auto_connect: bool,
    preview_play: Option<&str>,
) -> Option<&'static str> {
    if no_auto_connect {
        Some(debug_log::NO_AUTO_CONNECT_VAR)
    } else {
        preview_play.map(|_| PREVIEW_PLAY_VAR)
    }
}

/// The first local output whose name holds `wanted`.
fn preview_play_output<'a>(
    speakers: &'a [ui_model::Speaker],
    wanted: &str,
) -> Option<&'a ui_model::Speaker> {
    use ui_model::SpeakerKind::{AirPlay, AirPlay2, Cast};
    speakers.iter().find(|speaker| {
        !matches!(speaker.kind, AirPlay | AirPlay2 | Cast) && speaker.name.contains(wanted)
    })
}

#[cfg(windows)]
fn quit_through_event_loop() {
    // Queued until the event loop runs, so a Ctrl-C before then still quits through it.
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

/// The quit steps for an end that may come before the event loop ends, or
/// instead of it (S351).
fn session_ending(
    app: &std::rc::Rc<view::App>,
    log: &DebugLog,
    what: &'static str,
) -> Box<dyn Fn()> {
    let app = std::rc::Rc::downgrade(app);
    let log = log.clone();
    Box::new(move || {
        let Some(app) = app.upgrade() else {
            return;
        };
        let saved = app.at_session_end();
        log.log(|| {
            format!(
                "{what}: settings {}",
                if saved { "saved" } else { "not saved" }
            )
        });
        log.flush(LOG_FLUSH_AT_EXIT);
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_preview_play_switch_works_only_with_the_debug_log_on() {
        let steam = || Some("Steam".to_owned());
        assert_eq!(super::preview_play_wanted(false, steam()), None);
        assert_eq!(super::preview_play_wanted(true, steam()), steam());
        assert_eq!(super::preview_play_wanted(true, Some(String::new())), None);
        assert_eq!(super::preview_play_wanted(true, None), None);
    }

    #[test]
    fn the_preview_play_switch_makes_a_launch_that_saves_nothing() {
        use super::{PREVIEW_PLAY_VAR, auto_connect_skipped_by, debug_log::NO_AUTO_CONNECT_VAR};
        assert_eq!(
            auto_connect_skipped_by(false, Some("Steam")),
            Some(PREVIEW_PLAY_VAR)
        );
        assert_eq!(
            auto_connect_skipped_by(true, Some("Steam")),
            Some(NO_AUTO_CONNECT_VAR)
        );
        assert_eq!(
            auto_connect_skipped_by(true, None),
            Some(NO_AUTO_CONNECT_VAR)
        );
        assert_eq!(auto_connect_skipped_by(false, None), None);
    }

    #[test]
    fn the_preview_play_switch_picks_the_first_local_output_holding_the_name() {
        use crate::ui_model::{Backend, MockBackend};
        let speakers = MockBackend::new().snapshot().speakers;
        let picked = |wanted| super::preview_play_output(&speakers, wanted).map(|s| s.id.as_str());
        assert_eq!(picked("Speaker"), Some("local:speakers"));
        assert_eq!(picked("Desk"), Some("local:desk"));
        assert_eq!(
            picked("Kitchen"),
            None,
            "an AirPlay speaker is not a local output"
        );
        assert_eq!(picked("Steam"), None);
    }

    /// Run on purpose with `cargo test deliberate_crash -- --ignored`: the
    /// process dies of an access violation and leaves a dump in
    /// target/crash-dumps.
    #[cfg(windows)]
    #[test]
    #[ignore = "crashes the test process on purpose"]
    fn deliberate_crash_writes_a_minidump() {
        unsafe { std::ptr::null_mut::<u8>().write_volatile(1) };
    }

    // covers: S414
    #[cfg(windows)]
    #[test]
    fn a_ctrl_c_before_the_event_loop_runs_ends_it_as_soon_as_it_starts() {
        use std::sync::mpsc;
        use std::time::Duration;

        // The app's own backend: it drops a quit sent before its loop runs.
        // Slint's proxy is per process, so a view test's quit can end this loop: a false pass, never a false fail.
        slint::BackendSelector::new()
            .backend_name("winit".to_owned())
            .select()
            .expect("the winit backend");
        super::quit_through_event_loop();
        let (ended, wait) = mpsc::channel::<()>();
        // Ends a loop the Ctrl-C left running, so the test fails instead of hanging.
        let watchdog = std::thread::spawn(move || {
            let late = wait.recv_timeout(Duration::from_secs(10)).is_err();
            if late {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
            late
        });
        slint::run_event_loop_until_quit().expect("the event loop runs");
        let _ = ended.send(());
        assert!(
            !watchdog.join().unwrap(),
            "the Ctrl-C ends the event loop once it starts"
        );
    }

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

    // covers: S186
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
            main.contains("help-menu := ContextMenuArea {")
                && main.contains("root.about-requested();"),
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
        let menu = &main[main.find("title-row := ").expect("the menus are there")..];
        let shortcuts = &main[main
            .find("public function shortcut(")
            .expect("the window runs the shortcuts")..];
        let mut seen = std::collections::BTreeSet::new();
        for (mac, title, keys, written) in MENU_SHORTCUTS {
            assert!(
                menu.contains(&format!(
                    "title: Strings.with-keys(Strings.{title}, \"{written}\");"
                )),
                "{mac}: the menu shows its item with {written}"
            );
            let key = match keys.strip_prefix("Control + ") {
                Some(letter) => format!("text == \"{}\"", letter.to_lowercase()),
                None => format!("text == Key.{keys}"),
            };
            assert!(shortcuts.contains(&key), "{mac}: {keys} runs it");
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
        let shown = menu.matches("Strings.with-keys(Strings.").count();
        assert_eq!(
            shown,
            MENU_SHORTCUTS.len(),
            "every menu shortcut is in the table"
        );
    }

    const CTRL_Q: &str = "if event.modifiers.control && !event.modifiers.alt && !event.modifiers.shift && !event.modifiers.meta && event.text.to-lowercase() == \"q\" {\n                root.exit-requested();";

    /// A checkout on Windows can have CRLF line endings; the snippets above
    /// are written with LF.
    fn lf(source: &str) -> String {
        source.replace("\r\n", "\n")
    }

    /// The windows without a menu bar: the equalizer, both Guides and About.
    fn windows_without_a_menu_bar() -> [(&'static str, &'static str); 4] {
        [
            ("equalizer", include_str!("../ui/equalizer.slint")),
            ("AudioSync Guide", include_str!("../ui/help.slint")),
            ("double-audio guide", include_str!("../ui/guide.slint")),
            ("About dialog", include_str!("../ui/about.slint")),
        ]
    }

    /// Command-Q quits from any window on the Mac, so Ctrl+Q does from the
    /// windows without a menu bar too.
    #[test]
    fn ctrl_q_exits_from_the_windows_without_a_menu_bar() {
        let view = include_str!("view.rs");
        for (name, source) in windows_without_a_menu_bar() {
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

    /// The kind icon after a flyout row's name switches the speaker only
    /// while the row is enabled, as a click on the rest of the row does: a
    /// zone's missing speaker stays as it is (S439).
    // covers: S439
    #[test]
    fn the_kind_icon_on_a_disabled_flyout_row_switches_nothing() {
        let flyout = include_str!("../ui/flyout.slint");
        let body = handler_body(flyout, "if root.show-kind: KindIcon {", "clicked");
        assert!(body.contains("if root.enabled {"), "{body}");
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

    /// While other notices wait behind the newest, Dismiss All sits beside
    /// the button for them, as in the Mac panel, and dismisses every notice.
    // covers: S153
    #[test]
    fn the_flyout_offers_dismiss_all_beside_the_more_notices_button() {
        let flyout = include_str!("../ui/flyout.slint");
        let row = &flyout[flyout
            .find("if root.notice-count > root.notices.length: HorizontalLayout {")
            .expect("the more-notices row is there")..];
        let row = &row[..row.find("Divider { }").expect("the row ends")];
        assert!(row.contains("text: Strings.dismiss-all-notices;"), "{row}");
        let body = handler_body(row, "text: Strings.dismiss-all-notices;", "clicked");
        assert!(body.contains("root.notices-dismissed-all();"), "{body}");
    }

    /// While the Google Cast search finds nothing, the flyout offers Add by
    /// Address… under the list, as the Mac panel does, and it opens the
    /// main window's dialog.
    // covers: S419
    #[test]
    fn the_flyout_offers_add_by_address_while_no_cast_speaker_is_found() {
        let flyout = include_str!("../ui/flyout.slint");
        assert!(flyout.contains(
            "if root.cast-by-address && root.cast-list == CastList.none-found: HorizontalLayout {"
        ));
        let body = handler_body(flyout, "text: Strings.cast-add-address;", "clicked");
        assert!(
            body.contains("root.cast-add-address-requested();"),
            "{body}"
        );

        let view = include_str!("view.rs");
        let wiring = &view[view
            .find("flyout.on_cast_add_address_requested(")
            .expect("the flyout's link is wired")..];
        let wiring = &wiring[..wiring.find("});").expect("the wiring ends")];
        assert!(wiring.contains("app.hide_flyout();"), "{wiring}");
        assert!(
            wiring.contains("app.main.invoke_cast_add_address_requested();"),
            "{wiring}"
        );
    }

    /// While AirPlay finds no speaker, the note in the main window and the
    /// flyout offers Open Network Settings, as the Mac offers Open Local
    /// Network Settings, and it opens Settings > Network & internet.
    // covers: S419
    #[test]
    fn the_no_speakers_note_offers_open_network_settings() {
        let main = include_str!("../ui/main.slint");
        assert!(main.contains(
            "show-network-settings: root.airplay-none-found && Platform.os != OperatingSystemType.linux;"
        ));
        let body = handler_body(main, "text: Strings.open-network-settings;", "clicked");
        assert!(body.contains("root.network-settings();"), "{body}");
        let body = handler_body(main, "show-network-settings: root.", "network-settings");
        assert!(
            body.contains("root.network-settings-requested();"),
            "{body}"
        );
        let flyout = include_str!("../ui/flyout.slint");
        let body = handler_body(flyout, "text: Strings.open-network-settings;", "clicked");
        assert!(
            body.contains("root.network-settings-requested();"),
            "{body}"
        );

        let view = include_str!("view.rs");
        for window in ["main", "flyout"] {
            assert!(view.contains(&format!(
                "{window}.on_network_settings_requested(move || with_app(&w, App::open_network_settings));"
            )));
        }
        let open = &view[view
            .find("fn open_network_settings(&self) {")
            .expect("it opens a page")..];
        assert!(
            open[..open.find('}').expect("it ends")]
                .contains("let uri = SettingsPage::NetworkStatus.uri();")
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
                "for speaker in root.this-pc: FlyoutRow {",
                "focus-changed(focused)",
                "speaker-focused",
            ),
            (
                "for speaker in root.speakers: FlyoutRow {",
                "focus-changed(focused)",
                "speaker-focused",
            ),
            (
                "text: Strings.cast-add-address;",
                "changed keyboard-focused",
                "add-address-focused",
            ),
            (
                "text: Strings.open-network-settings;",
                "changed keyboard-focused",
                "network-settings-focused",
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

    /// The flyout lists its speakers in the Mac panel's order, which is also
    /// the Tab order: this PC's under the master volume and above the
    /// divider, then the zones, then the connected speakers.
    #[test]
    fn the_flyout_lists_this_pc_under_the_volume_then_zones_then_connected() {
        let flyout = include_str!("../ui/flyout.slint");
        let at = |part: &str| flyout.find(part).unwrap_or_else(|| panic!("{part}"));
        let volume = at("MasterVolume {");
        let this_pc = at("for speaker in root.this-pc: FlyoutRow {");
        let divider = volume + flyout[volume..].find("Divider { }").expect("a divider");
        let zones = at("for zone in root.zones: VerticalLayout {");
        let connected = at("for speaker in root.speakers: FlyoutRow {");
        assert!(
            volume < this_pc && this_pc < divider,
            "this PC sits with the volume"
        );
        assert!(
            divider < zones && zones < connected,
            "zones, then connected"
        );
    }

    /// A click changes a switch or combo box before the engine answers; each
    /// handler sets the control back to the state the engine reports.
    #[test]
    fn switches_and_combo_boxes_show_the_engine_state_after_a_click() {
        for (file, source, pairs) in [
            (
                "main.slint",
                include_str!("../ui/main.slint"),
                &[("power := PowerSwitch", "self.checked = root.running;")][..],
            ),
            (
                "flyout.slint",
                include_str!("../ui/flyout.slint"),
                &[("power := PowerSwitch", "self.checked = root.running;")][..],
            ),
            (
                "equalizer.slint",
                include_str!("../ui/equalizer.slint"),
                &[
                    ("power := ToggleSwitch", "self.checked = root.enabled;"),
                    (
                        "presets := ComboBox",
                        "self.current-index = root.preset-index;",
                    ),
                ][..],
            ),
        ] {
            for &(element, reset) in pairs {
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
        // The flyout's source menu has no state of its own to reset: its
        // check mark follows an input-only index the engine sets.
        let flyout = include_str!("../ui/flyout.slint");
        let menu = &flyout[flyout
            .find("component SourceMenu")
            .expect("the source menu is there")..];
        assert!(
            menu.contains("in property <int> current;")
                && menu.contains("checked: index == root.current;")
                && flyout.contains("current: root.source-index;"),
            "flyout.slint: the source menu checks the source the engine reports"
        );
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
