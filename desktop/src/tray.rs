//! The notification-area (tray) icon: the Windows counterpart of the macOS
//! menu bar item. A left click opens the flyout, a double click the main
//! window, and a right click a short menu (Open AudioSync, streaming on or
//! off, Launch at Sign-In, Exit). The icon follows the taskbar's light or dark mode (checked
//! every few seconds, so a theme change shows without a state change) and
//! dims while streaming is off; its tooltip says what is streaming and
//! names the newest notice, if any.
//!
//! On Linux the icon is a StatusNotifier item: the app's colored icon,
//! dimmed while streaming is off. A left click opens the flyout at the
//! pointer, since the host gives no icon position; a host that only shows
//! the menu sends no click, and its menu holds Open AudioSync.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
#[cfg(any(windows, target_os = "linux"))]
use std::time::{Duration, Instant};

#[cfg(any(windows, target_os = "linux"))]
use slint::ComponentHandle;
#[cfg(any(windows, target_os = "linux"))]
use slint::winit_030::winit::event::WindowEvent;
#[cfg(any(windows, target_os = "linux"))]
use slint::winit_030::{EventResult, WinitWindowAccessor};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[cfg(any(windows, target_os = "linux"))]
use crate::placement::{self, Bounds};
#[cfg(any(windows, target_os = "linux"))]
use crate::platform;
use crate::ui_model::AppState;
use crate::view::{self, App};

/// Icon resource ordinals, as build.rs embeds them.
#[cfg(windows)]
const ICON_LIGHT: u16 = 2;
#[cfg(windows)]
const ICON_DARK: u16 = 3;
#[cfg(windows)]
const ICON_LIGHT_OFF: u16 = 4;
#[cfg(windows)]
const ICON_DARK_OFF: u16 = 5;
/// Each of those plus this is the same with the notice badge.
#[cfg(windows)]
const ICON_NOTICE_STEP: u16 = 4;

/// The app's icon, compiled in: a StatusNotifier host draws the pixmap as
/// it is, so a colored icon reads on light and dark panels alike.
#[cfg(target_os = "linux")]
const APP_ICON_PNG: &[u8] = include_bytes!("../res/audiosync-256.png");

/// A click on the icon right after the flyout closed (because that click
/// took its focus) only closes it.
#[cfg(any(windows, target_os = "linux"))]
const REOPEN_GUARD: Duration = Duration::from_millis(300);

/// Logical pixels between the flyout and the taskbar.
#[cfg(any(windows, target_os = "linux"))]
const FLYOUT_GAP: f64 = 12.0;

/// How often the taskbar's theme and the icon size are checked again. A
/// registry read and a system metric: cheap enough to poll.
#[cfg(windows)]
const THEME_CHECK: Duration = Duration::from_secs(2);

thread_local! {
    /// The app, for tray events, which arrive through `invoke_from_event_loop`.
    static APP: RefCell<Weak<App>> = const { RefCell::new(Weak::new()) };
}

#[cfg(any(windows, target_os = "linux"))]
thread_local! {
    static FLYOUT_CLOSED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    /// The icon the flyout last opened next to: a refresh while it is open
    /// can change its height, and it is placed again.
    static FLYOUT_ICON: Cell<Option<Bounds>> = const { Cell::new(None) };
}

pub struct Tray {
    icon: TrayIcon,
    toggle_item: MenuItem,
    login_item: CheckMenuItem,
    /// The icon variant showing now, so it is replaced only when it changes.
    shown: Cell<Option<IconChoice>>,
    running: Cell<bool>,
    /// A notice is up: the icon shows its badge, as the Mac's shows a dot.
    notice: Cell<bool>,
    /// A tooltip not shown yet: Windows refuses it while Explorer restarts,
    /// and the icon it adds again would keep the old one.
    tooltip: RefCell<Option<String>>,
    #[cfg(windows)]
    theme_timer: slint::Timer,
}

/// Which icon resource the tray shows, and at what size.
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq)]
struct IconChoice {
    ordinal: u16,
    size: Option<(u32, u32)>,
}

#[cfg(windows)]
impl IconChoice {
    /// The variant for the taskbar's current theme and scale.
    fn current(running: bool, notice: bool) -> IconChoice {
        IconChoice {
            ordinal: icon_ordinal(platform::taskbar_is_light(), running, notice),
            size: platform::small_icon_size(),
        }
    }
}

/// The app's icon, full or dimmed: the panel's theme isn't known here.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq)]
struct IconChoice {
    running: bool,
}

#[cfg(target_os = "linux")]
impl IconChoice {
    // The panel's icon has no notice badge.
    fn current(running: bool, _notice: bool) -> IconChoice {
        IconChoice { running }
    }
}

/// Why the tray icon couldn't be made.
#[derive(Debug)]
pub struct TrayError(String);

impl std::fmt::Display for TrayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Tray {
    /// Adds the icon and connects it to `app`. The icon stays until the
    /// returned `Tray` is dropped.
    pub fn create(app: &Rc<App>) -> Result<Rc<Tray>, TrayError> {
        let strings = app.strings();
        let open_item = MenuItem::with_id(MenuIds::OPEN, strings.get_open_app(), true, None);
        let toggle_item = MenuItem::with_id(
            MenuIds::TOGGLE,
            strings.get_menu_turn_streaming_on(),
            true,
            None,
        );
        let login_item = CheckMenuItem::with_id(
            MenuIds::LOGIN,
            strings.get_launch_at_login(),
            true,
            app.launch_at_login(),
            None,
        );
        let exit_item = MenuItem::with_id(MenuIds::EXIT, strings.get_exit_app(), true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open_item,
            &toggle_item,
            &PredefinedMenuItem::separator(),
            &login_item,
            &PredefinedMenuItem::separator(),
            &exit_item,
        ])
        .map_err(|e| TrayError(e.to_string()))?;

        let state = app.snapshot();
        let notice = !app.notices_posted().is_empty();
        let choice = IconChoice::current(state.running, notice);
        let icon = TrayIconBuilder::new()
            .with_icon(load_icon(choice)?)
            .with_tooltip(view::tray_tooltip(&strings, &state, &app.notices_posted()))
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| TrayError(e.to_string()))?;

        let tray = Rc::new(Tray {
            icon,
            toggle_item,
            login_item,
            shown: Cell::new(Some(choice)),
            running: Cell::new(state.running),
            notice: Cell::new(notice),
            tooltip: RefCell::new(None),
            #[cfg(windows)]
            theme_timer: slint::Timer::default(),
        });

        APP.with(|cell| *cell.borrow_mut() = Rc::downgrade(app));
        TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
            let _ = slint::invoke_from_event_loop(move || on_tray_event(event));
        }));
        MenuEvent::set_event_handler(Some(|event: MenuEvent| {
            let _ = slint::invoke_from_event_loop(move || on_menu_event(event.id));
        }));

        // The flyout closes when it loses focus, like the system flyouts.
        #[cfg(any(windows, target_os = "linux"))]
        app.flyout.window().on_winit_window_event(|window, event| {
            if let Some(input) = crate::view::focus_input(event)
                && let Some(app) = APP.with(|cell| cell.borrow().upgrade())
            {
                app.note_focus_input(input);
            }
            match event {
                WindowEvent::Focused(false) => {
                    FLYOUT_CLOSED_AT.with(|at| at.set(Some(Instant::now())));
                    let _ = window.hide();
                }
                // Slint reads the Windows accent after this filter returns.
                #[cfg(windows)]
                WindowEvent::ThemeChanged(_) => {
                    slint::Timer::single_shot(Duration::ZERO, || {
                        if let Some(app) = APP.with(|cell| cell.borrow().upgrade()) {
                            app.use_brand_accent(app.flyout.window());
                        }
                    });
                }
                _ => {}
            }
            EventResult::Propagate
        });

        let weak = Rc::downgrade(&tray);
        app.set_on_refresh(move |state| {
            if let Some(tray) = weak.upgrade() {
                tray.show_state(state);
            }
            #[cfg(any(windows, target_os = "linux"))]
            place_open_flyout();
        });
        #[cfg(any(windows, target_os = "linux"))]
        app.flyout.on_fit_height_changed(place_open_flyout);
        // The taskbar's theme can change while nothing else does.
        #[cfg(windows)]
        {
            let weak = Rc::downgrade(&tray);
            tray.theme_timer
                .start(slint::TimerMode::Repeated, THEME_CHECK, move || {
                    if let Some(tray) = weak.upgrade() {
                        tray.show_tooltip();
                        tray.show_icon();
                    }
                });
        }
        tray.show_state(&state);
        Ok(tray)
    }

    fn show_state(&self, state: &AppState) {
        let Some(app) = APP.with(|cell| cell.borrow().upgrade()) else {
            return;
        };
        let strings = app.strings();
        let posted = app.notices_posted();
        self.notice.set(!posted.is_empty());
        *self.tooltip.borrow_mut() = Some(view::tray_tooltip(&strings, state, &posted));
        self.show_tooltip();
        self.toggle_item.set_text(if state.running {
            strings.get_menu_turn_streaming_off()
        } else {
            strings.get_menu_turn_streaming_on()
        });
        self.login_item.set_checked(app.launch_at_login());
        self.running.set(state.running);
        self.show_icon();
    }

    fn show_tooltip(&self) {
        show_pending(&mut self.tooltip.borrow_mut(), |text| {
            self.icon.set_tooltip(Some(text))
        });
    }

    /// Shows the icon variant for the current theme, scale and state, if it
    /// isn't showing already.
    fn show_icon(&self) {
        let choice = IconChoice::current(self.running.get(), self.notice.get());
        if self.shown.get() != Some(choice)
            && let Ok(icon) = load_icon(choice)
            && self.icon.set_icon(Some(icon)).is_ok()
        {
            self.shown.set(Some(choice));
        }
    }

    /// Opens the flyout at the icon, as a click on it does.
    #[cfg(windows)]
    pub fn open_flyout(&self, app: &App) {
        if let Some(rect) = self.icon.rect() {
            let icon = Bounds {
                x: rect.position.x,
                y: rect.position.y,
                width: f64::from(rect.size.width),
                height: f64::from(rect.size.height),
            };
            show_flyout(app, icon);
        }
    }
}

/// Hands `pending` to `set`, keeping it for the next try when `set` fails.
fn show_pending<E>(pending: &mut Option<String>, set: impl FnOnce(&str) -> Result<(), E>) {
    if let Some(text) = pending.take()
        && set(&text).is_err()
    {
        *pending = Some(text);
    }
}

struct MenuIds;

impl MenuIds {
    const OPEN: &str = "open";
    const TOGGLE: &str = "toggle";
    const LOGIN: &str = "login";
    const EXIT: &str = "exit";
}

#[cfg(windows)]
fn icon_ordinal(light_taskbar: bool, running: bool, notice: bool) -> u16 {
    let plain = match (light_taskbar, running) {
        (true, true) => ICON_LIGHT,
        (false, true) => ICON_DARK,
        (true, false) => ICON_LIGHT_OFF,
        (false, false) => ICON_DARK_OFF,
    };
    if notice {
        plain + ICON_NOTICE_STEP
    } else {
        plain
    }
}

#[cfg(windows)]
fn load_icon(choice: IconChoice) -> Result<Icon, TrayError> {
    // The small icon size, so the 16, 20 and 24 px frames are used; without
    // a size, Windows loads the 32 px (large icon) frame and shrinks it.
    Icon::from_resource(choice.ordinal, choice.size).map_err(|e| TrayError(e.to_string()))
}

#[cfg(target_os = "linux")]
fn load_icon(choice: IconChoice) -> Result<Icon, TrayError> {
    let error = |e: &dyn std::fmt::Display| TrayError(format!("the tray icon image: {e}"));
    let decoder = png::Decoder::new(std::io::Cursor::new(APP_ICON_PNG));
    let mut reader = decoder.read_info().map_err(|e| error(&e))?;
    if reader.output_color_type() != (png::ColorType::Rgba, png::BitDepth::Eight) {
        return Err(error(&"not 8-bit RGBA"));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| error(&"too large"))?;
    let mut rgba = vec![0; size];
    let frame = reader.next_frame(&mut rgba).map_err(|e| error(&e))?;
    rgba.truncate(frame.buffer_size());
    if !choice.running {
        crate::os::tray_watch::dim(&mut rgba);
    }
    Icon::from_rgba(rgba, frame.width, frame.height).map_err(|e| error(&e))
}

fn on_tray_event(event: TrayIconEvent) {
    let Some(app) = APP.with(|cell| cell.borrow().upgrade()) else {
        return;
    };
    match event {
        #[cfg(windows)]
        TrayIconEvent::Click {
            rect,
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } => toggle_flyout(
            &app,
            Bounds {
                x: rect.position.x,
                y: rect.position.y,
                width: f64::from(rect.size.width),
                height: f64::from(rect.size.height),
            },
        ),
        // The host reports the pointer, which stands in for the icon.
        #[cfg(target_os = "linux")]
        TrayIconEvent::Click {
            position,
            button: MouseButton::Left,
            ..
        } => toggle_flyout(
            &app,
            Bounds {
                x: position.x,
                y: position.y,
                width: 0.0,
                height: 0.0,
            },
        ),
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => {
            app.hide_flyout();
            app.show_main();
        }
        // Before the menu opens on the right button's release: a change
        // made in Task Manager shows in its check mark.
        TrayIconEvent::Enter { .. }
        | TrayIconEvent::Click {
            button: MouseButton::Right,
            button_state: MouseButtonState::Down,
            ..
        } => app.reread_launch_at_login(),
        _ => {}
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn toggle_flyout(app: &App, icon: Bounds) {
    let just_closed = FLYOUT_CLOSED_AT.with(|at| {
        at.take()
            .is_some_and(|closed| closed.elapsed() < REOPEN_GUARD)
    });
    if app.flyout.window().is_visible() {
        app.hide_flyout();
    } else if !just_closed {
        show_flyout(app, icon);
    }
}

fn on_menu_event(id: MenuId) {
    let Some(app) = APP.with(|cell| cell.borrow().upgrade()) else {
        return;
    };
    if id == MenuIds::OPEN {
        app.show_main();
    } else if id == MenuIds::TOGGLE {
        let running = app.snapshot().running;
        app.update(|backend| backend.set_running(!running));
    } else if id == MenuIds::LOGIN {
        app.set_launch_at_login(!app.launch_at_login());
    } else if id == MenuIds::EXIT {
        let _ = slint::quit_event_loop();
    }
}

/// Opens the flyout next to the icon, inside the work area of the icon's
/// monitor, and gives it the keyboard focus.
#[cfg(any(windows, target_os = "linux"))]
fn show_flyout(app: &App, icon: Bounds) {
    let flyout = app.flyout.window();
    app.prepare_flyout_open(platform::animation_effects());
    if flyout.show().is_err() {
        return;
    }
    #[cfg(windows)]
    flyout.with_winit_window(|window| {
        use slint::winit_030::winit::platform::windows::WindowExtWindows;
        window.set_skip_taskbar(true);
    });
    // With the system's rounded frame, the flyout leaves out its own edge.
    let rounded = app.apply_frame(flyout, "tray flyout", crate::backdrop::Frame::Flyout);
    app.flyout.set_rounded(rounded);
    FLYOUT_ICON.with(|cell| cell.set(Some(icon)));
    place_flyout(app, icon);
    platform::bring_to_front(flyout);
    app.flyout.set_revealed(true);
    app.flyout.invoke_take_focus();
}

/// Sizes and places the flyout again while it is open.
#[cfg(any(windows, target_os = "linux"))]
fn place_open_flyout() {
    if let Some(app) = APP.with(|cell| cell.borrow().upgrade())
        && app.flyout.window().is_visible()
        && let Some(icon) = FLYOUT_ICON.with(Cell::get)
    {
        place_flyout(&app, icon);
    }
}

/// Sizes the flyout to its parts, as much as the work area of the icon's
/// monitor holds, and places it next to the icon.
#[cfg(windows)]
fn place_flyout(app: &App, icon: Bounds) {
    let flyout = app.flyout.window();
    let scale = f64::from(flyout.scale_factor());
    let work_area = platform::work_area_at(icon.x, icon.y).unwrap_or(Bounds {
        x: 0.0,
        y: 0.0,
        width: icon.x + icon.width,
        height: icon.y,
    });
    let gap = FLYOUT_GAP * scale;
    let room = (work_area.height - 2.0 * gap) / scale;
    let height = f64::from(app.fit_flyout(room as f32)) * scale;
    let (x, y) =
        placement::flyout_origin(icon, f64::from(flyout.size().width), height, work_area, gap);
    let origin = slint::PhysicalPosition::new(x.round() as i32, y.round() as i32);
    if flyout.position() != origin {
        flyout.set_position(origin);
    }
}

/// Sizes the flyout to its parts, as much as the monitor under the pointer
/// holds, and places it next to the pointer. A Wayland compositor places it
/// itself and ignores the position.
#[cfg(target_os = "linux")]
fn place_flyout(app: &App, pointer: Bounds) {
    let flyout = app.flyout.window();
    let monitors: Vec<Bounds> = flyout
        .with_winit_window(|window| {
            window
                .available_monitors()
                .map(|monitor| Bounds {
                    x: f64::from(monitor.position().x),
                    y: f64::from(monitor.position().y),
                    width: f64::from(monitor.size().width),
                    height: f64::from(monitor.size().height),
                })
                .collect()
        })
        .unwrap_or_default();
    let Some(monitor) = placement::monitor_at(&monitors, pointer.x, pointer.y) else {
        return;
    };
    let scale = f64::from(flyout.scale_factor());
    let gap = FLYOUT_GAP * scale;
    let room = (monitor.height - 2.0 * gap) / scale;
    let height = f64::from(app.fit_flyout(room as f32)) * scale;
    let (x, y) = placement::pointer_flyout_origin(
        (pointer.x, pointer.y),
        f64::from(flyout.size().width),
        height,
        monitor,
        gap,
    );
    let origin = slint::PhysicalPosition::new(x.round() as i32, y.round() as i32);
    if flyout.position() != origin {
        flyout.set_position(origin);
    }
}

#[cfg(test)]
mod tests {
    /// While a notice is up the tray icon takes its variant with the badge,
    /// for either taskbar and either streaming state (S440).
    // covers: S440
    #[cfg(windows)]
    #[test]
    fn a_notice_puts_the_badge_on_the_tray_icon() {
        use super::*;
        for (light, running) in [(true, true), (false, true), (true, false), (false, false)] {
            let plain = icon_ordinal(light, running, false);
            assert!((ICON_LIGHT..=ICON_DARK_OFF).contains(&plain));
            assert_eq!(icon_ordinal(light, running, true), plain + 4);
        }
    }

    use super::show_pending;

    #[test]
    fn a_tooltip_windows_refuses_is_kept_until_it_is_shown() {
        let mut pending = Some("Streaming".to_owned());
        show_pending(&mut pending, |_| Err(()));
        assert_eq!(pending.as_deref(), Some("Streaming"));
        let mut shown = None;
        show_pending(&mut pending, |text| {
            shown = Some(text.to_owned());
            Ok::<(), ()>(())
        });
        assert_eq!(shown.as_deref(), Some("Streaming"));
        assert_eq!(pending, None);
    }
}
