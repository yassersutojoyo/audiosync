//! The few calls that differ between Windows and the hosts the UI is
//! developed on. Only the Windows versions ship; elsewhere the app skips the
//! version check and reports errors on stderr.

use crate::debug_log::DebugLog;
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

/// The double-click time the user set in Windows, in milliseconds.
#[cfg(windows)]
pub fn double_click_ms() -> i64 {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;

    // SAFETY: GetDoubleClickTime takes no arguments and only reads a setting.
    double_click_or_default(unsafe { GetDoubleClickTime() })
}

#[cfg(not(windows))]
pub fn double_click_ms() -> i64 {
    DEFAULT_DOUBLE_CLICK_MS
}

/// Whether Windows' Animation effects (Settings > Accessibility > Visual
/// effects) are on; on when the setting can't be read.
#[cfg(windows)]
pub fn animation_effects() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    use windows::core::BOOL;

    let mut on = BOOL(1);
    // SAFETY: `on` is a writable BOOL, which this setting's read fills in.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(std::ptr::from_mut(&mut on).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_err() || on.as_bool()
}

/// Windows' own default double-click time.
pub const DEFAULT_DOUBLE_CLICK_MS: i64 = 500;

/// Windows caps the setting at 5000 ms; 0 would make every click a single one.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn double_click_or_default(ms: u32) -> i64 {
    if ms == 0 {
        DEFAULT_DOUBLE_CLICK_MS
    } else {
        i64::from(ms.min(5000))
    }
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

/// The system the app runs on, for the debug log: the distribution named
/// by `/etc/os-release`, the kernel release, the architecture and the
/// local time.
#[cfg(target_os = "linux")]
pub fn system_summary() -> String {
    let os_release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok();
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    linux_summary(
        os_release.as_deref(),
        kernel.as_deref(),
        std::env::consts::ARCH,
        &notice_time(),
    )
}

/// Hours and minutes at `seconds` since the Unix epoch, on a clock
/// `utc_offset` seconds ahead of UTC.
#[cfg_attr(windows, allow(dead_code))]
pub fn clock_at(seconds: i64, utc_offset: i64) -> String {
    let minutes = (seconds + utc_offset).div_euclid(60).rem_euclid(24 * 60);
    format!("{:02}.{:02}", minutes / 60, minutes % 60)
}

/// The Linux line of the debug log, from the text of `os-release`, the
/// kernel release, the architecture and the local time.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn linux_summary(
    os_release: Option<&str>,
    kernel: Option<&str>,
    arch: &str,
    local_time: &str,
) -> String {
    let name = os_release
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        })
        .map(|value| value.trim().trim_matches(['"', '\'']))
        .filter(|name| !name.is_empty())
        .unwrap_or("Linux");
    let kernel = kernel.map_or_else(
        || "version unknown".to_owned(),
        |kernel| format!("kernel {}", kernel.trim()),
    );
    format!("{name}, {kernel}, {arch}, local time {local_time}")
}

/// The host's name as the debug log's first line gives it, from Rust's
/// name for the OS.
pub fn host_name(os: &str) -> &str {
    match os {
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
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
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    };
    use windows::core::{HSTRING, w};

    let text = HSTRING::from(text);
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call,
    // and the message box has no owner window.
    unsafe {
        MessageBoxW(
            None,
            &text,
            w!("AudioSync"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
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

#[cfg(not(any(windows, target_os = "linux")))]
pub fn open_settings(uri: &str) -> bool {
    eprintln!("would open {uri}");
    false
}

/// Whether a guide step's button for the page at `uri` can do anything:
/// on Linux only when a program that shows it is installed. Tests see every
/// page as available, whatever the machine running them has installed.
#[cfg(any(not(target_os = "linux"), test))]
pub fn can_open_settings(_uri: &str) -> bool {
    true
}

#[cfg(all(target_os = "linux", not(test)))]
pub fn can_open_settings(uri: &str) -> bool {
    linux_settings_program(uri).is_some()
}

/// The program on Linux that shows the page a Windows settings URI names,
/// for this desktop ([`crate::guide::linux_program`]).
#[cfg(target_os = "linux")]
fn linux_settings_program(uri: &str) -> Option<(&'static str, &'static [&'static str])> {
    use crate::guide::SettingsPage;

    let page = [
        SettingsPage::VolumeMixer,
        SettingsPage::Sound,
        SettingsPage::MicrophonePrivacy,
    ]
    .into_iter()
    .find(|page| page.uri() == uri)?;
    let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    crate::guide::linux_program(page, &desktops, |program| {
        std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
    })
}

/// Starts the program that shows the page `uri` names, without waiting for
/// it; false when none is installed or it couldn't start.
#[cfg(target_os = "linux")]
pub fn open_settings(uri: &str) -> bool {
    let Some((program, args)) = linux_settings_program(uri) else {
        return false;
    };
    match std::process::Command::new(program).args(args).spawn() {
        Ok(mut child) => {
            // Reaped when it exits, so it leaves no zombie behind.
            std::thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
    }
}

pub use crate::os::notice_time;

/// The smallest outer size, in physical pixels, of a window whose inner size
/// may not go below `min` logical pixels at `dpi`, with a frame (borders,
/// title and menu bar) of `frame` physical pixels.
#[cfg(any(windows, test))]
pub fn min_track_size(min: (f64, f64), dpi: u32, frame: (i32, i32)) -> (i32, i32) {
    let scale = f64::from(dpi) / 96.0;
    let side = |logical: f64, frame: i32| (logical * scale).ceil() as i32 + frame;
    (side(min.0, frame.0), side(min.1, frame.1))
}

/// The outer size a move or resize that asked for `asked` gets, between the
/// smallest outer size `min` and the largest `max` (physical pixels); `min`
/// wins when the screen is too small for it.
#[cfg(any(windows, test))]
pub fn held_size(asked: (i32, i32), min: (i32, i32), max: (i32, i32)) -> (i32, i32) {
    (asked.0.min(max.0).max(min.0), asked.1.min(max.1).max(min.1))
}

/// The largest outer size along one side, from the tracking size metric in
/// physical pixels (`system`) and the same metric for the window's DPI
/// (`for_dpi`), which Windows rescales from the system DPI, so it comes out
/// short on a monitor scaled below the primary one. 0 means unreadable; with
/// neither, there is no largest size.
#[cfg(any(windows, test))]
pub fn largest_track_size(system: i32, for_dpi: i32) -> i32 {
    match system.max(for_dpi) {
        size if size > 0 => size,
        _ => i32::MAX,
    }
}

/// `min` (logical pixels, each below 65 536) in the one number a window
/// subclass carries, and back.
#[cfg(any(windows, test))]
fn pack_min_size(min: (f64, f64)) -> usize {
    let side = |logical: f64| logical.round().clamp(0.0, 65_535.0) as usize;
    (side(min.0) << 16) | side(min.1)
}

#[cfg(any(windows, test))]
fn unpack_min_size(packed: usize) -> (f64, f64) {
    ((packed >> 16) as f64, (packed & 0xffff) as f64)
}

/// Holds `window` at an inner size of at least `min` logical pixels, also
/// against another program's MoveWindow or SetWindowPos: Windows asks the
/// window for its smallest size (WM_GETMINMAXINFO) on every resize, and a
/// subclass answers after winit, whose minimum Slint resets when its layout
/// changes. A move also stops at the largest size Windows allows a window,
/// as for any other window. `false` while the native window doesn't exist
/// yet; repeating it only updates the size.
#[cfg(windows)]
pub fn hold_min_size(window: &slint::Window, min: (f64, f64)) -> bool {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::SetWindowSubclass;

    window
        .with_winit_window(|window| {
            let Ok(handle) = window.window_handle() else {
                return false;
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return false;
            };
            let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
            // SAFETY: `hwnd` is this thread's live window, and the procedure
            // only touches the structures its two messages point to.
            let held = unsafe {
                SetWindowSubclass(
                    hwnd,
                    Some(min_size_procedure),
                    MIN_SIZE_SUBCLASS,
                    pack_min_size(min),
                )
            }
            .as_bool();
            log_min_size_once(MinSizeEvent::Installed, || {
                if held {
                    format!("main window: minimum {} x {} held natively", min.0, min.1)
                } else {
                    "main window: the minimum size subclass did not install".to_owned()
                }
            });
            held
        })
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn hold_min_size(_window: &slint::Window, _min: (f64, f64)) -> bool {
    false
}

/// Gives the window subclass behind `hold_min_size` the debug log, so a
/// launch shows whether it installed and which messages reach it.
pub fn set_min_size_log(log: DebugLog) {
    MIN_SIZE_LOG.with(|slot| *slot.borrow_mut() = Some(log));
}

#[derive(Clone, Copy)]
#[cfg(windows)]
enum MinSizeEvent {
    Installed = 1,
    MinMaxInfo = 2,
    Clamped = 4,
}

thread_local! {
    // A window procedure takes no context of ours; it runs on this thread.
    static MIN_SIZE_LOG: std::cell::RefCell<Option<DebugLog>> = const { std::cell::RefCell::new(None) };
    static MIN_SIZE_LOGGED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// Logs `event` the first time it happens on this thread.
#[cfg(windows)]
fn log_min_size_once(event: MinSizeEvent, message: impl FnOnce() -> String) {
    let bit = event as u8;
    if MIN_SIZE_LOGGED.with(|logged| logged.replace(logged.get() | bit)) & bit != 0 {
        return;
    }
    MIN_SIZE_LOG.with(|slot| {
        if let Some(log) = slot.borrow().as_ref() {
            log.log(message);
        }
    });
}

#[cfg(windows)]
const MIN_SIZE_SUBCLASS: usize = 0x4153_4d4e;

#[cfg(windows)]
unsafe extern "system" fn min_size_procedure(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _id: usize,
    packed: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::HiDpi::{
        AdjustWindowRectExForDpi, GetDpiForWindow, GetSystemMetricsForDpi,
    };
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWL_STYLE, GetMenu, GetSystemMetrics, GetWindowLongW, IsIconic, IsZoomed,
        MINMAXINFO, SM_CXMAXTRACK, SM_CYMAXTRACK, SWP_NOSIZE, WINDOW_EX_STYLE, WINDOW_STYLE,
        WINDOWPOS, WM_GETMINMAXINFO, WM_NCDESTROY, WM_WINDOWPOSCHANGING,
    };

    if message == WM_NCDESTROY {
        // SAFETY: removes this procedure from the window being destroyed, as
        // the comctl32 subclass documentation asks.
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(min_size_procedure), MIN_SIZE_SUBCLASS) };
    }
    // SAFETY: passes the message on unchanged, as a subclass must.
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    if (message != WM_GETMINMAXINFO && message != WM_WINDOWPOSCHANGING) || lparam.0 == 0 {
        return result;
    }
    // SAFETY: `hwnd` is the live window this message is for.
    let (dpi, style, ex_style, has_menu, minimized, maximized) = unsafe {
        (
            GetDpiForWindow(hwnd),
            WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32),
            WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32),
            !GetMenu(hwnd).is_invalid(),
            IsIconic(hwnd).as_bool(),
            IsZoomed(hwnd).as_bool(),
        )
    };
    let mut rect = RECT::default();
    // With a menu this counts one menu row; a menu wrapped to two rows is not measured.
    // SAFETY: `rect` is a valid, writable RECT.
    if unsafe { AdjustWindowRectExForDpi(&mut rect, style, has_menu, ex_style, dpi) }.is_err() {
        return result;
    }
    let frame = (rect.right - rect.left, rect.bottom - rect.top);
    let (width, height) = min_track_size(unpack_min_size(packed), dpi, frame);
    if message == WM_GETMINMAXINFO {
        log_min_size_once(MinSizeEvent::MinMaxInfo, || {
            format!("main window: WM_GETMINMAXINFO arrived, minimum {width} x {height} px")
        });
        // SAFETY: for WM_GETMINMAXINFO, `lparam` points to the MINMAXINFO
        // that Windows reads back once the message returns.
        let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
        info.ptMinTrackSize.x = info.ptMinTrackSize.x.max(width);
        info.ptMinTrackSize.y = info.ptMinTrackSize.y.max(height);
        return result;
    }
    // winit answers WM_WINDOWPOSCHANGING without DefWindowProc, which would
    // ask for WM_GETMINMAXINFO and keep the size between the tracking sizes,
    // so a move could make the window any size. Minimizing shrinks it on purpose.
    // SAFETY: for WM_WINDOWPOSCHANGING, `lparam` points to the WINDOWPOS
    // that Windows applies once the message returns.
    let position = unsafe { &mut *(lparam.0 as *mut WINDOWPOS) };
    if minimized || maximized || position.flags.contains(SWP_NOSIZE) {
        return result;
    }
    // SAFETY: both calls only read a system setting.
    let metric = |index| unsafe {
        largest_track_size(GetSystemMetrics(index), GetSystemMetricsForDpi(index, dpi))
    };
    let largest = (metric(SM_CXMAXTRACK), metric(SM_CYMAXTRACK));
    let clamped = held_size((position.cx, position.cy), (width, height), largest);
    if clamped != (position.cx, position.cy) {
        let asked = (position.cx, position.cy);
        log_min_size_once(MinSizeEvent::Clamped, || {
            format!(
                "main window: a move to {} x {} px kept at {} x {} px",
                asked.0, asked.1, clamped.0, clamped.1
            )
        });
        (position.cx, position.cy) = clamped;
    }
    result
}

/// Brings a window that is showing to the front and gives it the keyboard
/// focus, restoring it first when it is minimized (Windows doesn't focus a
/// minimized window).
pub fn bring_to_front(window: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    window.with_winit_window(|window| {
        if window.is_minimized() == Some(true) {
            window.set_minimized(false);
        }
        window.focus_window();
    });
}

/// muda 0.19.3's private `MENU_UPDATE_THEME`: draws a window's menu bar dark
/// (0) or light (1) and repaints its frame. Slint asks for muda ^0.19, so only
/// Cargo.lock holds the version (a test checks it). Slint sends it only when
/// Windows changes theme, so View > Appearance sends it too.
#[cfg(windows)]
const MUDA_MENU_BAR_THEME: u32 = 201;

/// Repaints a window's frame after its title bar turned light or dark, and
/// with `menu_bar` sets the menu bar to `dark` as well.
#[cfg(windows)]
pub fn repaint_frame(window: &slint::Window, menu_bar: bool, dark: bool) {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, PostMessageW, SendMessageW, WM_NCACTIVATE,
    };

    window.with_winit_window(|window| {
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
        // SAFETY: `hwnd` is this thread's live window. The menu bar message
        // goes only to the main window, whose menu bar muda subclassed; the
        // frame is redrawn by toggling the active caption and back, which
        // leaves the focus where it is.
        unsafe {
            if menu_bar {
                SendMessageW(
                    hwnd,
                    MUDA_MENU_BAR_THEME,
                    Some(WPARAM(0)),
                    Some(LPARAM(if dark { 0 } else { 1 })),
                );
                return;
            }
            let active = GetForegroundWindow() == hwnd;
            let _ = PostMessageW(
                Some(hwnd),
                WM_NCACTIVATE,
                WPARAM(usize::from(!active)),
                LPARAM(0),
            );
            let _ = PostMessageW(
                Some(hwnd),
                WM_NCACTIVATE,
                WPARAM(usize::from(active)),
                LPARAM(0),
            );
        }
    });
}

#[cfg(not(windows))]
pub fn repaint_frame(_window: &slint::Window, _menu_bar: bool, _dark: bool) {}

/// The handle of `window`'s native window, to tell it from one created after
/// it; `None` while it has none.
#[cfg(windows)]
pub fn native_id(window: &slint::Window) -> Option<isize> {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    window
        .with_winit_window(|window| match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
            _ => None,
        })
        .flatten()
}

#[cfg(not(windows))]
pub fn native_id(_window: &slint::Window) -> Option<isize> {
    None
}

/// Whether a WM_SETTINGCHANGE names `area` "ImmersiveColorSet", the one
/// Windows sends when it switches between light and dark.
#[cfg(any(windows, test))]
pub fn names_color_set(area: &[u16]) -> bool {
    area.iter().copied().eq("ImmersiveColorSet".encode_utf16())
}

/// Runs `changed` once, on the next turn of the event loop, after Windows
/// switches between light and dark, as seen by any window `follow_color_set`
/// watches. winit reports that switch only for a window that follows the
/// Windows theme, and leaves one set to Light or Dark alone.
pub fn set_on_color_set_change(changed: std::rc::Rc<dyn Fn()>) {
    COLOR_SET_CHANGED.with(|slot| *slot.borrow_mut() = Some(changed));
}

thread_local! {
    static COLOR_SET_CHANGED: std::cell::RefCell<Option<std::rc::Rc<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
    // Windows broadcasts the change to every top-level window.
    static COLOR_SET_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Watches `window` for the light and dark switch (`set_on_color_set_change`).
/// `false` while the native window doesn't exist yet; repeating it is harmless.
#[cfg(windows)]
pub fn follow_color_set(window: &slint::Window) -> bool {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::SetWindowSubclass;

    window
        .with_winit_window(|window| {
            let Ok(handle) = window.window_handle() else {
                return false;
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return false;
            };
            let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
            // SAFETY: `hwnd` is this thread's live window, and the procedure
            // only reads the string WM_SETTINGCHANGE points to.
            unsafe { SetWindowSubclass(hwnd, Some(color_set_procedure), COLOR_SET_SUBCLASS, 0) }
                .as_bool()
        })
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn follow_color_set(_window: &slint::Window) -> bool {
    false
}

#[cfg(windows)]
const COLOR_SET_SUBCLASS: usize = 0x4153_4353;

#[cfg(windows)]
unsafe extern "system" fn color_set_procedure(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _id: usize,
    _data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{WM_NCDESTROY, WM_SETTINGCHANGE};
    use windows::core::PCWSTR;

    if message == WM_NCDESTROY {
        // SAFETY: removes this procedure from the window being destroyed, as
        // the comctl32 subclass documentation asks.
        let _ =
            unsafe { RemoveWindowSubclass(hwnd, Some(color_set_procedure), COLOR_SET_SUBCLASS) };
    }
    // SAFETY: passes the message on unchanged, as a subclass must.
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    if message != WM_SETTINGCHANGE || lparam.0 == 0 {
        return result;
    }
    let area = PCWSTR(lparam.0 as *const u16);
    // SAFETY: a WM_SETTINGCHANGE `lparam` that isn't null points to the
    // NUL-terminated name of the setting area that changed.
    let color_set = names_color_set(unsafe { area.as_wide() });
    if color_set && !COLOR_SET_PENDING.replace(true) {
        slint::Timer::single_shot(std::time::Duration::ZERO, || {
            COLOR_SET_PENDING.set(false);
            let changed = COLOR_SET_CHANGED.with(|slot| slot.borrow().clone());
            if let Some(changed) = changed {
                changed();
            }
        });
    }
    result
}

/// Runs `ending` on this thread when Windows ends the session (sign-out,
/// shutdown or restart), which may end the process as soon as it returns.
/// Returns the hidden window that receives that message, `None` without one.
#[cfg(windows)]
pub fn on_session_end(ending: Box<dyn Fn()>) -> Option<isize> {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
    };
    use windows::core::w;

    SESSION_ENDING.with(|slot| *slot.borrow_mut() = Some(ending));
    let class = WNDCLASSW {
        lpfnWndProc: Some(session_end_procedure),
        lpszClassName: w!("AudioSyncSessionEnd"),
        ..Default::default()
    };
    // SAFETY: the class names a procedure and a NUL-terminated literal that
    // both live for the whole process; a second registration only fails.
    unsafe { RegisterClassW(&class) };
    // A hidden top-level window, not a message-only one: those miss the
    // end-of-session broadcast.
    // SAFETY: the class was registered above, and the window takes no
    // parent, menu or creation data.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("AudioSyncSessionEnd"),
            w!("AudioSync"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    }
    .ok()?;
    Some(hwnd.0 as isize)
}

#[cfg(windows)]
thread_local! {
    static SESSION_ENDING: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(windows)]
unsafe extern "system" fn session_end_procedure(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_ENDSESSION};

    // A zero `wparam` means the end was called off.
    if message == WM_ENDSESSION && wparam.0 != 0 {
        let ending = SESSION_ENDING.with(|slot| slot.borrow_mut().take());
        if let Some(ending) = ending {
            ending();
        }
        return LRESULT(0);
    }
    // SAFETY: passes every other message on unchanged; WM_QUERYENDSESSION
    // then answers that the session may end.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_double_click_time_follows_windows_within_its_range() {
        assert_eq!(double_click_or_default(500), 500);
        assert_eq!(double_click_or_default(200), 200);
        assert_eq!(double_click_or_default(900), 900);
        assert_eq!(double_click_or_default(0), DEFAULT_DOUBLE_CLICK_MS);
        assert_eq!(double_click_or_default(60_000), 5000);
        assert!((1..=5000).contains(&double_click_ms()));
    }

    #[test]
    fn the_smallest_outer_size_scales_the_inner_minimum_and_adds_the_frame() {
        // 860 x 440 at 150 %, with a 16 x 59 pixel frame and menu bar.
        assert_eq!(min_track_size((860.0, 440.0), 144, (16, 59)), (1306, 719));
        assert_eq!(min_track_size((860.0, 440.0), 96, (0, 0)), (860, 440));
        // A fraction of a pixel rounds up, so the inner size never falls short.
        assert_eq!(min_track_size((101.0, 101.0), 120, (0, 0)), (127, 127));
    }

    #[test]
    fn a_move_past_the_screen_stops_at_the_largest_size_windows_allows() {
        let min = (876, 499);
        let max = (1946, 1106);
        assert_eq!(held_size((996, 1400), min, max), (996, 1106));
        assert_eq!(held_size((996, 32767), min, max), (996, 1106));
        assert_eq!(held_size((4000, 739), min, max), (1946, 739));
        assert_eq!(held_size((996, 739), min, max), (996, 739));
        assert_eq!(held_size((286, 193), min, max), (876, 499));
        // A screen shorter than the minimum: the minimum still holds.
        assert_eq!(held_size((996, 1400), (876, 1200), max), (996, 1200));
    }

    #[test]
    fn the_largest_size_takes_the_bigger_metric_and_none_when_both_are_unread() {
        // A 100 % window next to a 150 % primary: the DPI metric shrinks to 2/3.
        assert_eq!(largest_track_size(3860, 2573), 3860);
        assert_eq!(largest_track_size(1946, 2919), 2919);
        assert_eq!(largest_track_size(0, 1106), 1106);
        assert_eq!(largest_track_size(1106, 0), 1106);
        assert_eq!(largest_track_size(0, 0), i32::MAX);
        let min = (876, 499);
        let largest = (largest_track_size(0, 0), largest_track_size(0, 0));
        assert_eq!(held_size((996, 1400), min, largest), (996, 1400));
    }

    #[test]
    fn only_the_color_set_area_counts_as_a_theme_switch() {
        let wide = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
        assert!(names_color_set(&wide("ImmersiveColorSet")));
        assert!(!names_color_set(&wide("immersivecolorset")));
        assert!(!names_color_set(&wide("ImmersiveColorSetX")));
        assert!(!names_color_set(&wide("Environment")));
        assert!(!names_color_set(&[]));
    }

    // covers: S212
    #[cfg(windows)]
    #[test]
    fn the_end_of_a_windows_session_runs_its_handler_once_and_never_holds_it_up() {
        use std::cell::Cell;
        use std::rc::Rc;
        use windows::Win32::Foundation::{HWND, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            DestroyWindow, GA_PARENT, GetAncestor, GetDesktopWindow, SendMessageW, WM_ENDSESSION,
            WM_QUERYENDSESSION,
        };

        let runs = Rc::new(Cell::new(0));
        let counted = runs.clone();
        let window = on_session_end(Box::new(move || counted.set(counted.get() + 1)))
            .expect("the hidden window");
        let hwnd = HWND(window as *mut core::ffi::c_void);
        // SAFETY: the window belongs to this thread, so each SendMessageW
        // runs its procedure right here.
        unsafe {
            // A message-only window, which the broadcast misses, has the message root as parent.
            assert_eq!(GetAncestor(hwnd, GA_PARENT), GetDesktopWindow());
            let query = SendMessageW(hwnd, WM_QUERYENDSESSION, None, None);
            assert_ne!(query.0, 0, "the session may end");
            SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(0)), None);
            assert_eq!(runs.get(), 0, "an end that was called off runs nothing");
            SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(1)), None);
            assert_eq!(runs.get(), 1);
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn the_minimum_survives_the_subclass_round_trip() {
        let min = crate::settings::MIN_WINDOW;
        assert_eq!(unpack_min_size(pack_min_size(min)), min);
        assert_eq!(
            unpack_min_size(pack_min_size((0.0, 65_535.0))),
            (0.0, 65_535.0)
        );
    }

    #[test]
    fn the_clock_follows_the_local_offset_across_midnight() {
        assert_eq!(clock_at(0, 0), "00.00");
        assert_eq!(clock_at(86_399, 0), "23.59");
        assert_eq!(clock_at(16 * 3600 + 40 * 60, 7 * 3600), "23.40");
        assert_eq!(clock_at(20 * 3600, 7 * 3600), "03.00");
        assert_eq!(clock_at(3600, -2 * 3600), "23.00");
        assert_eq!(clock_at(0, 5 * 3600 + 30 * 60), "05.30");
    }

    // covers: S170
    #[test]
    fn the_log_names_the_host_it_runs_on() {
        assert_eq!(host_name("linux"), "Linux");
        assert_eq!(host_name("windows"), "Windows");
    }

    // covers: S170
    #[test]
    fn the_linux_summary_gives_the_os_version_and_local_time() {
        let os_release =
            "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nVERSION_ID=\"24.04\"\n";
        assert_eq!(
            linux_summary(
                Some(os_release),
                Some("6.8.0-45-generic\n"),
                "x86_64",
                "21.40"
            ),
            "Ubuntu 24.04.1 LTS, kernel 6.8.0-45-generic, x86_64, local time 21.40"
        );
        assert_eq!(
            linux_summary(Some("PRETTY_NAME='Arch Linux'\n"), None, "x86_64", "08.00"),
            "Arch Linux, version unknown, x86_64, local time 08.00"
        );
        assert_eq!(
            linux_summary(None, None, "aarch64", "07.05"),
            "Linux, version unknown, aarch64, local time 07.05"
        );
    }
}
