//! The few calls that differ between Windows and the hosts the UI is
//! developed on. Only the Windows versions ship; elsewhere the app skips the
//! version check and reports errors on stderr.

use crate::os_version::OsVersion;

/// The version of the running Windows, or `None` when it can't be read (and
/// on other hosts).
///
/// Uses `RtlGetVersion`: `GetVersionEx` reports whatever the application
/// manifest declares support for, not the real version.
#[cfg(windows)]
pub fn windows_version() -> Option<OsVersion> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid, writable OSVERSIONINFOW whose size field is
    // set, which is all RtlGetVersion needs.
    let status = unsafe { RtlGetVersion(&mut info) };
    status.is_ok().then_some(OsVersion {
        major: info.dwMajorVersion,
        minor: info.dwMinorVersion,
        build: info.dwBuildNumber,
    })
}

#[cfg(not(windows))]
pub fn windows_version() -> Option<OsVersion> {
    None
}

/// Tells the user why the app stops: a message box on Windows, stderr
/// elsewhere.
#[cfg(windows)]
pub fn show_error(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::{HSTRING, w};

    let text = HSTRING::from(text);
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call,
    // and the message box has no owner window.
    unsafe {
        MessageBoxW(None, &text, w!("AudioSync"), MB_OK | MB_ICONERROR);
    }
}

#[cfg(not(windows))]
pub fn show_error(text: &str) {
    eprintln!("{text}");
}
