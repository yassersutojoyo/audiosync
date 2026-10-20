//! The notification-area (tray) icon: the Windows counterpart of the macOS
//! menu bar item. A left click opens the flyout, a double click the main
//! window, and a right click a short menu (Open AudioSync, streaming on or
//! off, Exit). The icon follows the taskbar's light or dark mode (checked
//! every few seconds, so a theme change shows without a state change) and
//! dims while streaming is off; its tooltip says what is streaming and
//! names the newest notice, if any.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::placement::{self, Bounds};
use crate::platform;
use crate::ui_model::AppState;
use crate::view::{self, App};

/// Icon resource ordinals, as build.rs embeds them.
const ICON_LIGHT: u16 = 2;
const ICON_DARK: u16 = 3;
const ICON_LIGHT_OFF: u16 = 4;
const ICON_DARK_OFF: u16 = 5;

/// A click on the icon right after the flyout closed (because that click
/// took its focus) only closes it.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

/// Logical pixels between the flyout and the taskbar.
const FLYOUT_GAP: f64 = 12.0;

/// How often the taskbar's theme and the icon size are checked again. A
/// registry read and a system metric: cheap enough to poll.
const THEME_CHECK: Duration = Duration::from_secs(2);

thread_local! {
    /// The app, for tray events, which arrive through `invoke_from_event_loop`.
    static APP: RefCell<Weak<App>> = const { RefCell::new(Weak::new()) };
    static FLYOUT_CLOSED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub struct Tray {
    icon: TrayIcon,
    toggle_item: MenuItem,
    /// The icon variant showing now, so it is replaced only when it changes.
    shown: Cell<Option<IconChoice>>,
    running: Cell<bool>,
    theme_timer: slint::Timer,
}

/// Which icon resource the tray shows, and at what size.
#[derive(Clone, Copy, PartialEq)]
struct IconChoice {
    ordinal: u16,
    size: Option<(u32, u32)>,
}

impl IconChoice {
    /// The variant for the taskbar's current theme and scale.
    fn current(running: bool) -> IconChoice {
        IconChoice {
            ordinal: icon_ordinal(platform::taskbar_is_light(), running),
            size: platform::small_icon_size(),
        }
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
        let exit_item = MenuItem::with_id(MenuIds::EXIT, strings.get_exit_app(), true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open_item,
            &toggle_item,
            &PredefinedMenuItem::separator(),
            &exit_item,
        ])
        .map_err(|e| TrayError(e.to_string()))?;

        let state = app.snapshot();
        let choice = IconChoice::current(state.running);
        let icon = TrayIconBuilder::new()
            .with_icon(load_icon(choice)?)
            .with_tooltip(view::tray_tooltip(&strings, &state))
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| TrayError(e.to_string()))?;

        let tray = Rc::new(Tray {
            icon,
            toggle_item,
            shown: Cell::new(Some(choice)),
            running: Cell::new(state.running),
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
        app.flyout.window().on_winit_window_event(|window, event| {
            match event {
                WindowEvent::Focused(false) => {
                    FLYOUT_CLOSED_AT.with(|at| at.set(Some(Instant::now())));
                    let _ = window.hide();
                }
                // Slint reads the Windows accent after this filter returns.
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
        });
        // The taskbar's theme can change while nothing else does.
        let weak = Rc::downgrade(&tray);
        tray.theme_timer
            .start(slint::TimerMode::Repeated, THEME_CHECK, move || {
                if let Some(tray) = weak.upgrade() {
                    tray.show_icon();
                }
            });
        tray.show_state(&state);
        Ok(tray)
    }

    fn show_state(&self, state: &AppState) {
        let Some(app) = APP.with(|cell| cell.borrow().upgrade()) else {
            return;
        };
        let strings = app.strings();
        let _ = self
            .icon
            .set_tooltip(Some(view::tray_tooltip(&strings, state)));
        self.toggle_item.set_text(if state.running {
            strings.get_menu_turn_streaming_off()
        } else {
            strings.get_menu_turn_streaming_on()
        });
        self.running.set(state.running);
        self.show_icon();
    }

    /// Shows the icon variant for the current theme, scale and state, if it
    /// isn't showing already.
    fn show_icon(&self) {
        let choice = IconChoice::current(self.running.get());
        if self.shown.get() != Some(choice)
            && let Ok(icon) = load_icon(choice)
            && self.icon.set_icon(Some(icon)).is_ok()
        {
            self.shown.set(Some(choice));
        }
    }
}

struct MenuIds;

impl MenuIds {
    const OPEN: &str = "open";
    const TOGGLE: &str = "toggle";
    const EXIT: &str = "exit";
}

fn icon_ordinal(light_taskbar: bool, running: bool) -> u16 {
    match (light_taskbar, running) {
        (true, true) => ICON_LIGHT,
        (false, true) => ICON_DARK,
        (true, false) => ICON_LIGHT_OFF,
        (false, false) => ICON_DARK_OFF,
    }
}

fn load_icon(choice: IconChoice) -> Result<Icon, TrayError> {
    // The small icon size, so the 16, 20 and 24 px frames are used; without
    // a size, Windows loads the 32 px (large icon) frame and shrinks it.
    Icon::from_resource(choice.ordinal, choice.size).map_err(|e| TrayError(e.to_string()))
}

fn on_tray_event(event: TrayIconEvent) {
    let Some(app) = APP.with(|cell| cell.borrow().upgrade()) else {
        return;
    };
    match event {
        TrayIconEvent::Click {
            rect,
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } => {
            let just_closed = FLYOUT_CLOSED_AT.with(|at| {
                at.take()
                    .is_some_and(|closed| closed.elapsed() < REOPEN_GUARD)
            });
            if app.flyout.window().is_visible() {
                app.hide_flyout();
            } else if !just_closed {
                let icon = Bounds {
                    x: rect.position.x,
                    y: rect.position.y,
                    width: f64::from(rect.size.width),
                    height: f64::from(rect.size.height),
                };
                show_flyout(&app, icon);
            }
        }
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => {
            app.hide_flyout();
            app.show_main();
        }
        _ => {}
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
    } else if id == MenuIds::EXIT {
        let _ = slint::quit_event_loop();
    }
}

/// Opens the flyout next to the icon, inside the work area of the icon's
/// monitor, and gives it the keyboard focus.
fn show_flyout(app: &App, icon: Bounds) {
    let flyout = app.flyout.window();
    if flyout.show().is_err() {
        return;
    }
    flyout.with_winit_window(|window| {
        use slint::winit_030::winit::platform::windows::WindowExtWindows;
        window.set_skip_taskbar(true);
    });
    // With the system's rounded frame, the flyout leaves out its own edge.
    let rounded = app.apply_frame(flyout, "tray flyout", crate::backdrop::Frame::Flyout);
    app.flyout.set_rounded(rounded);
    let scale = f64::from(flyout.scale_factor());
    let size = flyout.size();
    let work_area = platform::work_area_at(icon.x, icon.y).unwrap_or(Bounds {
        x: 0.0,
        y: 0.0,
        width: icon.x + icon.width,
        height: icon.y,
    });
    let (x, y) = placement::flyout_origin(
        icon,
        f64::from(size.width),
        f64::from(size.height),
        work_area,
        FLYOUT_GAP * scale,
    );
    flyout.set_position(slint::PhysicalPosition::new(
        x.round() as i32,
        y.round() as i32,
    ));
    platform::bring_to_front(flyout);
    app.flyout.invoke_take_focus();
}
