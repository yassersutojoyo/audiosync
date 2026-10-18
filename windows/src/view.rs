//! Connects the windows to a [`Backend`]: fills them from its snapshots and
//! turns every user action into a backend call, then refreshes.
//!
//! The models are updated in place, row by row, so a slider being dragged or a
//! field being typed in keeps its focus while the rest of the window updates.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use audiosync_core::eq::{self, EqSettings};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::ui_model::{self, AppState, Backend, SpeakerState};
use crate::{
    AboutDialog, Availability, EqualizerWindow, GroupItem, GroupNameDialog, MainWindow, NoticeItem,
    RowState, SpeakerItem, SpeakerKind, Strings, TrayFlyout,
};

/// What the group name dialog is open for.
#[derive(Clone, Debug, PartialEq)]
enum DialogPurpose {
    NewGroup,
    Rename(String),
    /// Only a message (no name field), e.g. why a group can't be made yet.
    Message,
}

/// UI state the backend doesn't keep: which sync controls are open and what
/// is being typed in their fields.
#[derive(Default)]
struct UiState {
    sync_open: HashSet<String>,
    delay_drafts: HashMap<String, String>,
}

struct Models {
    sources: Rc<VecModel<SharedString>>,
    this_pc: Rc<VecModel<SpeakerItem>>,
    connected: Rc<VecModel<SpeakerItem>>,
    all_speakers: Rc<VecModel<SpeakerItem>>,
    groups: Rc<VecModel<GroupItem>>,
    notices: Rc<VecModel<NoticeItem>>,
    gains: Rc<VecModel<f32>>,
    presets: Rc<VecModel<SharedString>>,
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
    ui: RefCell<UiState>,
    dialog_purpose: RefCell<DialogPurpose>,
    models: Models,
    /// With a tray icon, closing the main window keeps the app running there.
    tray_available: Cell<bool>,
    /// Called after every refresh, e.g. to update the tray icon and tooltip.
    on_refresh: RefCell<Option<RefreshHook>>,
}

/// Runs `action` on the app if it still exists.
fn with_app(app: &Weak<App>, action: impl FnOnce(&App)) {
    if let Some(app) = app.upgrade() {
        action(&app);
    }
}

impl App {
    pub fn new(backend: Box<dyn Backend>) -> Result<Rc<App>, slint::PlatformError> {
        let models = Models {
            sources: Rc::new(VecModel::default()),
            this_pc: Rc::new(VecModel::default()),
            connected: Rc::new(VecModel::default()),
            all_speakers: Rc::new(VecModel::default()),
            groups: Rc::new(VecModel::default()),
            notices: Rc::new(VecModel::default()),
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
            ui: RefCell::default(),
            dialog_purpose: RefCell::new(DialogPurpose::Message),
            models,
            tray_available: Cell::new(false),
            on_refresh: RefCell::new(None),
        });
        app.attach_models();
        app.connect_main();
        app.connect_flyout();
        app.connect_equalizer();
        app.connect_dialogs();
        app.refresh();
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

    /// Shows the main window and brings it to the front.
    pub fn show_main(&self) {
        if let Err(error) = self.main.show() {
            self.show_window_error(&error);
            return;
        }
        crate::platform::bring_to_front(self.main.window());
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
        self.flyout.set_notices(ModelRc::from(m.notices.clone()));
        self.equalizer.set_gains(ModelRc::from(m.gains.clone()));
        self.equalizer
            .set_preset_names(ModelRc::from(m.presets.clone()));
        let band_hz: Vec<f32> = eq::BAND_CENTERS_HZ.iter().map(|&hz| hz as f32).collect();
        self.equalizer
            .set_band_hz(ModelRc::new(VecModel::from(band_hz)));
        self.equalizer.set_max_gain(eq::MAX_GAIN_DB as f32);
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
        let this_pc: Vec<SpeakerItem> = this_pc.into_iter().map(|s| item(s, false)).collect();
        let connected: Vec<SpeakerItem> = connected.into_iter().map(|s| item(s, true)).collect();
        let all: Vec<SpeakerItem> = this_pc.iter().chain(&connected).cloned().collect();
        let groups: Vec<GroupItem> = state.groups.iter().map(|g| group_item(&state, g)).collect();
        let notices: Vec<NoticeItem> = state.notices.iter().map(notice_item).collect();
        let sources: Vec<SharedString> = state
            .sources
            .iter()
            .map(|s| s.name.as_str().into())
            .collect();
        let source_index = state
            .sources
            .iter()
            .position(|s| s.id == state.source_id)
            .map_or(-1, |i| i as i32);
        let has_airplay = state
            .speakers
            .iter()
            .any(|s| s.kind == ui_model::SpeakerKind::AirPlay);
        let status = strings.invoke_status(state.running, count(state.active_speakers()));

        let m = &self.models;
        sync_model(&m.sources, sources);
        sync_model(&m.this_pc, this_pc);
        sync_model(&m.connected, connected);
        sync_model(&m.all_speakers, all);
        sync_model(&m.groups, groups);
        sync_model(&m.notices, notices);

        for window in [&self.main as &dyn WindowState, &self.flyout] {
            window.show_state(&state, &status, source_index);
        }
        self.main.set_equalizer_on(state.equalizer.enabled);
        self.main.set_connected_note(if has_airplay {
            SharedString::new()
        } else {
            strings.get_air_play_empty()
        });

        self.show_equalizer_state(&state.equalizer);
        drop(ui);
        if let Some(callback) = self.on_refresh.borrow().as_ref() {
            callback(&state);
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
        main.on_notice_dismissed(move |id| {
            with_app(&w, |app| app.update(|b| b.dismiss_notice(id as u64)))
        });
        let w = weak.clone();
        main.on_notices_dismissed_all(move || {
            with_app(&w, |app| app.update(|b| b.dismiss_all_notices()))
        });
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
        ui_model::Availability::PasswordUnsupported => Availability::PasswordUnsupported,
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

pub fn notice_item(notice: &ui_model::Notice) -> NoticeItem {
    NoticeItem {
        id: i32::try_from(notice.id).unwrap_or(i32::MAX),
        message: notice.message.as_str().into(),
        time: notice.time.as_str().into(),
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
    fn models_update_in_place_when_the_length_is_unchanged() {
        let model = VecModel::from(vec![1, 2, 3]);
        sync_model(&model, vec![1, 5, 3]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5, 3]);
        sync_model(&model, vec![4]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [4]);
    }
}
