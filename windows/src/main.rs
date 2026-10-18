//! AudioSync for Windows 11 (in development).
//!
//! For now this is the application shell: the Windows 11 check at startup,
//! the main window with its menu, and the About dialog. The audio work will
//! come from `audiosync-core`, which the macOS app shares; so far only its
//! version shows, in the About dialog.

// No console window next to the app. Test builds keep the console so their
// output stays visible.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod os_version;
mod platform;
// Generated from brand/tokens.json: rustfmt leaves it as the generator wrote it.
#[cfg(test)]
#[rustfmt::skip]
mod tokens;

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

/// Shows the main window and runs the UI until it closes.
fn run() -> Result<(), slint::PlatformError> {
    let main_window = MainWindow::new()?;
    let about = AboutDialog::new()?;
    about.set_version(env!("CARGO_PKG_VERSION").into());
    about.set_core_version(audiosync_core::VERSION.into());

    let about_handle = about.as_weak();
    main_window.on_about_requested(move || {
        if let Some(about) = about_handle.upgrade()
            && let Err(error) = about.show()
        {
            platform::show_error(&format!("The About window could not open: {error}"));
        }
    });

    let about_handle = about.as_weak();
    about.on_ok_clicked(move || {
        if let Some(about) = about_handle.upgrade() {
            // A window that fails to hide stays open; the user can still close it.
            let _ = about.hide();
        }
    });

    // Closing the main window quits the app, even while the About dialog is
    // open.
    main_window.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });

    main_window.run()
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
