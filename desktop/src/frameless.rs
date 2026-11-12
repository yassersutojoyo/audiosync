//! The main window without a system title bar: its own top row moves,
//! resizes and maximizes it through winit, and on Windows through the
//! window's hit test, so Windows draws the resize cursors, snaps the window
//! and offers its snap layouts on the maximize button.

use crate::MainWindow;
use crate::title_bar::{MenuKey, MenuKeyInput, MenuKeys, Mods};
use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;
use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::winit::keyboard::{Key, ModifiersState, NamedKey};
use slint::winit_030::winit::window::ResizeDirection;

/// In `title_bar::Edge`'s order, which the resize edges in ui/main.slint send.
const DIRECTIONS: [ResizeDirection; 8] = [
    ResizeDirection::North,
    ResizeDirection::South,
    ResizeDirection::East,
    ResizeDirection::West,
    ResizeDirection::NorthEast,
    ResizeDirection::NorthWest,
    ResizeDirection::SouthEast,
    ResizeDirection::SouthWest,
];

/// Lets the top row's drag area, right click and edges reach the native window.
pub fn connect(main: &MainWindow) {
    let weak = main.as_weak();
    main.on_drag_requested(move || {
        if let Some(main) = weak.upgrade() {
            main.window().with_winit_window(|window| {
                let _ = window.drag_window();
            });
        }
    });
    let weak = main.as_weak();
    main.on_resize_requested(move |edge| {
        let Some(&direction) = usize::try_from(edge).ok().and_then(|i| DIRECTIONS.get(i)) else {
            return;
        };
        if let Some(main) = weak.upgrade() {
            main.window().with_winit_window(|window| {
                let _ = window.drag_resize_window(direction);
            });
        }
    });
    let weak = main.as_weak();
    main.on_window_menu_requested(move |x, y| {
        if let Some(main) = weak.upgrade() {
            main.window().with_winit_window(|window| {
                window.show_window_menu(slint::winit_030::winit::dpi::LogicalPosition::new(x, y));
            });
        }
    });
}

/// What a key did in the main window before Slint saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Alt or F10 put the focus on the first menu.
    MenuFocused,
    /// A menu opened or a menu shortcut ran; Slint doesn't get the key.
    Taken,
}

/// The modifiers and Alt's state between the window's key events.
#[derive(Debug, Default)]
pub struct WindowKeys {
    mods: ModifiersState,
    menus: MenuKeys,
}

impl WindowKeys {
    pub fn event(&mut self, main: &MainWindow, event: &WindowEvent) -> Option<KeyOutcome> {
        let key = match event {
            WindowEvent::ModifiersChanged(mods) => {
                self.mods = mods.state();
                return None;
            }
            WindowEvent::Focused(_) => {
                self.menus.focus_changed();
                return None;
            }
            // winit makes these up for keys held while the focus moves (Alt+Tab).
            WindowEvent::KeyboardInput {
                is_synthetic: true, ..
            } => return None,
            WindowEvent::KeyboardInput { event, .. } => event,
            _ => return None,
        };
        let pressed = key.state == ElementState::Pressed;
        let input = match &key.logical_key {
            Key::Named(NamedKey::Alt) => MenuKeyInput::Alt,
            Key::Named(NamedKey::F10) => MenuKeyInput::F10,
            Key::Character(text) => {
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => MenuKeyInput::Char(c),
                    _ => MenuKeyInput::Other,
                }
            }
            _ => MenuKeyInput::Other,
        };
        let mods = Mods {
            alt: self.mods.alt_key(),
            ctrl: self.mods.control_key(),
            shift: self.mods.shift_key(),
        };
        let strings = main.global::<crate::Strings>();
        let titles = [
            strings.get_menu_file(),
            strings.get_menu_speakers(),
            strings.get_menu_view(),
            strings.get_menu_help(),
        ];
        let titles = titles.each_ref().map(|title| title.as_str());
        match self.menus.key(input, pressed, mods, &titles) {
            Some(MenuKey::FocusRow) => {
                main.invoke_focus_menu(0);
                return Some(KeyOutcome::MenuFocused);
            }
            Some(MenuKey::Open(index)) => {
                // The menu runs its own modal loop: open it after this event.
                let weak = main.as_weak();
                slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                    if let Some(main) = weak.upgrade() {
                        main.invoke_open_menu(index as i32);
                    }
                });
                return Some(KeyOutcome::Taken);
            }
            None => {}
        }
        if !pressed {
            return None;
        }
        let text: slint::SharedString = match &key.logical_key {
            Key::Named(NamedKey::F1) => slint::platform::Key::F1.into(),
            Key::Named(NamedKey::F5) => slint::platform::Key::F5.into(),
            Key::Character(text) => text.to_lowercase().into(),
            _ => return None,
        };
        let others = mods.alt || mods.shift || self.mods.super_key();
        main.invoke_shortcut(text, mods.ctrl, others)
            .then_some(KeyOutcome::Taken)
    }
}

/// Gives the main window's native window its Windows 11 shadow and rounded
/// corners and the hit test of its top row. Repeating it is harmless.
#[cfg(windows)]
pub fn attach_native(main: &MainWindow) {
    native::attach(main);
}

#[cfg(not(windows))]
pub fn attach_native(_main: &MainWindow) {}

/// Tells the Windows hit test where the menus end; Slint lays them out.
#[cfg(windows)]
pub fn note_layout(main: &MainWindow) {
    native::CAPTION_START.with(|start| start.set(main.get_caption_start()));
}

#[cfg(not(windows))]
pub fn note_layout(_main: &MainWindow) {}

#[cfg(windows)]
mod native {
    use super::MainWindow;
    use crate::title_bar::{Edge, Layout, Region, hit_test};
    use slint::ComponentHandle;
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::cell::{Cell, RefCell};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::{
        DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        DwmSetWindowAttribute,
    };
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
    };
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT,
        HTMAXBUTTON, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed, SW_MAXIMIZE, SW_RESTORE,
        ShowWindow, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN,
        WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
    };

    const SUBCLASS: usize = 0x4153_5442;

    thread_local! {
        // A window procedure takes no context of ours; it runs on this thread.
        pub(super) static CAPTION_START: Cell<f32> = const { Cell::new(f32::MAX) };
        static MAIN: RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
        static POINTER: Cell<(bool, bool)> = const { Cell::new((false, false)) };
    }

    pub fn attach(main: &MainWindow) {
        MAIN.with(|slot| *slot.borrow_mut() = Some(main.as_weak()));
        super::note_layout(main);
        main.window().with_winit_window(|window| {
            use slint::winit_030::winit::platform::windows::WindowExtWindows;
            window.set_undecorated_shadow(true);
            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return;
            };
            let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
            let corners: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
            // SAFETY: `hwnd` is this thread's live window; the corner value is
            // a 4-byte DWM enum read for exactly its size, and the procedure
            // only reads the messages it is given.
            unsafe {
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_WINDOW_CORNER_PREFERENCE,
                    (&raw const corners).cast(),
                    size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
                );
                let _ = SetWindowSubclass(hwnd, Some(procedure), SUBCLASS, 0);
            }
        });
    }

    /// The maximize button's hover and press go to Slint after the message,
    /// never from inside the window procedure.
    fn set_pointer(hover: bool, pressed: bool) {
        if POINTER.with(|pointer| pointer.replace((hover, pressed))) == (hover, pressed) {
            return;
        }
        let Some(weak) = MAIN.with(|slot| slot.borrow().clone()) else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(main) = weak.upgrade() {
                main.set_maximize_hovered(hover);
                main.set_maximize_pressed(pressed);
            }
        });
    }

    fn region(hwnd: HWND, lparam: LPARAM) -> Option<Region> {
        let mut point = POINT {
            x: i32::from(lparam.0 as u16 as i16),
            y: i32::from((lparam.0 >> 16) as u16 as i16),
        };
        let mut rect = RECT::default();
        // SAFETY: `hwnd` is the live window this message is for, and both
        // structures are valid and writable.
        let (dpi, maximized) = unsafe {
            if !ScreenToClient(hwnd, &mut point).as_bool()
                || GetClientRect(hwnd, &mut rect).is_err()
            {
                return None;
            }
            (GetDpiForWindow(hwnd), IsZoomed(hwnd).as_bool())
        };
        let scale = dpi.max(1) as f32 / 96.0;
        let layout = Layout {
            width: (rect.right - rect.left) as f32 / scale,
            height: (rect.bottom - rect.top) as f32 / scale,
            caption_start: CAPTION_START.with(Cell::get),
            maximized,
        };
        Some(hit_test(
            point.x as f32 / scale,
            point.y as f32 / scale,
            layout,
        ))
    }

    unsafe extern "system" fn procedure(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        let on_maximize = wparam.0 == HTMAXBUTTON as usize;
        match message {
            WM_NCHITTEST => {
                // SAFETY: asks winit and Windows first, unchanged.
                let hit = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
                if hit.0 != HTCLIENT as isize {
                    return hit;
                }
                let code = match region(hwnd, lparam) {
                    None | Some(Region::Client) => HTCLIENT,
                    Some(Region::Caption) => HTCAPTION,
                    Some(Region::Maximize) => HTMAXBUTTON,
                    Some(Region::Resize(edge)) => match edge {
                        Edge::North => HTTOP,
                        Edge::South => HTBOTTOM,
                        Edge::East => HTRIGHT,
                        Edge::West => HTLEFT,
                        Edge::NorthEast => HTTOPRIGHT,
                        Edge::NorthWest => HTTOPLEFT,
                        Edge::SouthEast => HTBOTTOMRIGHT,
                        Edge::SouthWest => HTBOTTOMLEFT,
                    },
                };
                return LRESULT(code as isize);
            }
            WM_NCMOUSEMOVE => {
                let pressed = POINTER.with(Cell::get).1;
                set_pointer(on_maximize, pressed && on_maximize);
                if on_maximize {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE | TME_NONCLIENT,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: `track` is a valid structure for this live window.
                    let _ = unsafe { TrackMouseEvent(&mut track) };
                }
            }
            WM_NCMOUSELEAVE => set_pointer(false, false),
            // The button is drawn by Slint: Windows must not track or draw
            // its own maximize button.
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if on_maximize => {
                set_pointer(true, true);
                return LRESULT(0);
            }
            WM_NCLBUTTONUP if on_maximize => {
                if POINTER.with(Cell::get).1 {
                    // SAFETY: `hwnd` is the live window this message is for.
                    unsafe {
                        let show = if IsZoomed(hwnd).as_bool() {
                            SW_RESTORE
                        } else {
                            SW_MAXIMIZE
                        };
                        let _ = ShowWindow(hwnd, show);
                    }
                }
                set_pointer(true, false);
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                // SAFETY: removes this procedure from the window being
                // destroyed, as the comctl32 subclass documentation asks.
                let _ = unsafe { RemoveWindowSubclass(hwnd, Some(procedure), SUBCLASS) };
            }
            _ => {}
        }
        // SAFETY: passes the message on unchanged, as a subclass must.
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }
}
