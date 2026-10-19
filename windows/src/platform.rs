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

/// The system the app runs on, for the debug log: the Windows version with
/// its update revision and release name, the app's architecture and, when
/// it differs, the PC's, and the local time, which tells how the log's UTC
/// times map to the PC's clock.
#[cfg(windows)]
pub fn system_summary() -> String {
    use windows::core::w;

    let version = windows_version().map_or_else(
        || "Windows, version unknown".to_owned(),
        |v| format!("Windows {}.{}.{}", v.major, v.minor, v.build),
    );
    let revision = registry_u32(
        w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
        w!("UBR"),
    )
    .map_or_else(String::new, |revision| format!(".{revision}"));
    let release = registry_string(
        w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
        w!("DisplayVersion"),
    )
    .map_or_else(String::new, |name| format!(" ({name})"));
    let arch = std::env::consts::ARCH;
    let machine = match native_machine() {
        Some(native) if native != arch => format!("{arch} build, running on {native}"),
        _ => arch.to_owned(),
    };
    format!(
        "{version}{revision}{release}, {machine}, local time {}",
        notice_time()
    )
}

#[cfg(not(windows))]
pub fn system_summary() -> String {
    format!(
        "{} on {}, not Windows: example speakers only",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// The PC's own architecture, by the names Rust gives them, even when the
/// app runs emulated (an x64 build on an ARM64 PC); `None` when it can't be
/// read or has no such name.
#[cfg(windows)]
fn native_machine() -> Option<&'static str> {
    use windows::Win32::System::SystemInformation::{
        IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
        IMAGE_FILE_MACHINE_I386,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};

    let mut process = IMAGE_FILE_MACHINE::default();
    let mut native = IMAGE_FILE_MACHINE::default();
    // SAFETY: both pointers are valid for writes of an IMAGE_FILE_MACHINE;
    // the current process's pseudo handle needs no closing.
    unsafe { IsWow64Process2(GetCurrentProcess(), &raw mut process, Some(&raw mut native)) }
        .ok()?;
    if native == IMAGE_FILE_MACHINE_AMD64 {
        Some("x86_64")
    } else if native == IMAGE_FILE_MACHINE_ARM64 {
        Some("aarch64")
    } else if native == IMAGE_FILE_MACHINE_I386 {
        Some("x86")
    } else {
        None
    }
}

/// A DWORD value of the key `key` under `HKEY_LOCAL_MACHINE`.
#[cfg(windows)]
fn registry_u32(key: windows::core::PCWSTR, name: windows::core::PCWSTR) -> Option<u32> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};

    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` and `size` are valid for writes of a DWORD, which is
    // what RRF_RT_REG_DWORD limits the result to; the key and value names
    // are NUL-terminated UTF-16 literals.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&raw mut size),
        )
    };
    status.is_ok().then_some(value)
}

/// A short string value of the key `key` under `HKEY_LOCAL_MACHINE`; `None`
/// when it is missing or longer than 63 characters.
#[cfg(windows)]
fn registry_string(key: windows::core::PCWSTR, name: windows::core::PCWSTR) -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

    let mut text = [0u16; 64];
    let mut size = size_of_val(&text) as u32;
    // SAFETY: `text` is valid for writes of `size` bytes, and RegGetValueW
    // writes no more (it fails when the value doesn't fit) and terminates
    // the string; the key and value names are NUL-terminated UTF-16
    // literals.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            name,
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    // The size is in bytes and counts the terminating NUL.
    let length = (size as usize / 2).saturating_sub(1).min(text.len());
    Some(String::from_utf16_lossy(&text[..length]))
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

/// Opens a Settings page by its `ms-settings:` URI, through the shell the
/// way the Settings app documents for launching its pages. Returns whether
/// the shell took it. Off Windows, it only says which page it would open.
#[cfg(windows)]
pub fn open_settings(uri: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    let uri = HSTRING::from(uri);
    // SAFETY: the operation and the URI are NUL-terminated UTF-16 that
    // outlive the call; no owner window, parameters or directory.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &uri,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success (ShellExecuteW's documented return).
    result.0 as usize > 32
}

#[cfg(not(windows))]
pub fn open_settings(uri: &str) -> bool {
    eprintln!("would open {uri}");
    false
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
    monitor_at(x, y).map(|(work_area, _)| work_area)
}

/// The work area of the monitor at a point (or the nearest one), in physical
/// pixels, and that monitor's scale factor (1.0 at 96 DPI).
#[cfg(windows)]
pub fn monitor_at(x: f64, y: f64) -> Option<(crate::placement::Bounds, f64)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    let point = POINT {
        x: x.round() as i32,
        y: y.round() as i32,
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    // SAFETY: MonitorFromPoint takes the point by value; `info` is a valid,
    // writable MONITORINFO whose size field is set, and the DPI outputs are
    // valid, writable u32s.
    let (found, dpi) = unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let found = GetMonitorInfoW(monitor, &mut info);
        let dpi = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        (found, dpi)
    };
    let scale = if dpi.is_ok() && dpi_x > 0 {
        f64::from(dpi_x) / 96.0
    } else {
        1.0
    };
    let work = info.rcWork;
    found.as_bool().then(|| {
        let work_area = crate::placement::Bounds {
            x: f64::from(work.left),
            y: f64::from(work.top),
            width: f64::from(work.right - work.left),
            height: f64::from(work.bottom - work.top),
        };
        (work_area, scale)
    })
}

/// Elsewhere the window manager places windows itself.
#[cfg(not(windows))]
pub fn monitor_at(_x: f64, _y: f64) -> Option<(crate::placement::Bounds, f64)> {
    None
}

/// Whether a window whose outer frame starts at `(x, y)`, in physical
/// pixels, would show its title bar on a monitor: a saved place on a monitor
/// that is gone would open the window out of sight.
#[cfg(windows)]
pub fn is_on_a_monitor(x: i32, y: i32) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};

    // The left part of the title bar, where the user can grab it.
    let title_bar = RECT {
        left: x,
        top: y,
        right: x.saturating_add(200),
        bottom: y.saturating_add(40),
    };
    // SAFETY: MonitorFromRect only reads the rectangle.
    let monitor = unsafe { MonitorFromRect(&title_bar, MONITOR_DEFAULTTONULL) };
    !monitor.is_invalid()
}

/// Elsewhere the window manager places windows itself.
#[cfg(not(windows))]
pub fn is_on_a_monitor(_x: i32, _y: i32) -> bool {
    false
}
