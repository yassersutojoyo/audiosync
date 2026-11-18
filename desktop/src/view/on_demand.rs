//! A window built when it opens and dropped once it is closed: on the GPU
//! renderer each window shown once holds its own Skia context (about 70 MB of
//! private memory) until the window itself is dropped.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use audiosync_core::eq;
use slint::{ComponentHandle, ModelRc, PhysicalPosition, PhysicalSize, VecModel};

use super::{App, count, with_app};
use crate::{
    AboutDialog, EqualizerWindow, GuideWindow, KeyboardFocus, PasswordDialog, PinDialog,
    ZoneNameDialog,
};

pub(crate) struct OnDemand<T>(RefCell<Option<T>>);

impl<T> Default for OnDemand<T> {
    fn default() -> Self {
        Self(RefCell::new(None))
    }
}

impl<T: ComponentHandle> OnDemand<T> {
    /// The window, if it is built now.
    pub(crate) fn built(&self) -> Option<T> {
        self.0.borrow().as_ref().map(ComponentHandle::clone_strong)
    }

    fn keep(&self, window: &T) {
        *self.0.borrow_mut() = Some(window.clone_strong());
    }

    /// Whether the window is built and shows; a closed window is not built
    /// for the asking.
    pub(crate) fn is_visible(&self) -> bool {
        self.0
            .borrow()
            .as_ref()
            .is_some_and(|window| window.window().is_visible())
    }

    /// Takes the window out when it is built and hidden.
    fn take_hidden(&self) -> Option<T> {
        if self.built().is_some() && !self.is_visible() {
            self.0.borrow_mut().take()
        } else {
            None
        }
    }
}

/// Where a closed window was and its state a rebuild would lose.
#[derive(Default)]
pub(super) struct Kept {
    frames: HashMap<&'static str, (PhysicalPosition, PhysicalSize)>,
    steps_for_iphone: Option<bool>,
}

macro_rules! menu_keys {
    ($app:expr, $window:expr) => {{
        let weak = Rc::downgrade($app);
        let window = $window;
        let w = weak.clone();
        window.on_help_requested(move || with_app(&w, App::show_help));
        let w = weak.clone();
        window.on_new_zone_requested(move || with_app(&w, App::ask_new_zone));
        let w = weak.clone();
        window.on_refresh_devices(move || with_app(&w, |app| app.update(|b| b.refresh_devices())));
        let w = weak.clone();
        window.on_settings_requested(move || with_app(&w, App::show_settings));
        let w = weak.clone();
        window.on_equalizer_requested(move || with_app(&w, App::show_equalizer));
        let w = weak.clone();
        window.on_main_window_requested(move || with_app(&w, App::show_main));
        let w = weak;
        window.on_preset_shortcut(move |n| with_app(&w, |app| app.recall_preset_at(n)));
    }};
}

macro_rules! on_demand_windows {
    ($($name:ident: $ty:ty => $wire:ident;)+) => {
        impl App {$(
            pub(super) fn $name(&self) -> $ty {
                if let Some(window) = self.$name.built() {
                    return window;
                }
                let window = <$ty>::new().expect("a window builds");
                self.$name.keep(&window);
                let frame = self.kept.borrow().frames.get(stringify!($name)).copied();
                if let Some((position, size)) = frame {
                    window.window().set_position(position);
                    window.window().set_size(size);
                }
                let app = self.me.borrow().upgrade();
                if let Some(app) = app {
                    app.dress(&window);
                    app.$wire();
                }
                window
            }
        )+

            /// Drops every window built on demand that is closed, noting
            /// where each shown one is so its rebuild opens there.
            pub(super) fn drop_closed_windows(&self) {
                if let Some(remote) = self.remote_window.built() {
                    self.kept.borrow_mut().steps_for_iphone = Some(remote.get_steps_for_iphone());
                }
                $(
                    if let Some(window) = self.$name.built()
                        && window.window().is_visible()
                    {
                        let frame = (window.window().position(), window.window().size());
                        self.kept.borrow_mut().frames.insert(stringify!($name), frame);
                    }
                    drop(self.$name.take_hidden());
                )+
            }
        }
    };
}

on_demand_windows! {
    equalizer: EqualizerWindow => wire_equalizer;
    zone_dialog: ZoneNameDialog => connect_zone_dialog;
    pin_dialog: PinDialog => connect_pin_dialog;
    password_dialog: PasswordDialog => connect_password_dialog;
    about: AboutDialog => connect_about;
    guide: GuideWindow => wire_guide;
    help: crate::HelpWindow => wire_help;
    remote_window: crate::RemoteWindow => wire_remote;
    setup: crate::SetupWindow => connect_setup;
    settings: crate::SettingsWindow => wire_settings;
}

pub(super) fn quit() {
    let _ = slint::quit_event_loop();
}

impl App {
    fn wire_equalizer(self: &Rc<Self>) {
        let window = self.equalizer();
        let m = &self.models;
        window
            .global::<KeyboardFocus>()
            .set_shown(self.focus_ring_shown.get());
        window.set_gains(ModelRc::from(m.gains.clone()));
        window.set_preset_names(ModelRc::from(m.presets.clone()));
        let band_hz: Vec<f32> = eq::BAND_CENTERS_HZ.iter().map(|&hz| hz as f32).collect();
        window.set_band_hz(ModelRc::new(VecModel::from(band_hz)));
        window.set_max_gain(eq::MAX_GAIN_DB as f32);
        window.set_preset_count(count(eq::PRESETS.len()));
        window.on_exit_requested(quit);
        menu_keys!(self, &window);
        self.connect_equalizer();
        self.show_equalizer_state(&self.snapshot());
    }

    fn wire_guide(self: &Rc<Self>) {
        let window = self.guide();
        window.set_steps(ModelRc::from(self.guide_steps.clone()));
        window.on_exit_requested(quit);
        menu_keys!(self, &window);
        self.connect_guide();
    }

    fn wire_help(self: &Rc<Self>) {
        let window = self.help();
        window
            .global::<KeyboardFocus>()
            .set_shown(self.focus_ring_shown.get());
        window.set_updater_available(self.has_updater());
        window.on_exit_requested(quit);
        menu_keys!(self, &window);
        self.connect_help();
        self.refresh_help(&self.snapshot());
    }

    fn wire_remote(self: &Rc<Self>) {
        let window = self.remote_window();
        let iphone = self.kept.borrow().steps_for_iphone;
        if let Some(iphone) = iphone {
            window.set_steps_for_iphone(iphone);
        }
        window.on_exit_requested(quit);
        menu_keys!(self, &window);
        self.connect_remote();
        self.show_remote_state();
    }

    fn wire_settings(self: &Rc<Self>) {
        menu_keys!(self, &self.settings());
        self.connect_settings();
        self.show_settings_state();
    }
}

#[cfg(test)]
mod tests {
    use slint::{ComponentHandle, Model, PhysicalPosition, PhysicalSize};

    use super::super::tests::{app_over, draw_frames};
    use crate::ui_model::MockBackend;

    macro_rules! each_window {
        ($check:ident) => {
            $check!(equalizer);
            $check!(zone_dialog);
            $check!(pin_dialog);
            $check!(password_dialog);
            $check!(about);
            $check!(guide);
            $check!(help);
            $check!(remote_window);
            $check!(setup);
            $check!(settings);
        };
    }

    #[test]
    fn no_window_is_built_before_it_opens() {
        let app = app_over(MockBackend::new());
        app.refresh();
        app.poll();
        draw_frames();
        macro_rules! unbuilt {
            ($name:ident) => {
                assert!(app.$name.built().is_none(), stringify!($name));
            };
        }
        each_window!(unbuilt);
    }

    #[test]
    fn a_closed_window_is_dropped_and_opens_again_where_it_was() {
        let app = app_over(MockBackend::new());
        macro_rules! reopens_in_place {
            ($name:ident) => {
                let window = app.$name();
                window.show().expect("the window shows");
                window.window().set_position(PhysicalPosition::new(123, 45));
                window.window().set_size(PhysicalSize::new(610, 470));
                app.drop_closed_windows();
                window.hide().expect("the window hides");
                drop(window);
                app.drop_closed_windows();
                assert!(app.$name.built().is_none(), stringify!($name));
                // A headless window has no position: the kept one is what it read.
                let kept = app.kept.borrow().frames.get(stringify!($name)).copied();
                assert_eq!(
                    kept.map(|(_, size)| size),
                    Some(PhysicalSize::new(610, 470)),
                    stringify!($name)
                );
                let again = app.$name();
                assert_eq!(
                    again.window().size(),
                    PhysicalSize::new(610, 470),
                    stringify!($name)
                );
            };
        }
        each_window!(reopens_in_place);
    }

    #[test]
    fn closing_or_refreshing_a_closed_window_builds_nothing() {
        let app = app_over(MockBackend::new());
        app.hide_equalizer();
        app.cancel_pin();
        app.close_pin_preview();
        app.hide_dialog();
        app.cancel_password();
        app.hide_setup();
        app.show_setup_check(None, crate::setup_check::Microphone::Blocked);
        app.refresh();
        macro_rules! unbuilt {
            ($name:ident) => {
                assert!(app.$name.built().is_none(), stringify!($name));
            };
        }
        each_window!(unbuilt);
        app.show_setup();
        assert_eq!(
            app.setup().get_microphone(),
            crate::SetupMicrophone::Blocked,
            "a rebuilt Check Setup shows the last microphone reading"
        );
    }

    #[test]
    fn a_dropped_window_is_freed() {
        let app = app_over(MockBackend::new());
        macro_rules! freed {
            ($name:ident) => {
                let weak = app.$name().as_weak();
                app.$name().show().expect("the window shows");
                draw_frames();
                app.$name().hide().expect("the window hides");
                app.drop_closed_windows();
                assert!(weak.upgrade().is_none(), stringify!($name));
            };
        }
        each_window!(freed);
    }

    /// A window adapter that sizes a window as slint's winit backend 1.18
    /// does: before the native window exists it holds the layout's minimum
    /// size, and the native window, made later in the event loop, takes the
    /// preferred size unless `set_size` was called.
    struct WinitLike {
        window: slint::Window,
        renderer: slint::platform::software_renderer::SoftwareRenderer,
        size: std::cell::Cell<slint::LogicalSize>,
        preferred: std::cell::Cell<slint::LogicalSize>,
        explicit: std::cell::Cell<bool>,
        pending: std::cell::Cell<bool>,
    }

    thread_local! {
        static WINIT_LIKE: std::cell::RefCell<Vec<std::rc::Weak<WinitLike>>> =
            std::cell::RefCell::default();
    }

    impl WinitLike {
        fn resize(&self, size: slint::LogicalSize) {
            self.size.set(size);
            self.window
                .dispatch_event(slint::platform::WindowEvent::Resized { size });
        }

        /// The event loop's turn: makes the native windows shown since.
        fn make_native_windows() {
            let windows: Vec<_> = WINIT_LIKE.with(|all| {
                all.borrow()
                    .iter()
                    .filter_map(std::rc::Weak::upgrade)
                    .collect()
            });
            for adapter in windows {
                if adapter.pending.replace(false) && !adapter.explicit.get() {
                    adapter.resize(adapter.preferred.get());
                }
            }
        }
    }

    impl slint::platform::WindowAdapter for WinitLike {
        fn window(&self) -> &slint::Window {
            &self.window
        }

        fn size(&self) -> PhysicalSize {
            self.size.get().to_physical(1.0)
        }

        fn set_size(&self, size: slint::WindowSize) {
            self.explicit.set(true);
            self.resize(size.to_logical(1.0));
        }

        fn set_visible(&self, visible: bool) -> Result<(), slint::PlatformError> {
            self.pending.set(visible);
            Ok(())
        }

        fn renderer(&self) -> &dyn slint::platform::Renderer {
            &self.renderer
        }

        fn update_window_properties(&self, properties: slint::platform::WindowProperties<'_>) {
            let constraints = properties.layout_constraints();
            self.preferred.set(constraints.preferred);
            let mut size = self.size.get();
            if let Some(min) = constraints.min {
                size.width = size.width.max(min.width);
                size.height = size.height.max(min.height);
            }
            if let Some(max) = constraints.max {
                size.width = size.width.min(max.width);
                size.height = size.height.min(max.height);
            }
            if size != self.size.get() {
                self.resize(size);
            }
        }
    }

    struct WinitLikePlatform;

    impl slint::platform::Platform for WinitLikePlatform {
        fn create_window_adapter(
            &self,
        ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            let adapter = std::rc::Rc::new_cyclic(|me: &std::rc::Weak<WinitLike>| WinitLike {
                window: slint::Window::new(me.clone()),
                renderer: slint::platform::software_renderer::SoftwareRenderer::new(),
                size: std::cell::Cell::default(),
                preferred: std::cell::Cell::default(),
                explicit: std::cell::Cell::new(false),
                pending: std::cell::Cell::new(false),
            });
            WINIT_LIKE.with(|all| all.borrow_mut().push(std::rc::Rc::downgrade(&adapter)));
            Ok(adapter)
        }
    }

    /// The PIN dialog fitted its height before its native window existed,
    /// at the layout's minimum width it held then: it opened 141 px wide,
    /// its message on 6 lines.
    #[test]
    fn the_pin_dialog_opens_at_its_380_px_width() {
        slint::platform::set_platform(Box::new(WinitLikePlatform)).expect("a fresh test thread");
        let app = app_over(MockBackend::new());
        app.preview_pin(false);
        WinitLike::make_native_windows();
        let window = app.pin_dialog();
        assert_eq!(window.window().size().width, 380);
    }

    /// Every window built on open takes the width its .slint file declares.
    #[test]
    fn each_window_built_on_open_opens_at_its_declared_width() {
        slint::platform::set_platform(Box::new(WinitLikePlatform)).expect("a fresh test thread");
        let app = app_over(MockBackend::new());
        macro_rules! opens_at {
            ($name:ident, $width:expr) => {
                let window = app.$name();
                window.show().expect("the window shows");
                WinitLike::make_native_windows();
                assert_eq!(window.window().size().width, $width, stringify!($name));
            };
        }
        opens_at!(equalizer, 520);
        opens_at!(zone_dialog, 380);
        opens_at!(pin_dialog, 380);
        opens_at!(password_dialog, 380);
        opens_at!(about, 360);
        opens_at!(guide, 460);
        opens_at!(help, 640);
        opens_at!(remote_window, 460);
        opens_at!(setup, 480);
        opens_at!(settings, 460);
    }

    #[test]
    fn web_remote_keeps_its_phone_choice_over_a_rebuild() {
        let app = app_over(MockBackend::new());
        app.show_remote();
        app.remote_window().set_steps_for_iphone(false);
        app.remote_window().hide().expect("the window hides");
        app.drop_closed_windows();
        assert!(app.remote_window.built().is_none());
        app.show_remote();
        assert!(!app.remote_window().get_steps_for_iphone());
    }

    #[test]
    fn a_rebuilt_equalizer_and_settings_show_the_current_state() {
        let app = app_over(MockBackend::new());
        let mut on = app.snapshot().equalizer;
        on.enabled = true;
        app.update(|backend| backend.set_equalizer(on));
        app.show_equalizer();
        assert!(app.equalizer().get_enabled());
        assert_eq!(app.equalizer().get_band_hz().row_count(), 10);
        app.show_settings();
        assert_eq!(app.settings().get_appearance(), app.main.get_appearance());
    }
}
