//! Connects the windows to a [`Backend`]: fills them from its snapshots and
//! turns every user action into a backend call, then refreshes. A timer polls
//! the backend twice a second, so what happens on its own (a device unplugged,
//! a source that quit) shows too.
//!
//! The models are updated in place, row by row, so a slider being dragged or a
//! field being typed in keeps its focus while the rest of the window updates.
//! A list whose length changes is rebuilt; when that removes the control that
//! had the focus in the tray flyout, the flyout takes the focus back so Escape
//! still closes it.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::time::Duration;

use audiosync_core::eq::{self, EqSettings};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::backdrop::Frame;
use crate::debug_log::DebugLog;
use crate::guide::{self, Guide, GuideStep, SettingsPage};
use crate::ui_model::{self, AppState, Backend, NoticeMessage, SourceKind, SpeakerState};
use crate::{
    AboutDialog, AirPlayProblem, Availability, EqualizerWindow, GroupItem, GroupNameDialog,
    GuidePage, GuideStepItem, GuideWindow, MainWindow, NoticeItem, Palette, RowState, SpeakerItem,
    SpeakerKind, Strings, TrayFlyout,
};

/// What the group name dialog is open for.
#[derive(Clone, Debug, PartialEq)]
enum DialogPurpose {
    NewGroup,
    Rename(String),
    /// Only a message (no name field), e.g. why a group can't be made yet.
    Message,
}

/// UI state the backend doesn't keep: which sync controls are open, what is
/// being typed in their fields, and whether virtual outputs are listed.
#[derive(Default)]
struct UiState {
    sync_open: HashSet<String>,
    delay_drafts: HashMap<String, String>,
    /// View > Show Virtual Devices; off at launch, like the Mac's default.
    show_virtual: bool,
}

struct Models {
    sources: Rc<VecModel<SharedString>>,
    this_pc: Rc<VecModel<SpeakerItem>>,
    connected: Rc<VecModel<SpeakerItem>>,
    all_speakers: Rc<VecModel<SpeakerItem>>,
    groups: Rc<VecModel<GroupItem>>,
    notices: Rc<VecModel<NoticeItem>>,
    flyout_notices: Rc<VecModel<NoticeItem>>,
    gains: Rc<VecModel<f32>>,
    presets: Rc<VecModel<SharedString>>,
}

/// How many notices the main window lists, newest first.
const MAIN_NOTICE_LIMIT: usize = 4;
/// The flyout lists only the newest notice and a button for the rest.
const FLYOUT_NOTICE_LIMIT: usize = 1;

/// How often the backend is polled for what happened on its own.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The parts of the tray flyout that a refresh can remove, with the controls
/// in them: the listed notice, the button for more notices, the group rows
/// and the speaker rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FlyoutParts {
    notices: bool,
    more_notices: bool,
    groups: bool,
    speakers: bool,
}

impl FlyoutParts {
    /// Whether a part is set in both.
    fn overlaps(self, other: FlyoutParts) -> bool {
        (self.notices && other.notices)
            || (self.more_notices && other.more_notices)
            || (self.groups && other.groups)
            || (self.speakers && other.speakers)
    }
}

/// What the tray flyout shows: how many notices there are and how many of
/// them it lists, and how many groups and speakers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FlyoutCounts {
    notices: usize,
    listed_notices: usize,
    groups: usize,
    speakers: usize,
}

impl FlyoutCounts {
    /// Whether the flyout shows the button for the notices it doesn't list.
    fn shows_more_notices(self) -> bool {
        self.notices > self.listed_notices
    }

    /// The parts that a refresh to `next` removes. A list whose length
    /// changes is rebuilt ([`sync_model`]), so all its rows go, even the
    /// ones whose items come back.
    fn removed_by(self, next: FlyoutCounts) -> FlyoutParts {
        FlyoutParts {
            notices: self.listed_notices != next.listed_notices,
            more_notices: self.shows_more_notices() && !next.shows_more_notices(),
            groups: self.groups != next.groups,
            speakers: self.speakers != next.speakers,
        }
    }
}

/// Runs after every refresh with the new state.
type RefreshHook = Box<dyn Fn(&AppState)>;

/// The application's windows and the backend behind them.
pub struct App {
    backend: RefCell<Box<dyn Backend>>,
    pub main: MainWindow,
    pub flyout: TrayFlyout,
    pub equalizer: EqualizerWindow,
    group_dialog: GroupNameDialog,
    about: AboutDialog,
    guide: GuideWindow,
    /// The paragraphs the guide shows.
    guide_steps: Rc<VecModel<GuideStepItem>>,
    /// The guide is open for the double-audio notice and follows it (not
    /// for a made-up preview notice).
    guide_follows_notice: Cell<bool>,
    log: DebugLog,
    ui: RefCell<UiState>,
    dialog_purpose: RefCell<DialogPurpose>,
    models: Models,
    /// With a tray icon, closing the main window keeps the app running there.
    tray_available: Cell<bool>,
    /// Dark (true) or light for every window instead of the Windows setting:
    /// the theme preview, `None` normally.
    theme_override: Cell<Option<bool>>,
    /// Whether each window got its Windows 11 frame, as last logged.
    frames: RefCell<HashMap<&'static str, bool>>,
    /// Called after every refresh, e.g. to update the tray icon and tooltip.
    on_refresh: RefCell<Option<RefreshHook>>,
    /// Polls the backend; it stops when the app is dropped.
    poll_timer: Timer,
}

/// Runs `action` on the app if it still exists.
fn with_app(app: &Weak<App>, action: impl FnOnce(&App)) {
    if let Some(app) = app.upgrade() {
        action(&app);
    }
}

impl App {
    /// The windows over `backend`; what they do themselves goes to `log`.
    pub fn new(backend: Box<dyn Backend>, log: DebugLog) -> Result<Rc<App>, slint::PlatformError> {
        let models = Models {
            sources: Rc::new(VecModel::default()),
            this_pc: Rc::new(VecModel::default()),
            connected: Rc::new(VecModel::default()),
            all_speakers: Rc::new(VecModel::default()),
            groups: Rc::new(VecModel::default()),
            notices: Rc::new(VecModel::default()),
            flyout_notices: Rc::new(VecModel::default()),
            gains: Rc::new(VecModel::default()),
            presets: Rc::new(VecModel::default()),
        };
        let app = Rc::new(App {
            backend: RefCell::new(backend),
            main: MainWindow::new()?,
            flyout: TrayFlyout::new()?,
            equalizer: EqualizerWindow::new()?,
            group_dialog: GroupNameDialog::new()?,
            about: AboutDialog::new()?,
            guide: GuideWindow::new()?,
            guide_steps: Rc::new(VecModel::default()),
            guide_follows_notice: Cell::new(false),
            log,
            ui: RefCell::default(),
            dialog_purpose: RefCell::new(DialogPurpose::Message),
            models,
            tray_available: Cell::new(false),
            theme_override: Cell::new(None),
            frames: RefCell::default(),
            on_refresh: RefCell::new(None),
            poll_timer: Timer::default(),
        });
        app.attach_models();
        app.connect_main();
        app.connect_flyout();
        app.connect_equalizer();
        app.connect_dialogs();
        app.connect_guide();
        app.refresh();
        let weak = Rc::downgrade(&app);
        app.poll_timer
            .start(TimerMode::Repeated, POLL_INTERVAL, move || {
                with_app(&weak, App::poll)
            });
        Ok(app)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_tray_available(&self, available: bool) {
        self.tray_available.set(available);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_on_refresh(&self, callback: impl Fn(&AppState) + 'static) {
        *self.on_refresh.borrow_mut() = Some(Box::new(callback));
    }

    pub fn strings(&self) -> Strings<'_> {
        self.main.global::<Strings>()
    }

    pub fn snapshot(&self) -> AppState {
        self.backend.borrow().snapshot()
    }

    /// Changes the backend, then shows the result everywhere.
    pub fn update(&self, change: impl FnOnce(&mut dyn Backend)) {
        change(self.backend.borrow_mut().as_mut());
        self.refresh();
    }

    /// Lets the backend take in what happened on its own, and shows it.
    fn poll(&self) {
        let changed = self.backend.borrow_mut().poll();
        if changed {
            self.refresh();
        }
    }

    /// Shows the main window and brings it to the front.
    pub fn show_main(&self) {
        if let Err(error) = self.main.show() {
            self.show_window_error(&error);
            return;
        }
        self.apply_frame(self.main.window(), "main window", Frame::Titled);
        crate::platform::bring_to_front(self.main.window());
    }

    /// Shows every window light or dark, whatever Windows uses, to check both
    /// appearances without changing the Windows setting (the theme preview).
    pub fn preview_theme(&self, dark: bool) {
        use slint::language::ColorScheme;
        let scheme = if dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        self.main.global::<Palette>().set_color_scheme(scheme);
        self.flyout.global::<Palette>().set_color_scheme(scheme);
        self.equalizer.global::<Palette>().set_color_scheme(scheme);
        self.group_dialog
            .global::<Palette>()
            .set_color_scheme(scheme);
        self.about.global::<Palette>().set_color_scheme(scheme);
        self.guide.global::<Palette>().set_color_scheme(scheme);
        self.theme_override.set(Some(dark));
    }

    /// Gives a window that is showing its Windows 11 frame (`backdrop`), and
    /// logs whether DWM took it. `true` when it did.
    pub fn apply_frame(&self, window: &slint::Window, name: &'static str, frame: Frame) -> bool {
        if let Some(dark) = self.theme_override.get() {
            use slint::winit_030::WinitWindowAccessor;
            use slint::winit_030::winit::window::Theme;
            let theme = if dark { Theme::Dark } else { Theme::Light };
            window.with_winit_window(|window| window.set_theme(Some(theme)));
        }
        // Before the event loop runs there is no native window yet; the
        // window's first focus (see `follow_frame`) tries again.
        let Some(applied) = crate::backdrop::apply(window, frame) else {
            return false;
        };
        if self.frames.borrow_mut().insert(name, applied) != Some(applied) {
            self.log.log(|| {
                let what = match frame {
                    Frame::Titled => "Mica title bar",
                    Frame::Flyout => "rounded corners",
                };
                let result = if applied { "on" } else { "not available" };
                format!("{name}: {what} {result}")
            });
        }
        applied
    }

    /// Gives a titled window (every window but the flyout) its frame each
    /// time it gains the focus, which covers a native window created after
    /// `show` returned.
    fn follow_frame(self: &Rc<Self>, window: fn(&App) -> &slint::Window, name: &'static str) {
        use slint::winit_030::winit::event::WindowEvent;
        use slint::winit_030::{EventResult, WinitWindowAccessor};

        let weak = Rc::downgrade(self);
        window(self).on_winit_window_event(move |_, event| {
            if let WindowEvent::Focused(true) = event {
                with_app(&weak, |app| {
                    app.apply_frame(window(app), name, Frame::Titled);
                });
            }
            EventResult::Propagate
        });
    }

    fn attach_models(&self) {
        let m = &self.models;
        self.main.set_sources(ModelRc::from(m.sources.clone()));
        self.main.set_this_pc(ModelRc::from(m.this_pc.clone()));
        self.main.set_connected(ModelRc::from(m.connected.clone()));
        self.main.set_groups(ModelRc::from(m.groups.clone()));
        self.main.set_notices(ModelRc::from(m.notices.clone()));
        self.flyout.set_sources(ModelRc::from(m.sources.clone()));
        self.flyout
            .set_speakers(ModelRc::from(m.all_speakers.clone()));
        self.flyout.set_groups(ModelRc::from(m.groups.clone()));
        self.flyout
            .set_notices(ModelRc::from(m.flyout_notices.clone()));
        self.equalizer.set_gains(ModelRc::from(m.gains.clone()));
        self.equalizer
            .set_preset_names(ModelRc::from(m.presets.clone()));
        let band_hz: Vec<f32> = eq::BAND_CENTERS_HZ.iter().map(|&hz| hz as f32).collect();
        self.equalizer
            .set_band_hz(ModelRc::new(VecModel::from(band_hz)));
        self.equalizer.set_max_gain(eq::MAX_GAIN_DB as f32);
        self.guide
            .set_steps(ModelRc::from(self.guide_steps.clone()));
        self.about.set_version(env!("CARGO_PKG_VERSION").into());
        self.about.set_core_version(audiosync_core::VERSION.into());
    }

    /// Reads a snapshot and shows it in every window.
    pub fn refresh(&self) {
        let state = self.snapshot();
        let strings = self.strings();
        let ui = self.ui.borrow();
        let item = |speaker: &ui_model::Speaker, show_kind: bool| {
            let draft = ui.delay_drafts.get(&speaker.id).map(String::as_str);
            speaker_item(
                &state,
                speaker,
                show_kind,
                ui.sync_open.contains(&speaker.id),
                draft,
            )
        };
        let (this_pc, connected) = state.layout();
        let listed = |s: &&ui_model::Speaker| s.is_listed(ui.show_virtual);
        let this_pc: Vec<SpeakerItem> = this_pc
            .into_iter()
            .filter(listed)
            .map(|s| item(s, false))
            .collect();
        let connected: Vec<SpeakerItem> = connected
            .into_iter()
            .filter(listed)
            .map(|s| item(s, true))
            .collect();
        let all: Vec<SpeakerItem> = this_pc.iter().chain(&connected).cloned().collect();
        let groups: Vec<GroupItem> = state.groups.iter().map(|g| group_item(&state, g)).collect();
        let notices: Vec<NoticeItem> = state
            .notices
            .iter()
            .map(|n| notice_item(&strings, n))
            .collect();
        let sources: Vec<SharedString> = state
            .sources
            .iter()
            .map(|s| match s.kind {
                // The backend's name for it is English; the UI's is translated.
                SourceKind::System => strings.get_source_system(),
                SourceKind::App | SourceKind::Input => s.name.as_str().into(),
            })
            .collect();
        let source_index = state
            .sources
            .iter()
            .position(|s| s.id == state.source_id)
            .map_or(-1, |i| i as i32);
        let status = strings.invoke_status(state.running, count(state.active_speakers()));
        let flyout_notices = newest_notices(&notices, FLYOUT_NOTICE_LIMIT);

        let m = &self.models;
        let flyout_shown = FlyoutCounts {
            notices: usize::try_from(self.flyout.get_notice_count()).unwrap_or(0),
            listed_notices: m.flyout_notices.row_count(),
            groups: m.groups.row_count(),
            speakers: m.all_speakers.row_count(),
        };
        let flyout_removed = flyout_shown.removed_by(FlyoutCounts {
            notices: notices.len(),
            listed_notices: flyout_notices.len(),
            groups: groups.len(),
            speakers: all.len(),
        });
        sync_model(&m.sources, sources);
        sync_model(&m.this_pc, this_pc);
        sync_model(&m.connected, connected);
        sync_model(&m.all_speakers, all);
        sync_model(&m.groups, groups);
        sync_model(&m.notices, newest_notices(&notices, MAIN_NOTICE_LIMIT));
        sync_model(&m.flyout_notices, flyout_notices);
        self.flyout.set_notice_count(count(notices.len()));
        self.restore_flyout_focus(flyout_removed);
        self.refresh_guide(&state);

        for window in [&self.main as &dyn WindowState, &self.flyout] {
            window.show_state(&state, &status, source_index);
        }
        self.main.set_equalizer_on(state.equalizer.enabled);
        self.main.set_show_virtual_devices(ui.show_virtual);

        self.show_equalizer_state(&state.equalizer);
        drop(ui);
        if let Some(callback) = self.on_refresh.borrow().as_ref() {
            callback(&state);
        }
    }

    /// Gives the keyboard focus back to the open flyout when a refresh
    /// removed the control that had it (a notice dismissed with the
    /// keyboard, a speaker unplugged): Slint then leaves nothing focused,
    /// and Escape would not close the flyout until Tab focused something.
    fn restore_flyout_focus(&self, removed: FlyoutParts) {
        let flyout = &self.flyout;
        let focused = FlyoutParts {
            notices: flyout.get_notice_focused(),
            more_notices: flyout.get_more_notices_focused(),
            groups: flyout.get_group_focused(),
            speakers: flyout.get_speaker_focused(),
        };
        if focused.overlaps(removed) && flyout.window().is_visible() {
            flyout.invoke_take_focus();
        }
    }

    fn show_equalizer_state(&self, settings: &EqSettings) {
        let strings = self.strings();
        let mut presets: Vec<SharedString> = eq::PRESETS
            .iter()
            .map(|p| strings.invoke_equalizer_preset(p.id.into()))
            .collect();
        let preset_index = match ui_model::matching_preset_index(settings) {
            Some(index) => index,
            None => {
                presets.push(strings.get_equalizer_custom());
                presets.len() - 1
            }
        };
        sync_model(&self.models.presets, presets);
        sync_model(
            &self.models.gains,
            settings.gains_db.iter().map(|&g| g as f32).collect(),
        );
        self.equalizer.set_enabled(settings.enabled);
        self.equalizer.set_preamp(settings.preamp_db as f32);
        self.equalizer.set_preset_index(preset_index as i32);
    }

    fn connect_main(self: &Rc<Self>) {
        self.follow_frame(|app| app.main.window(), "main window");
        let main = &self.main;
        let weak = Rc::downgrade(self);

        let w = weak.clone();
        main.on_power_toggled(move |on| with_app(&w, |app| app.update(|b| b.set_running(on))));
        let w = weak.clone();
        main.on_source_selected(move |index| with_app(&w, |app| app.select_source(index)));
        let w = weak.clone();
        main.on_master_volume_changed(move |volume| {
            with_app(&w, |app| app.update(|b| b.set_master_volume(volume.into())))
        });
        let w = weak.clone();
        main.on_equalizer_requested(move || with_app(&w, App::show_equalizer));
        let w = weak.clone();
        main.on_speaker_toggled(move |id| with_app(&w, |app| app.toggle_speaker(&id)));
        let w = weak.clone();
        main.on_speaker_volume_changed(move |id, volume| {
            with_app(&w, |app| {
                app.update(|b| b.set_speaker_volume(&id, volume.into()))
            })
        });
        let w = weak.clone();
        main.on_sync_toggled(move |id| with_app(&w, |app| app.toggle_sync(&id)));
        let w = weak.clone();
        main.on_delay_stepped(move |id, delta| with_app(&w, |app| app.step_delay(&id, delta)));
        let w = weak.clone();
        main.on_delay_edited(move |id, text| with_app(&w, |app| app.edit_delay(&id, &text)));
        let w = weak.clone();
        main.on_auto_connect_toggled(move |id, on| {
            with_app(&w, |app| app.update(|b| b.set_auto_connect(&id, on)))
        });
        let w = weak.clone();
        main.on_group_toggled(move |id| with_app(&w, |app| app.update(|b| b.toggle_group(&id))));
        let w = weak.clone();
        main.on_group_rename_requested(move |id| with_app(&w, |app| app.ask_rename(&id)));
        let w = weak.clone();
        main.on_group_deleted(move |id| with_app(&w, |app| app.update(|b| b.delete_group(&id))));
        let w = weak.clone();
        main.on_new_group_requested(move || with_app(&w, App::ask_new_group));
        let w = weak.clone();
        main.on_refresh_devices(move || with_app(&w, |app| app.update(|b| b.refresh_devices())));
        let w = weak.clone();
        main.on_show_virtual_devices_toggled(move |on| {
            with_app(&w, |app| app.show_virtual_devices(on))
        });
        let w = weak.clone();
        main.on_notice_dismissed(move |id| {
            with_app(&w, |app| app.update(|b| b.dismiss_notice(id as u64)))
        });
        let w = weak.clone();
        main.on_notices_dismissed_all(move || {
            with_app(&w, |app| app.update(|b| b.dismiss_all_notices()))
        });
        let w = weak.clone();
        main.on_notice_guide_requested(move |id| with_app(&w, |app| app.show_guide(id)));
        let w = weak.clone();
        main.on_close_requested(move || with_app(&w, App::close_main));
        main.on_exit_requested(|| {
            let _ = slint::quit_event_loop();
        });
        let w = weak.clone();
        main.on_about_requested(move || {
            with_app(&w, |app| {
                if let Err(error) = app.about.show() {
                    app.show_window_error(&error);
                }
            })
        });
        let w = weak;
        main.window().on_close_requested(move || {
            with_app(&w, App::close_main);
            slint::CloseRequestResponse::HideWindow
        });

        let about = self.about.as_weak();
        self.about.on_ok_clicked(move || {
            if let Some(about) = about.upgrade() {
                // A window that fails to hide stays open; the user can still close it.
                let _ = about.hide();
            }
        });
    }

    fn connect_flyout(self: &Rc<Self>) {
        let flyout = &self.flyout;
        let weak = Rc::downgrade(self);

        let w = weak.clone();
        flyout.on_power_toggled(move |on| with_app(&w, |app| app.update(|b| b.set_running(on))));
        let w = weak.clone();
        flyout.on_source_selected(move |index| with_app(&w, |app| app.select_source(index)));
        let w = weak.clone();
        flyout.on_master_volume_changed(move |volume| {
            with_app(&w, |app| app.update(|b| b.set_master_volume(volume.into())))
        });
        let w = weak.clone();
        flyout.on_speaker_toggled(move |id| with_app(&w, |app| app.toggle_speaker(&id)));
        let w = weak.clone();
        flyout.on_group_toggled(move |id| with_app(&w, |app| app.update(|b| b.toggle_group(&id))));
        let w = weak.clone();
        flyout.on_notice_dismissed(move |id| {
            with_app(&w, |app| app.update(|b| b.dismiss_notice(id as u64)))
        });
        let w = weak.clone();
        flyout.on_notices_dismissed_all(move || {
            with_app(&w, |app| app.update(|b| b.dismiss_all_notices()))
        });
        let w = weak.clone();
        flyout.on_notice_guide_requested(move |id| {
            with_app(&w, |app| {
                app.hide_flyout();
                app.show_guide(id);
            })
        });
        let w = weak.clone();
        flyout.on_open_app(move || {
            with_app(&w, |app| {
                app.hide_flyout();
                app.show_main();
            })
        });
        flyout.on_exit_app(|| {
            let _ = slint::quit_event_loop();
        });
        let w = weak;
        flyout.on_close_requested(move || with_app(&w, App::hide_flyout));
    }

    fn connect_equalizer(self: &Rc<Self>) {
        self.follow_frame(|app| app.equalizer.window(), "equalizer");
        let eq_window = &self.equalizer;
        let weak = Rc::downgrade(self);

        let w = weak.clone();
        eq_window.on_enabled_toggled(move |on| {
            with_app(&w, |app| app.change_equalizer(|s| s.enabled = on))
        });
        let w = weak.clone();
        eq_window.on_preamp_changed(move |db| {
            with_app(&w, |app| app.change_equalizer(|s| s.preamp_db = db.into()))
        });
        let w = weak.clone();
        eq_window.on_gain_changed(move |band, db| {
            with_app(&w, |app| {
                app.change_equalizer(|s| {
                    if let Some(gain) = usize::try_from(band)
                        .ok()
                        .and_then(|b| s.gains_db.get_mut(b))
                    {
                        *gain = db.into();
                    }
                })
            })
        });
        let w = weak.clone();
        eq_window.on_preset_selected(move |index| {
            with_app(&w, |app| {
                if let Some(preset) = usize::try_from(index).ok().and_then(|i| eq::PRESETS.get(i)) {
                    // Like the Mac: picking a preset turns the equalizer on.
                    app.change_equalizer(|s| {
                        s.apply_preset(preset);
                        s.enabled = true;
                    });
                }
            })
        });
        let w = weak.clone();
        eq_window.on_reset(move || {
            with_app(&w, |app| {
                app.change_equalizer(|s| {
                    s.apply_preset(&eq::PRESETS[0]);
                    s.preamp_db = 0.0;
                })
            })
        });
        let w = weak;
        eq_window.on_close_requested(move || {
            with_app(&w, |app| {
                let _ = app.equalizer.hide();
            })
        });
    }

    fn connect_dialogs(self: &Rc<Self>) {
        self.follow_frame(|app| app.group_dialog.window(), "group name dialog");
        self.follow_frame(|app| app.about.window(), "about dialog");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.group_dialog
            .on_confirmed(move |name| with_app(&w, |app| app.confirm_dialog(&name)));
        let w = weak;
        self.group_dialog.on_cancelled(move || {
            with_app(&w, |app| {
                let _ = app.group_dialog.hide();
            })
        });
    }

    fn connect_guide(self: &Rc<Self>) {
        self.follow_frame(|app| app.guide.window(), "guide");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.guide
            .on_page_requested(move |page| with_app(&w, |app| app.open_guide_page(page)));
        let w = weak.clone();
        self.guide
            .on_close_requested(move || with_app(&w, App::close_guide));
        let w = weak;
        self.guide.window().on_close_requested(move || {
            with_app(&w, App::close_guide);
            slint::CloseRequestResponse::HideWindow
        });
    }

    fn close_guide(&self) {
        self.guide_follows_notice.set(false);
        let _ = self.guide.hide();
    }

    /// Opens the Settings page a guide button asks for; only the pages the
    /// guide names, nothing else. When the shell doesn't take it, the guide
    /// says where to find the page.
    fn open_guide_page(&self, page: GuidePage) {
        let Some(page) = settings_page(page) else {
            return;
        };
        let opened = crate::platform::open_settings(page.uri());
        if !opened {
            self.log
                .log(|| format!("guide: the shell did not open {}", page.uri()));
        }
        self.guide.set_settings_failed(!opened);
    }

    /// Keeps an open guide in step with the double-audio notice it was
    /// opened from: updated while that notice changes, closed once it is
    /// gone (the app quit, streaming stopped, the routing was fixed).
    fn refresh_guide(&self, state: &AppState) {
        if !self.guide_follows_notice.get() {
            return;
        }
        match guide::current(&state.notices) {
            Some(guide) => self.fill_guide(&guide),
            None => {
                self.log
                    .log(|| "guide: closed, its notice is gone".to_owned());
                self.close_guide();
            }
        }
    }

    /// Shows `guide` in the guide window, row by row so a focused button
    /// keeps its focus.
    fn fill_guide(&self, guide: &Guide) {
        let strings = self.strings();
        self.guide.set_intro(guide_intro(&strings, guide));
        let items: Vec<GuideStepItem> = guide
            .steps
            .iter()
            .map(|step| GuideStepItem {
                text: guide_step_text(&strings, step),
                page: guide_page(step.page()),
            })
            .collect();
        sync_model(&self.guide_steps, items);
    }

    /// Opens the guide of notice `id`, if it is still there and has one.
    fn show_guide(&self, id: i32) {
        let state = self.snapshot();
        let Some(notice) = state
            .notices
            .iter()
            .find(|notice| i32::try_from(notice.id).ok() == Some(id))
        else {
            return;
        };
        if self.show_guide_for(&notice.message) {
            self.guide_follows_notice.set(true);
        }
    }

    /// Opens the guide for a notice saying `message`, if it has one, and
    /// brings it to the front. Returns whether it opened. A guide opened
    /// this way stays as it is; one opened from a notice follows it.
    pub fn show_guide_for(&self, message: &NoticeMessage) -> bool {
        let Some(guide) = guide::guide(message) else {
            return false;
        };
        self.guide_follows_notice.set(false);
        self.fill_guide(&guide);
        self.guide.set_settings_failed(false);
        if let Err(error) = self.guide.show() {
            self.show_window_error(&error);
            return false;
        }
        self.guide.invoke_take_focus();
        crate::platform::bring_to_front(self.guide.window());
        true
    }

    fn select_source(&self, index: i32) {
        let state = self.snapshot();
        if let Some(source) = usize::try_from(index)
            .ok()
            .and_then(|i| state.sources.get(i))
        {
            self.update(|b| b.select_source(&source.id));
        }
    }

    fn toggle_speaker(&self, id: &str) {
        let state = self.snapshot();
        if let Some(speaker) = state.speaker(id) {
            let on = state.speaker_state(speaker).click_turns_on();
            self.update(|b| b.set_speaker_enabled(id, on));
        }
    }

    fn toggle_sync(&self, id: &str) {
        {
            let mut ui = self.ui.borrow_mut();
            if !ui.sync_open.remove(id) {
                ui.sync_open.insert(id.to_string());
            }
            ui.delay_drafts.remove(id);
        }
        self.refresh();
    }

    fn show_virtual_devices(&self, on: bool) {
        self.ui.borrow_mut().show_virtual = on;
        self.refresh();
    }

    fn step_delay(&self, id: &str, delta: i32) {
        let state = self.snapshot();
        let Some(current) = state.speaker(id).and_then(|s| s.delay_ms) else {
            return;
        };
        self.ui.borrow_mut().delay_drafts.remove(id);
        self.update(|b| b.set_speaker_delay(id, current + f64::from(delta)));
    }

    /// Applies the typed delay as soon as it is a number. Out of range, the
    /// field shows the value that was applied.
    fn edit_delay(&self, id: &str, text: &str) {
        let draft = match ui_model::parse_delay_ms(text) {
            Some(ms) => {
                self.backend.borrow_mut().set_speaker_delay(id, ms);
                let typed: f64 = text.trim().replace(',', ".").parse().unwrap_or(ms);
                if typed == ms {
                    text.to_string()
                } else {
                    format_ms(ms)
                }
            }
            None => text.to_string(),
        };
        self.ui
            .borrow_mut()
            .delay_drafts
            .insert(id.to_string(), draft);
        self.refresh();
    }

    fn change_equalizer(&self, change: impl FnOnce(&mut EqSettings)) {
        let mut settings = self.snapshot().equalizer;
        change(&mut settings);
        self.update(|b| b.set_equalizer(settings));
    }

    pub fn show_equalizer(&self) {
        if let Err(error) = self.equalizer.show() {
            self.show_window_error(&error);
            return;
        }
        self.apply_frame(self.equalizer.window(), "equalizer", Frame::Titled);
        self.equalizer.invoke_take_focus();
        crate::platform::bring_to_front(self.equalizer.window());
    }

    fn ask_new_group(&self) {
        let strings = self.strings();
        let dialog = &self.group_dialog;
        if self.snapshot().active_speakers() == 0 {
            *self.dialog_purpose.borrow_mut() = DialogPurpose::Message;
            dialog.set_heading(strings.get_new_group_title());
            dialog.set_message(strings.get_new_group_needs_speakers());
            dialog.set_show_field(false);
            dialog.set_confirm_label(strings.get_ok());
        } else {
            *self.dialog_purpose.borrow_mut() = DialogPurpose::NewGroup;
            dialog.set_heading(strings.get_new_group_title());
            dialog.set_message(strings.get_new_group_message());
            dialog.set_placeholder(strings.get_new_group_placeholder());
            dialog.set_name(SharedString::new());
            dialog.set_show_field(true);
            dialog.set_confirm_label(strings.get_create());
        }
        self.open_dialog();
    }

    fn ask_rename(&self, id: &str) {
        let state = self.snapshot();
        let Some(group) = state.groups.iter().find(|g| g.id == id) else {
            return;
        };
        let strings = self.strings();
        let dialog = &self.group_dialog;
        *self.dialog_purpose.borrow_mut() = DialogPurpose::Rename(id.to_string());
        dialog.set_heading(strings.get_rename_group_title());
        dialog.set_message(strings.invoke_rename_group_message(group.name.as_str().into()));
        dialog.set_placeholder(SharedString::new());
        dialog.set_name(group.name.as_str().into());
        dialog.set_show_field(true);
        dialog.set_confirm_label(strings.get_rename());
        self.open_dialog();
    }

    fn open_dialog(&self) {
        if let Err(error) = self.group_dialog.show() {
            self.show_window_error(&error);
            return;
        }
        self.group_dialog.invoke_prepare();
        crate::platform::bring_to_front(self.group_dialog.window());
    }

    fn confirm_dialog(&self, name: &str) {
        let purpose = self.dialog_purpose.borrow().clone();
        let _ = self.group_dialog.hide();
        match purpose {
            DialogPurpose::NewGroup => {
                let name = if name.trim().is_empty() {
                    let number = self.snapshot().groups.len() + 1;
                    self.strings()
                        .invoke_numbered_group(count(number))
                        .to_string()
                } else {
                    name.to_string()
                };
                // The dialog only offers a name while a speaker is on; if the
                // last one went off meanwhile, nothing is made.
                let _ = self.backend.borrow_mut().new_group(&name);
                self.refresh();
            }
            DialogPurpose::Rename(id) => self.update(|b| b.rename_group(&id, name)),
            DialogPurpose::Message => {}
        }
    }

    /// Closing the main window keeps AudioSync running in the tray, like the
    /// Mac app in the menu bar. Without a tray icon, it quits.
    fn close_main(&self) {
        let _ = self.main.hide();
        if !self.tray_available.get() {
            let _ = slint::quit_event_loop();
        }
    }

    fn show_window_error(&self, error: &slint::PlatformError) {
        let text = self
            .strings()
            .invoke_window_failed(error.to_string().into());
        crate::platform::show_error(&text);
    }

    pub fn hide_flyout(&self) {
        let _ = self.flyout.hide();
    }
}

/// The properties the main window and the flyout share.
trait WindowState {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32);
}

impl WindowState for MainWindow {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32) {
        self.set_running(state.running);
        self.set_status(status.clone());
        self.set_source_index(source_index);
        self.set_master_volume(state.master_volume as f32);
    }
}

impl WindowState for TrayFlyout {
    fn show_state(&self, state: &AppState, status: &SharedString, source_index: i32) {
        self.set_running(state.running);
        self.set_status(status.clone());
        self.set_source_index(source_index);
        self.set_master_volume(state.master_volume as f32);
    }
}

/// The newest `limit` notices: what a window lists. A window that shows
/// fewer than all of them says how many more there are instead.
fn newest_notices<T: Clone>(notices: &[T], limit: usize) -> Vec<T> {
    notices.iter().take(limit).cloned().collect()
}

/// Replaces the rows that changed; the whole list only when its length did.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, items: Vec<T>) {
    if model.row_count() == items.len() {
        for (row, item) in items.into_iter().enumerate() {
            if model.row_data(row).as_ref() != Some(&item) {
                model.set_row_data(row, item);
            }
        }
    } else {
        model.set_vec(items);
    }
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn format_ms(ms: f64) -> String {
    format!("{}", ms.round() as i64)
}

pub fn row_state(state: SpeakerState) -> RowState {
    match state {
        SpeakerState::Off => RowState::Off,
        SpeakerState::WillPlay => RowState::WillPlay,
        SpeakerState::Playing => RowState::Playing,
    }
}

pub fn speaker_kind(kind: ui_model::SpeakerKind) -> SpeakerKind {
    use ui_model::SpeakerKind as K;
    match kind {
        K::AirPlay => SpeakerKind::Airplay,
        K::BuiltIn => SpeakerKind::BuiltIn,
        K::Headphones => SpeakerKind::Headphones,
        K::Bluetooth => SpeakerKind::Bluetooth,
        K::Usb => SpeakerKind::Usb,
        K::Hdmi => SpeakerKind::Hdmi,
        K::DisplayPort => SpeakerKind::DisplayPort,
        K::Virtual => SpeakerKind::Virtual,
        K::Other => SpeakerKind::Other,
    }
}

fn availability(a: ui_model::Availability) -> Availability {
    match a {
        ui_model::Availability::Open => Availability::Open,
        ui_model::Availability::NeedsCode => Availability::NeedsCode,
        ui_model::Availability::PasswordNotYet => Availability::PasswordNotYet,
        ui_model::Availability::PairingNotYet => Availability::PairingNotYet,
        ui_model::Availability::AirPlay2NotYet => Availability::Airplay2NotYet,
        ui_model::Availability::Unsupported => Availability::Unsupported,
    }
}

fn airplay_problem(problem: ui_model::AirPlayProblem) -> AirPlayProblem {
    use ui_model::AirPlayProblem as P;
    match problem {
        P::NotResponding => AirPlayProblem::NotResponding,
        P::ConnectionLost => AirPlayProblem::ConnectionLost,
        P::Busy => AirPlayProblem::Busy,
        P::Refused => AirPlayProblem::Refused,
        P::PasswordNotYet => AirPlayProblem::PasswordNotYet,
        P::PairingNotYet => AirPlayProblem::PairingNotYet,
        P::AirPlay2NotYet => AirPlayProblem::Airplay2NotYet,
        P::Unsupported => AirPlayProblem::Unsupported,
    }
}

/// A speaker as a row shows it. `draft`: what is typed in its delay field.
pub fn speaker_item(
    state: &AppState,
    speaker: &ui_model::Speaker,
    show_kind: bool,
    sync_open: bool,
    draft: Option<&str>,
) -> SpeakerItem {
    let delay = speaker.delay_ms.unwrap_or(0.0);
    SpeakerItem {
        id: speaker.id.as_str().into(),
        name: speaker.name.as_str().into(),
        kind: speaker_kind(speaker.kind),
        show_kind,
        state: row_state(state.speaker_state(speaker)),
        volume: speaker.volume as f32,
        has_delay: speaker.delay_ms.is_some(),
        delay_ms: delay.round() as i32,
        delay_text: draft
            .map_or_else(|| format_ms(delay), str::to_string)
            .into(),
        sync_open: sync_open && speaker.delay_ms.is_some(),
        auto_connect: speaker.auto_connect,
        availability: availability(speaker.availability),
    }
}

pub fn group_item(state: &AppState, group: &ui_model::Group) -> GroupItem {
    GroupItem {
        id: group.id.as_str().into(),
        name: group.name.as_str().into(),
        state: row_state(state.group_state(group)),
        available: count(state.available_members(group).count()),
        total: count(group.members.len()),
    }
}

pub fn notice_item(strings: &Strings<'_>, notice: &ui_model::Notice) -> NoticeItem {
    NoticeItem {
        id: i32::try_from(notice.id).unwrap_or(i32::MAX),
        message: notice_text(strings, &notice.message),
        time: notice.time.as_str().into(),
        has_guide: guide::guide(&notice.message).is_some(),
    }
}

/// Names as one list, "A, B and C": commas between all but the last two,
/// then `and` (the UI's words for "head and last").
fn join_names(names: &[String], and: impl FnOnce(&str, &str) -> String) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [head @ .., last] => and(&head.join(", "), last),
    }
}

/// Output names as one list in the UI's words: what every notice and the
/// guide name outputs with.
fn joined(strings: &Strings<'_>, names: &[String]) -> SharedString {
    join_names(names, |head, last| {
        strings
            .invoke_list_and(head.into(), last.into())
            .to_string()
    })
    .into()
}

/// The guide's first paragraph, in the UI's words.
fn guide_intro(strings: &Strings<'_>, guide: &Guide) -> SharedString {
    let outputs = joined(strings, &guide.outputs);
    match &guide.source {
        Some(source) => strings.invoke_guide_intro_app(source.as_str().into(), outputs),
        None => strings.invoke_guide_intro_system(outputs),
    }
}

/// A guide step in the UI's words.
fn guide_step_text(strings: &Strings<'_>, step: &GuideStep) -> SharedString {
    match step {
        GuideStep::RouteApp { app } => strings.invoke_guide_route_app(app.as_str().into()),
        GuideStep::ChangeDefault { current } => {
            strings.invoke_guide_change_default(current.as_str().into())
        }
        GuideStep::RouteOtherApps { outputs } => {
            strings.invoke_guide_route_other_apps(joined(strings, outputs))
        }
        GuideStep::MuteOutputs { outputs } => {
            strings.invoke_guide_mute_outputs(joined(strings, outputs), count(outputs.len()))
        }
        GuideStep::SharedOutputs { outputs } => {
            strings.invoke_guide_shared_outputs(joined(strings, outputs))
        }
        GuideStep::KeepAppUnmuted { app } => {
            strings.invoke_guide_keep_app_unmuted(app.as_str().into())
        }
    }
}

/// The UI's name of a step's Settings page.
fn guide_page(page: Option<SettingsPage>) -> GuidePage {
    match page {
        None => GuidePage::None,
        Some(SettingsPage::VolumeMixer) => GuidePage::VolumeMixer,
        Some(SettingsPage::Sound) => GuidePage::Sound,
    }
}

/// The Settings page a step's button asks for.
fn settings_page(page: GuidePage) -> Option<SettingsPage> {
    match page {
        GuidePage::None => None,
        GuidePage::VolumeMixer => Some(SettingsPage::VolumeMixer),
        GuidePage::Sound => Some(SettingsPage::Sound),
    }
}

/// A notice in the UI's words.
fn notice_text(strings: &Strings<'_>, message: &NoticeMessage) -> SharedString {
    match message {
        NoticeMessage::NoSpeaker => strings.get_notice_no_speaker(),
        NoticeMessage::SourceQuit { source } => {
            strings.invoke_notice_source_quit(source.as_str().into())
        }
        NoticeMessage::SourceNotRunning { source } => {
            strings.invoke_notice_source_not_running(source.as_str().into())
        }
        NoticeMessage::SourceRefused { source } => {
            strings.invoke_notice_source_refused(source.as_str().into())
        }
        NoticeMessage::CaptureFailed => strings.get_notice_capture_failed(),
        NoticeMessage::CaptureLost => strings.get_notice_capture_lost(),
        NoticeMessage::SpeakerFailed { speaker } => {
            strings.invoke_notice_speaker_failed(speaker.as_str().into())
        }
        NoticeMessage::SpeakerLost { speaker } => {
            strings.invoke_notice_speaker_lost(speaker.as_str().into())
        }
        NoticeMessage::AirPlayRetrying { speaker, problem }
        | NoticeMessage::AirPlayCantPlay { speaker, problem } => {
            strings.invoke_notice_airplay(airplay_problem(*problem), speaker.as_str().into())
        }
        NoticeMessage::HeardTwice {
            source,
            mute,
            shared,
            ..
        } => {
            let mute = joined(strings, mute);
            let shared = joined(strings, shared);
            match source {
                Some(source) => {
                    strings.invoke_notice_heard_twice_app(source.as_str().into(), mute, shared)
                }
                None => strings.invoke_notice_heard_twice_system(mute, shared),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::MockBackend;

    #[test]
    fn rows_show_the_speaker_state_kind_and_delay() {
        let mut backend = MockBackend::new();
        backend.set_running(true);
        let state = backend.snapshot();
        let desk = state.speaker("local:desk").unwrap();
        let row = speaker_item(&state, desk, true, true, None);
        assert_eq!(row.state, RowState::Off);
        assert_eq!(row.kind, SpeakerKind::Usb);
        assert!(row.has_delay && row.sync_open);
        assert_eq!((row.delay_ms, row.delay_text.as_str()), (120, "120"));

        let kitchen = state.speaker("airplay:kitchen").unwrap();
        let row = speaker_item(&state, kitchen, true, true, None);
        assert_eq!(row.state, RowState::Playing);
        assert!(
            !row.has_delay && !row.sync_open,
            "AirPlay rows have no sync controls"
        );
    }

    #[test]
    fn a_typed_delay_stays_in_the_field() {
        let state = MockBackend::new().snapshot();
        let desk = state.speaker("local:desk").unwrap();
        let row = speaker_item(&state, desk, true, true, Some("12"));
        assert_eq!(row.delay_text.as_str(), "12");
        assert_eq!(row.delay_ms, 120);
    }

    #[test]
    fn groups_count_their_present_members() {
        let state = MockBackend::new().snapshot();
        let row = group_item(&state, &state.groups[0]);
        assert_eq!((row.available, row.total), (2, 2));
        assert_eq!(row.state, RowState::Off);
    }

    #[test]
    fn the_flyout_lists_only_the_newest_notice() {
        let mut backend = MockBackend::new();
        backend.post("source", NoticeMessage::CaptureLost, "09.40");
        backend.post("start", NoticeMessage::NoSpeaker, "09.41");
        let notices = backend.snapshot().notices;
        let flyout = newest_notices(&notices, FLYOUT_NOTICE_LIMIT);
        let messages: Vec<_> = flyout.iter().map(|n| n.message.clone()).collect();
        // One row: the stack draws no Dismiss All row under it, and the
        // flyout's button opens the main window for the other notice.
        assert_eq!(messages, [NoticeMessage::NoSpeaker]);
        assert_eq!(newest_notices(&notices, MAIN_NOTICE_LIMIT).len(), 2);
    }

    #[test]
    fn output_names_read_as_a_list() {
        let and = |head: &str, last: &str| format!("{head} and {last}");
        let names =
            |list: &[&str]| -> Vec<String> { list.iter().map(|name| (*name).to_owned()).collect() };
        assert_eq!(join_names(&names(&[]), and), "");
        assert_eq!(join_names(&names(&["TV"]), and), "TV");
        assert_eq!(
            join_names(&names(&["TV", "Speakers"]), and),
            "TV and Speakers"
        );
        assert_eq!(
            join_names(&names(&["TV", "Speakers", "Monitor"]), and),
            "TV, Speakers and Monitor"
        );
        // Every notice and the guide join names through it.
        let view = include_str!("view.rs");
        let comma_joins = view.matches(concat!(".join(", "\", \"", ")")).count();
        assert_eq!(comma_joins, 1, "only join_names joins names by commas");
    }

    #[test]
    fn guide_buttons_ask_for_the_page_their_step_names() {
        for page in [
            None,
            Some(SettingsPage::VolumeMixer),
            Some(SettingsPage::Sound),
        ] {
            assert_eq!(settings_page(guide_page(page)), page);
        }
        let message = guide::preview("app").unwrap();
        let pages: Vec<GuidePage> = guide::guide(&message)
            .unwrap()
            .steps
            .iter()
            .map(|step| guide_page(step.page()))
            .collect();
        assert_eq!(
            pages,
            [GuidePage::VolumeMixer, GuidePage::Sound, GuidePage::None]
        );
    }

    #[test]
    fn models_update_in_place_when_the_length_is_unchanged() {
        let model = VecModel::from(vec![1, 2, 3]);
        sync_model(&model, vec![1, 5, 3]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5, 3]);
        sync_model(&model, vec![4]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [4]);
    }

    /// The flyout with two notices (it lists the newest), one group and
    /// three speakers.
    const FLYOUT: FlyoutCounts = FlyoutCounts {
        notices: 2,
        listed_notices: 1,
        groups: 1,
        speakers: 3,
    };

    #[test]
    fn a_refresh_removes_the_flyout_lists_whose_length_changes() {
        assert_eq!(FLYOUT.removed_by(FLYOUT), FlyoutParts::default());
        let unplugged = FlyoutCounts {
            speakers: 2,
            ..FLYOUT
        };
        assert_eq!(
            FLYOUT.removed_by(unplugged),
            FlyoutParts {
                speakers: true,
                ..FlyoutParts::default()
            }
        );
        let group_added = FlyoutCounts {
            groups: 2,
            ..FLYOUT
        };
        assert!(FLYOUT.removed_by(group_added).groups, "rows are rebuilt");
    }

    #[test]
    fn dismissing_notices_removes_the_more_button_then_the_last_notice() {
        // The newest notice is dismissed: the next one takes its row, and
        // the button for the rest goes.
        let one_left = FlyoutCounts {
            notices: 1,
            ..FLYOUT
        };
        assert_eq!(
            FLYOUT.removed_by(one_left),
            FlyoutParts {
                more_notices: true,
                ..FlyoutParts::default()
            }
        );
        let none_left = FlyoutCounts {
            notices: 0,
            listed_notices: 0,
            ..FLYOUT
        };
        assert_eq!(
            one_left.removed_by(none_left),
            FlyoutParts {
                notices: true,
                ..FlyoutParts::default()
            }
        );
        assert!(!none_left.removed_by(FLYOUT).more_notices, "it came back");
    }

    #[test]
    fn the_flyout_takes_the_focus_back_only_from_a_removed_part() {
        let notice = FlyoutParts {
            notices: true,
            ..FlyoutParts::default()
        };
        let speakers = FlyoutParts {
            speakers: true,
            ..FlyoutParts::default()
        };
        assert!(notice.overlaps(notice));
        assert!(
            !speakers.overlaps(FlyoutParts {
                notices: true,
                more_notices: true,
                groups: true,
                speakers: false,
            }),
            "a focused speaker row keeps its focus when other parts change"
        );
        assert!(!FlyoutParts::default().overlaps(notice), "nothing focused");
    }
}
