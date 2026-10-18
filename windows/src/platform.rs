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

/// The local time now, in the user's short time format ("09.40", "9:40 AM"):
/// when a notice was posted. Hours and minutes when the format can't be read.
#[cfg(windows)]
pub fn notice_time() -> String {
    use windows::Win32::Globalization::{GetTimeFormatEx, TIME_NOSECONDS};
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::core::PCWSTR;

    let mut text = [0u16; 64];
    // SAFETY: formats the current local time (no time given) in the user's
    // locale and format (no locale name, no format) into `text`, whose length
    // the call is given.
    let written = unsafe {
        GetTimeFormatEx(
            PCWSTR::null(),
            TIME_NOSECONDS,
            None,
            PCWSTR::null(),
            Some(&mut text),
        )
    };
    // The count includes the terminating NUL; 0 means it failed.
    match usize::try_from(written) {
        Ok(length) if length > 1 => String::from_utf16_lossy(&text[..length - 1]),
        _ => {
            // SAFETY: only reads the clock.
            let now = unsafe { GetLocalTime() };
            format!("{:02}.{:02}", now.wHour, now.wMinute)
        }
    }
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

/// The small icon size (`SM_CXSMICON` by `SM_CYSMICON`) at the system DPI,
/// which the notification area uses: 16 px at 100% scaling, 20 at 125%, 24
/// at 150%. `None` when it can't be read, to let Windows pick.
#[cfg(windows)]
pub fn small_icon_size() -> Option<(u32, u32)> {
    use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
    use windows::Win32::UI::WindowsAndMessaging::{SM_CXSMICON, SM_CYSMICON};

    // SAFETY: both calls only read system settings.
    let (width, height) = unsafe {
        let dpi = GetDpiForSystem();
        (
            GetSystemMetricsForDpi(SM_CXSMICON, dpi),
            GetSystemMetricsForDpi(SM_CYSMICON, dpi),
        )
    };
    let width = u32::try_from(width).ok().filter(|&w| w > 0)?;
    let height = u32::try_from(height).ok().filter(|&h| h > 0)?;
    Some((width, height))
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
