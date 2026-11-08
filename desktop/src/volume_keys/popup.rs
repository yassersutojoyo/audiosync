//! The small level pop-up a volume key shows while the keys move the master
//! (spec S429), in place of the Windows volume flyout the key no longer
//! reaches. A plain Win32 window on the UI thread: it never takes the focus,
//! clicks pass through it, and it hides itself about 2 s after the last key.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateRoundRectRgn, CreateSolidBrush, DT_CENTER, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, GetMonitorInfoW, HDC, HGDIOBJ,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, PAINTSTRUCT, PS_SOLID, RoundRect,
    SelectObject, SetBkMode, SetTextColor, SetWindowRgn, TRANSPARENT,
};
use windows::Win32::UI::Accessibility::{
    NotificationKind_ActionCompleted, NotificationProcessing_MostRecent, UiaClientsAreListening,
    UiaHostProviderFromHwnd, UiaRaiseNotificationEvent,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetCursorPos, HWND_TOPMOST, KillTimer, LWA_ALPHA,
    RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetLayeredWindowAttributes,
    SetTimer, SetWindowPos, ShowWindow, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BSTR, PCWSTR, w};

/// The whole show: in for 6 %, held until 80 %, then out.
const SHOWN: Duration = Duration::from_millis(2100);
const IN: Duration = Duration::from_millis(126);
const OUT_FROM: Duration = Duration::from_millis(1680);
/// The frame timer's period.
const FRAME_MS: u32 = 16;
const TIMER_ID: usize = 1;
/// Logical size, and the gap above the taskbar.
const WIDTH: f64 = 240.0;
const HEIGHT: f64 = 64.0;
const GAP: f64 = 12.0;
/// How far the card rises as it comes in, unless animations are off.
const RISE: f64 = 6.0;

struct Colors {
    background: COLORREF,
    border: COLORREF,
    text: COLORREF,
    secondary: COLORREF,
    track: COLORREF,
    fill: COLORREF,
}

const fn rgb(hex: u32) -> COLORREF {
    COLORREF(((hex & 0xff) << 16) | (hex & 0xff00) | ((hex >> 16) & 0xff))
}

const LIGHT: Colors = Colors {
    background: rgb(0xFFFFFF),
    border: rgb(0xE6E6E6),
    text: rgb(0x1A1A1A),
    secondary: rgb(0x525C5A),
    track: rgb(0xD9DEDD),
    fill: rgb(0x147167),
};

const DARK: Colors = Colors {
    background: rgb(0x2B2B2B),
    border: rgb(0x3D3D3D),
    text: rgb(0xFFFFFF),
    secondary: rgb(0xB8C2C0),
    track: rgb(0x3D3D3D),
    fill: rgb(0x3CCBB9),
};

struct Shown {
    level: f64,
    dark: bool,
    name: Vec<u16>,
    started: Instant,
    rises: bool,
    scale: f64,
    /// Where the card rests, in physical pixels.
    left: i32,
    top: i32,
}

thread_local! {
    static POPUP: RefCell<Option<Shown>> = const { RefCell::new(None) };
}

/// Shows `level` (0 to 100) under `name`, or moves the one showing to it and
/// keeps it up for another 2 s; Narrator reads `announcement`.
pub fn show(level: f64, dark: bool, name: &str, announcement: &str, animations: bool) {
    let Some(hwnd) = window() else {
        return;
    };
    announce(hwnd, announcement);
    let (left, top, scale) = place();
    let size = (
        (WIDTH * scale).round() as i32,
        (HEIGHT * scale).round() as i32,
    );
    let now = Instant::now();
    POPUP.with(|slot| {
        let mut slot = slot.borrow_mut();
        let showing = slot
            .as_ref()
            .is_some_and(|shown| now.duration_since(shown.started) < SHOWN);
        // A key while it shows keeps it up without coming in again.
        let started = if showing {
            now.checked_sub(IN).unwrap_or(now)
        } else {
            now
        };
        *slot = Some(Shown {
            level,
            dark,
            name: name.encode_utf16().collect(),
            started,
            rises: animations,
            scale,
            left,
            top,
        });
    });
    // SAFETY: `hwnd` is this thread's live pop-up window; the region becomes
    // the window's, which frees it.
    unsafe {
        let corner = (16.0 * scale).round() as i32;
        let region = CreateRoundRectRgn(0, 0, size.0 + 1, size.1 + 1, corner, corner);
        SetWindowRgn(hwnd, Some(region), false);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            left,
            top,
            size.0,
            size.1,
            SWP_NOACTIVATE,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), TIMER_ID, FRAME_MS, None);
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, true);
    }
    frame(hwnd);
}

/// Raises a UIA notification on the pop-up's window: it never takes the
/// focus, so nothing else tells a screen reader the level. Each one replaces
/// the last one not yet read.
fn announce(hwnd: HWND, text: &str) {
    // SAFETY: `hwnd` is this thread's live pop-up window; the provider and
    // both strings outlive the call.
    unsafe {
        if !UiaClientsAreListening().as_bool() {
            return;
        }
        if let Ok(provider) = UiaHostProviderFromHwnd(hwnd) {
            let _ = UiaRaiseNotificationEvent(
                &provider,
                NotificationKind_ActionCompleted,
                NotificationProcessing_MostRecent,
                &BSTR::from(text),
                &BSTR::from("AudioSync.VolumeLevel"),
            );
        }
    }
}

/// The pop-up's window, made the first time.
fn window() -> Option<HWND> {
    thread_local! {
        static HWND_MADE: RefCell<Option<HWND>> = const { RefCell::new(None) };
    }
    HWND_MADE.with(|made| {
        if let Some(hwnd) = *made.borrow() {
            return Some(hwnd);
        }
        let class = w!("AudioSyncLevelPopup");
        let description = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            lpszClassName: class,
            ..WNDCLASSW::default()
        };
        // SAFETY: the class names a procedure that lives for the process, and
        // the window belongs to this thread, whose event loop dispatches it.
        let hwnd = unsafe {
            RegisterClassW(&raw const description);
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
                class,
                w!("AudioSync master volume"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .ok()?;
        *made.borrow_mut() = Some(hwnd);
        Some(hwnd)
    })
}

/// The card's resting place, bottom center of the work area of the monitor
/// under the pointer, where the Windows volume flyout shows, and the scale.
fn place() -> (i32, i32, f64) {
    let mut pointer = POINT::default();
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..MONITORINFO::default()
    };
    let (mut dpi, mut unused) = (96_u32, 96_u32);
    // SAFETY: plain reads into local structures.
    let work = unsafe {
        let _ = GetCursorPos(&raw mut pointer);
        let monitor = MonitorFromPoint(pointer, MONITOR_DEFAULTTOPRIMARY);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi, &raw mut unused);
        let _ = GetMonitorInfoW(monitor, &raw mut info);
        info.rcWork
    };
    let scale = f64::from(dpi.max(96)) / 96.0;
    let width = (WIDTH * scale).round() as i32;
    let height = (HEIGHT * scale).round() as i32;
    let left = work.left + (work.right - work.left - width) / 2;
    let top = work.bottom - height - (GAP * scale).round() as i32;
    (left, top, scale)
}

/// Sets the card's opacity and place for now, and hides it at the end.
fn frame(hwnd: HWND) {
    let now = Instant::now();
    let step = POPUP.with(|slot| {
        let slot = slot.borrow();
        let shown = slot.as_ref()?;
        let at = now.duration_since(shown.started);
        if at >= SHOWN {
            return Some(None);
        }
        let opacity = if at < IN {
            at.as_secs_f64() / IN.as_secs_f64()
        } else if at < OUT_FROM {
            1.0
        } else {
            1.0 - (at - OUT_FROM).as_secs_f64() / (SHOWN - OUT_FROM).as_secs_f64()
        };
        let rise = if shown.rises && at < IN {
            (RISE * shown.scale * (1.0 - opacity)).round() as i32
        } else {
            0
        };
        Some(Some((opacity, shown.left, shown.top + rise)))
    });
    // SAFETY: `hwnd` is this thread's live pop-up window.
    unsafe {
        match step {
            Some(Some((opacity, left, top))) => {
                let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    left,
                    top,
                    0,
                    0,
                    SWP_NOACTIVATE | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
                );
            }
            _ => {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TIMER => {
            frame(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        // SAFETY: the arguments are the ones Windows gave this procedure.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn paint(hwnd: HWND) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: a paint cycle on this thread's window; every object made here
    // is deselected and deleted before it ends.
    unsafe {
        let dc = BeginPaint(hwnd, &raw mut paint);
        POPUP.with(|slot| {
            if let Some(shown) = slot.borrow().as_ref() {
                draw(dc, shown);
            }
        });
        let _ = EndPaint(hwnd, &raw const paint);
    }
}

/// Draws the card: the name above, then the speaker, the bar and the number.
///
/// # Safety
/// `dc` is a live paint DC of the pop-up's window.
unsafe fn draw(dc: HDC, shown: &Shown) {
    let colors = if shown.dark { &DARK } else { &LIGHT };
    let px = |logical: f64| (logical * shown.scale).round() as i32;
    let (width, height) = (px(WIDTH), px(HEIGHT));
    // SAFETY: as the function's contract says.
    unsafe {
        let background = CreateSolidBrush(colors.background);
        let border = CreatePen(PS_SOLID, px(1.0).max(1), colors.border);
        let old_brush = SelectObject(dc, HGDIOBJ(background.0));
        let old_pen = SelectObject(dc, HGDIOBJ(border.0));
        let _ = RoundRect(dc, 0, 0, width, height, px(16.0), px(16.0));
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        let _ = DeleteObject(HGDIOBJ(border.0));
        let _ = DeleteObject(HGDIOBJ(background.0));

        SetBkMode(dc, TRANSPARENT);
        let text = font(px(11.0), w!("Segoe UI Variable Text"));
        let old_font = SelectObject(dc, HGDIOBJ(text.0));
        SetTextColor(dc, colors.secondary);
        let mut name = shown.name.clone();
        let mut line = RECT {
            left: px(14.0),
            top: px(12.0),
            right: width - px(14.0),
            bottom: px(27.0),
        };
        DrawTextW(dc, &mut name, &raw mut line, DT_SINGLELINE | DT_VCENTER);
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(text.0));

        let row_top = px(33.0);
        let row_bottom = px(51.0);
        let icon = font(px(16.0), w!("Segoe MDL2 Assets"));
        let old_font = SelectObject(dc, HGDIOBJ(icon.0));
        SetTextColor(dc, colors.text);
        let mut speaker = [0xE767_u16];
        let mut icon_box = RECT {
            left: px(14.0),
            top: row_top,
            right: px(32.0),
            bottom: row_bottom,
        };
        DrawTextW(
            dc,
            &mut speaker,
            &raw mut icon_box,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER,
        );
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(icon.0));

        let bar_left = px(42.0);
        let bar_right = width - px(14.0 + 32.0 + 10.0);
        let middle = (row_top + row_bottom) / 2;
        let track = RECT {
            left: bar_left,
            top: middle - px(3.0),
            right: bar_right,
            bottom: middle + px(3.0),
        };
        fill_round(dc, track, colors.track, px(6.0));
        let filled = bar_left
            + ((f64::from(bar_right - bar_left)) * shown.level.clamp(0.0, 100.0) / 100.0).round()
                as i32;
        if filled > bar_left {
            fill_round(
                dc,
                RECT {
                    right: filled,
                    ..track
                },
                colors.fill,
                px(6.0),
            );
        }

        let number = font(px(14.0), w!("Segoe UI Variable Text"));
        let old_font = SelectObject(dc, HGDIOBJ(number.0));
        SetTextColor(dc, colors.text);
        let mut digits: Vec<u16> = format!("{}", shown.level.round() as i32)
            .encode_utf16()
            .collect();
        let mut number_box = RECT {
            left: width - px(14.0 + 32.0),
            top: row_top,
            right: width - px(14.0),
            bottom: row_bottom,
        };
        DrawTextW(
            dc,
            &mut digits,
            &raw mut number_box,
            DT_SINGLELINE | DT_VCENTER | DT_RIGHT,
        );
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(number.0));
    }
}

/// A font of `pixels` em height.
///
/// # Safety
/// Plain GDI; the caller deletes the font.
unsafe fn font(pixels: i32, face: PCWSTR) -> windows::Win32::Graphics::Gdi::HFONT {
    use windows::Win32::Graphics::Gdi::{
        CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FW_NORMAL, OUT_DEFAULT_PRECIS,
    };
    // SAFETY: as the function's contract says.
    unsafe {
        CreateFontW(
            -pixels,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            face,
        )
    }
}

/// Fills `rect` with `color`, its ends rounded.
///
/// # Safety
/// `dc` is a live DC.
unsafe fn fill_round(dc: HDC, rect: RECT, color: COLORREF, corner: i32) {
    // SAFETY: as the function's contract says.
    unsafe {
        let brush = CreateSolidBrush(color);
        let pen = CreatePen(PS_SOLID, 1, color);
        let old_brush = SelectObject(dc, HGDIOBJ(brush.0));
        let old_pen = SelectObject(dc, HGDIOBJ(pen.0));
        let _ = RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            corner,
            corner,
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        let _ = DeleteObject(HGDIOBJ(pen.0));
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}
