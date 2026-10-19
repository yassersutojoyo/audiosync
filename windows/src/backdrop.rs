//! Windows 11 window frames, set through the documented DWM attributes on
//! each window's HWND: the Mica material in the title bar of the main window
//! and the equalizer, and the system's rounded corners, border and shadow
//! around the frameless tray flyout.
//!
//! The client area stays opaque in the token background, which is close to
//! Mica's own base color, so the title bar and the content read as one
//! surface. (Mica in the client area too would need the renderer to keep the
//! scene's alpha, and the main window's native menu bar makes Slint create
//! that window opaque.)
//!
//! The backdrop attribute needs Windows 11 22H2 (build 22621) and the corner
//! preference Windows 11 itself. On an older build, or when DWM refuses an
//! attribute, the window keeps the plain system frame or, for the flyout, its
//! own drawn edge, so nothing looks broken.

use crate::os_version::{FIRST_WINDOWS_11_BUILD, OsVersion};

/// How a window's frame looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// A titled window (the main window, the equalizer): Mica in the title
    /// bar.
    Titled,
    /// The frameless tray flyout: rounded corners with the system border and
    /// shadow, like the Windows 11 flyouts.
    Flyout,
}

/// The first build with `DWMWA_SYSTEMBACKDROP_TYPE` (Windows 11 22H2).
pub const FIRST_BACKDROP_BUILD: u32 = 22621;

fn windows_build(build: u32) -> OsVersion {
    OsVersion {
        major: 10,
        minor: 0,
        build,
    }
}

/// Whether `version` offers the system backdrop attribute.
pub fn supports_backdrop(version: OsVersion) -> bool {
    version >= windows_build(FIRST_BACKDROP_BUILD)
}

/// Whether `version` offers the corner preference attribute.
pub fn supports_corners(version: OsVersion) -> bool {
    version >= windows_build(FIRST_WINDOWS_11_BUILD)
}

/// Gives `window` its Windows 11 frame. `Some(true)` when DWM took it: for
/// the flyout, the window may then leave out its own edge. `None` while the
/// native window doesn't exist yet (Slint creates it once the event loop
/// runs), so the caller tries again later; repeating it is harmless.
#[cfg(windows)]
pub fn apply(window: &slint::Window, frame: Frame) -> Option<bool> {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;

    let supported = crate::platform::windows_version().is_some_and(|version| match frame {
        Frame::Titled => supports_backdrop(version),
        Frame::Flyout => supports_corners(version),
    });
    window.with_winit_window(|window| {
        if !supported {
            return false;
        }
        let Ok(handle) = window.window_handle() else {
            return false;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return false;
        };
        let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
        set_frame(hwnd, frame)
    })
}

#[cfg(not(windows))]
pub fn apply(_window: &slint::Window, _frame: Frame) -> Option<bool> {
    Some(false)
}

#[cfg(windows)]
fn set_frame(hwnd: windows::Win32::Foundation::HWND, frame: Frame) -> bool {
    use windows::Win32::Graphics::Dwm::{
        DWM_SYSTEMBACKDROP_TYPE, DWM_WINDOW_CORNER_PREFERENCE, DWMSBT_MAINWINDOW,
        DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        DwmSetWindowAttribute,
    };

    // SAFETY: `hwnd` is the live window winit created on this (the UI)
    // thread, and each value is a 4-byte DWM enum that lives through the
    // call and is read for exactly its size.
    unsafe {
        match frame {
            Frame::Titled => {
                let backdrop: DWM_SYSTEMBACKDROP_TYPE = DWMSBT_MAINWINDOW;
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    (&raw const backdrop).cast(),
                    size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
                )
            }
            Frame::Flyout => {
                let corners: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_WINDOW_CORNER_PREFERENCE,
                    (&raw const corners).cast(),
                    size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
                )
            }
        }
        .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_11_21h2_has_corners_but_no_backdrop() {
        let first = windows_build(FIRST_WINDOWS_11_BUILD);
        assert!(supports_corners(first));
        assert!(!supports_backdrop(first));
        assert!(!supports_backdrop(windows_build(FIRST_BACKDROP_BUILD - 1)));
    }

    #[test]
    fn windows_11_22h2_and_later_get_the_backdrop() {
        assert!(supports_backdrop(windows_build(FIRST_BACKDROP_BUILD)));
        assert!(supports_backdrop(windows_build(26100)));
    }

    #[test]
    fn windows_10_gets_neither() {
        let windows_10 = windows_build(19045);
        assert!(!supports_corners(windows_10));
        assert!(!supports_backdrop(windows_10));
    }
}
