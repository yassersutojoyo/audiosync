//! AudioSync for Windows 11 (in development).
//!
//! The Windows 11 check at startup, then the main window, the equalizer, the
//! tray icon with its flyout, and the About dialog, laid out like the macOS
//! app. The data comes through the `Backend` trait in `ui_model`; until the
//! WASAPI backend lands, a mock with example speakers stands in, so nothing
//! plays and no audio device is opened.

// No console window next to the app. Test builds keep the console so their
// output stays visible.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

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

/// Shows the main window and runs the UI until the user exits.
fn run() -> Result<(), slint::PlatformError> {
    let app = view::App::new(Box::new(ui_model::MockBackend::new()))?;

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
    slint::run_event_loop_until_quit()
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
