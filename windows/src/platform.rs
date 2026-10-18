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

/// Brings a window that is showing to the front and gives it the keyboard
/// focus.
pub fn bring_to_front(window: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    window.with_winit_window(|window| window.focus_window());
}

/// Whether the taskbar (and the notification area) uses the light theme, so
/// the tray icon picks its dark-on-light variant. Dark when it can't be read,
/// the Windows 11 default.
#[cfg(windows)]
pub fn taskbar_is_light() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;

    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` and `size` are valid for writes of a DWORD, which is
    // what RRF_RT_REG_DWORD limits the result to; the key and value names
    // are NUL-terminated UTF-16 literals.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&raw mut size),
        )
    };
    status.is_ok() && value != 0
}

/// The work area (the monitor minus the taskbar) of the monitor at a point,
/// in physical pixels.
#[cfg(windows)]
pub fn work_area_at(x: f64, y: f64) -> Option<crate::placement::Bounds> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };

    let point = POINT {
        x: x.round() as i32,
        y: y.round() as i32,
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: MonitorFromPoint takes the point by value; `info` is a valid,
    // writable MONITORINFO whose size field is set.
    let found = unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, &mut info)
    };
    let work = info.rcWork;
    found.as_bool().then(|| crate::placement::Bounds {
        x: f64::from(work.left),
        y: f64::from(work.top),
        width: f64::from(work.right - work.left),
        height: f64::from(work.bottom - work.top),
    })
}
