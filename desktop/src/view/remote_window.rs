//! File > Web Remote (`ui/remote.slint`) and the server behind it.
//!
//! The setting is kept with the windows' own settings and starts the server
//! at launch when it was on. Every refresh publishes the page's snapshot
//! (`crate::remote::snapshot`); the server hands each command from the page
//! to a channel and wakes the event loop, where it is applied like the
//! window's own controls (`crate::remote::actions`) and the result shown and
//! published again. The poll timer also takes commands, in case a wake-up
//! was missed, and writes what the server reports to the debug log, which
//! never sees a pairing code or a device token.
//!
//! While the server listens, a thread reads the networks' profiles, and the
//! window and a notice say so when one is Public.

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use audiosync_remote::auth::{DeviceFile, TokenStore};
use audiosync_remote::{Command, Config, RemoteError, RemoteServer};
use slint::{ComponentHandle, SharedString};

use super::{
    App, REMOTE_PUBLIC_NOTICE_ID, availability, cast_group_note, cast_state, row_state,
    speaker_kind, with_app,
};
use crate::Strings;
use crate::network_profile::{ProfileSource, PublicNetworkWarning, Watch};
use crate::remote::{self, Action, RemoteSettings, RowText};
use crate::ui_model::{AppState, Notice, NoticeMessage, Speaker};

thread_local! {
    /// The app a command wakes on the event loop's thread.
    static REMOTE_APP: RefCell<Weak<App>> = const { RefCell::new(Weak::new()) };
}

/// Pixels a QR module is drawn with before the window scales it.
const QR_SCALE: usize = 4;

/// How often the networks' profiles are read while the remote listens: a
/// move to a cafe's Wi-Fi shows within this. The tests read more often.
const PROFILE_CHECK: Duration = Duration::from_millis(if cfg!(test) { 10 } else { 5000 });

/// Why the remote is on but not listening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Problem {
    /// The port can't be listened on: another app has it (`in_use`), or
    /// Windows refuses it, as it does for the ports Hyper-V and WSL reserve.
    Port { in_use: bool },
    /// No randomness for a code, or a malformed page: not the user's to fix.
    Failed,
}

fn problem_for(error: &RemoteError) -> Problem {
    match error {
        RemoteError::Bind(_) => Problem::Port {
            in_use: error.port_in_use(),
        },
        RemoteError::Random | RemoteError::Page(_) => Problem::Failed,
    }
}

/// The running server and what the window shows about it.
pub(super) struct RemoteHost {
    server: Option<RemoteServer>,
    commands: Option<mpsc::Receiver<Command>>,
    problem: Option<Problem>,
    /// The port typed in the window wasn't one the setting takes.
    port_invalid: bool,
    /// Windows' DNS host name, as the page's footer shows it.
    computer_name: String,
    /// The same name as a `.local` host, when it is one.
    host: Option<String>,
    lan_address: Option<Ipv4Addr>,
    /// The addresses the server listens on; the tests keep to loopback.
    pub(super) listen: Vec<IpAddr>,
    /// The paired-device file; `None` keeps the devices in memory.
    pub(super) devices_path: Option<PathBuf>,
    /// The QR code drawn, with the address it holds.
    qr: Option<(String, slint::Image)>,
    /// The devices in the file while the server is off, read when needed.
    devices_offline: Option<usize>,
    /// Forget All Remote Devices couldn't save the empty list.
    forget_failed: bool,
    /// The networks' profiles; `None` reads none (the tests that don't fake one).
    pub(super) profiles: Option<Arc<dyn ProfileSource>>,
    watch: Option<Watch>,
    public: PublicNetworkWarning,
    /// When the Public network notice last appeared.
    public_since: String,
    /// The last reading failed and the log says so; repeats aren't logged.
    profiles_unread: bool,
    /// The port the debug switch started this run's remote on; kept out of
    /// the settings, and dropped once the user changes the setting.
    debug_port: Option<u16>,
}

impl Default for RemoteHost {
    fn default() -> RemoteHost {
        let computer_name = remote::computer_name().unwrap_or_default();
        RemoteHost {
            server: None,
            commands: None,
            problem: None,
            port_invalid: false,
            host: remote::local_host(&computer_name),
            computer_name,
            lan_address: None,
            #[cfg(not(test))]
            listen: Config::default().listen,
            // The wildcard default could make Windows Firewall ask about the test program.
            #[cfg(test)]
            listen: vec![Ipv4Addr::LOCALHOST.into()],
            devices_path: devices_path(),
            qr: None,
            devices_offline: None,
            forget_failed: false,
            profiles: profile_source(),
            watch: None,
            public: PublicNetworkWarning::default(),
            public_since: String::new(),
            debug_port: None,
            profiles_unread: false,
        }
    }
}

#[cfg(all(windows, not(test)))]
fn profile_source() -> Option<Arc<dyn ProfileSource>> {
    Some(Arc::new(crate::network_profile::NetworkList))
}

/// No watch and no warning: Linux has no Public network category, and
/// firewalld's default zone is named `public`, so mapping zones would warn
/// on most PCs.
#[cfg(any(not(windows), test))]
fn profile_source() -> Option<Arc<dyn ProfileSource>> {
    None
}

#[cfg(all(os_backend, not(test)))]
fn devices_path() -> Option<PathBuf> {
    crate::data_folder::folder().map(|folder| folder.join("remote-devices.json"))
}

#[cfg(any(not(os_backend), test))]
fn devices_path() -> Option<PathBuf> {
    None
}

/// The row texts the window shows, from its strings.
struct StringsText<'a>(Strings<'a>);

impl RowText for StringsText<'_> {
    fn turn_on_at_pc(&self) -> String {
        self.0.get_remote_turn_on_at_pc().into()
    }

    fn source_system(&self) -> String {
        self.0.get_source_system().into()
    }

    fn source_all_except(&self, names: &str) -> String {
        self.0.invoke_source_all_except(names.into()).into()
    }

    fn kind(&self, speaker: &Speaker) -> String {
        self.0
            .invoke_speaker_kind(speaker_kind(speaker.kind))
            .into()
    }

    fn note(&self, state: &AppState, speaker: &Speaker) -> String {
        if let Some(cast) = &speaker.cast {
            let group_note = cast_group_note(&self.0, speaker);
            let note = self
                .0
                .invoke_cast_note(cast_state(cast.state), cast.muted, group_note);
            if !note.is_empty() {
                return note.into();
            }
        }
        let note = self
            .0
            .invoke_availability(availability(speaker.availability));
        if !note.is_empty() {
            return note.into();
        }
        self.0
            .invoke_row_note(row_state(state.speaker_state(speaker)))
            .into()
    }
}

fn wake_on_event_loop() {
    // Fails only once the event loop is gone (or in the headless tests); the poll takes the command then.
    let _ = slint::invoke_from_event_loop(|| {
        let app = REMOTE_APP.with(|app| app.borrow().upgrade());
        if let Some(app) = app {
            app.take_remote_commands();
        }
    });
}

impl App {
    pub(super) fn connect_remote(self: &Rc<Self>) {
        REMOTE_APP.with(|app| *app.borrow_mut() = Rc::downgrade(self));
        self.follow_frame(|app| app.remote_window.window(), "Web Remote");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.main
            .on_remote_requested(move || with_app(&w, App::show_remote));
        let w = weak.clone();
        self.remote_window
            .on_remote_toggled(move |on| with_app(&w, |app| app.set_remote_on(on)));
        let w = weak.clone();
        self.remote_window
            .on_new_code_requested(move || with_app(&w, App::new_remote_code));
        let w = weak.clone();
        self.remote_window
            .on_forget_all_requested(move || with_app(&w, App::forget_remote_devices));
        self.remote_window
            .on_use_port_enabled(|text, port, failed| {
                u16::try_from(port).is_ok_and(|port| remote::use_port_enabled(&text, port, failed))
            });
        let w = weak.clone();
        self.remote_window.on_port_submitted(move |text| {
            with_app(&w, |app| app.use_remote_port(&text));
        });
        let w = weak.clone();
        self.remote_window.on_close_requested(move || {
            with_app(&w, |app| {
                // A window that fails to hide stays open; the user can still close it.
                let _ = app.remote_window.hide();
            })
        });
        self.remote_window.on_exit_requested(|| {
            let _ = slint::quit_event_loop();
        });
        let w = weak;
        self.remote_window.window().on_close_requested(move || {
            with_app(&w, |app| {
                let _ = app.remote_window.hide();
            });
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// Starts the remote at launch when it was left on or the debug switch
    /// asks for it.
    pub(super) fn restore_remote(&self) {
        let debug_port = remote::debug_switch_port(
            self.log.enabled(),
            std::env::var_os(remote::DEBUG_SWITCH_VAR).as_deref(),
            std::env::var_os(remote::DEBUG_PORT_VAR).as_deref(),
            self.ui.borrow().remote,
        );
        self.restore_remote_with(debug_port);
    }

    pub(super) fn restore_remote_with(&self, debug_port: Option<u16>) {
        if debug_port.is_some() {
            self.remote.borrow_mut().debug_port = debug_port;
            self.log
                .log(|| "remote: on for this run (debug switch)".to_owned());
        }
        if self.ui.borrow().remote.on || debug_port.is_some() {
            self.start_remote();
        }
        self.show_remote_state();
    }

    /// Opens the window, or brings it to the front; opening it makes a new
    /// pairing code and ends a lockout.
    pub fn show_remote(&self) {
        let opening = !self.remote_window.window().is_visible();
        if opening {
            self.rotate_remote_code();
            self.remote.borrow_mut().devices_offline = None;
            self.log.log(|| "web remote: window opened".to_owned());
        }
        self.show_remote_state();
        if let Err(error) = self.remote_window.show() {
            self.show_window_error(&error);
            return;
        }
        self.remote_window.invoke_take_focus();
        crate::platform::bring_to_front(self.remote_window.window());
    }

    fn set_remote_on(&self, on: bool) {
        if self.ui.borrow().remote.on == on {
            return;
        }
        self.ui.borrow_mut().remote.on = on;
        self.remote.borrow_mut().debug_port = None;
        self.remember_view();
        if on {
            self.start_remote();
        } else {
            self.stop_remote();
            self.remote.borrow_mut().problem = None;
        }
        self.show_remote_state();
        self.refresh();
    }

    pub(super) fn remote_listening(&self) -> bool {
        self.remote.borrow().server.is_some()
    }

    fn start_remote(&self) {
        self.stop_remote();
        let debug_port = self.remote.borrow().debug_port;
        let port = debug_port.unwrap_or(self.ui.borrow().remote.port);
        let config = {
            let host = self.remote.borrow();
            Config {
                port,
                listen: host.listen.clone(),
                local_name: host.host.clone(),
                devices_path: host.devices_path.clone(),
                log_pairing_codes: self.log.enabled(),
                ..Config::default()
            }
        };
        let (sender, receiver) = mpsc::channel();
        let started = RemoteServer::start(config, move |command: &Command| {
            if sender.send(command.clone()).is_ok() {
                wake_on_event_loop();
            }
        });
        let mut host = self.remote.borrow_mut();
        host.port_invalid = false;
        host.forget_failed = false;
        match started {
            Ok(server) => {
                self.log
                    .log(|| format!("web remote: listening on port {}", server.port()));
                host.server = Some(server);
                host.commands = Some(receiver);
                host.problem = None;
                host.watch = host
                    .profiles
                    .clone()
                    .map(|source| Watch::start(source, PROFILE_CHECK));
            }
            Err(error) => {
                self.log
                    .log(|| format!("web remote: didn't start on port {port} ({error})"));
                host.problem = Some(problem_for(&error));
            }
        }
        drop(host);
        self.publish_remote(&self.snapshot());
    }

    fn stop_remote(&self) {
        let server = {
            let mut host = self.remote.borrow_mut();
            host.commands = None;
            host.watch = None;
            host.public = PublicNetworkWarning::default();
            host.profiles_unread = false;
            host.server.take()
        };
        if let Some(server) = server {
            self.log_remote_events(&server);
            server.stop();
            self.log.log(|| "web remote: stopped".to_owned());
        }
        self.remote.borrow_mut().devices_offline = None;
    }

    fn log_remote_events(&self, server: &RemoteServer) {
        while let Some(event) = server.poll_event() {
            self.log.log(|| event.to_string());
        }
    }

    fn rotate_remote_code(&self) {
        let host = self.remote.borrow();
        if let Some(server) = host.server.as_ref()
            && let Err(error) = server.rotate_code()
        {
            self.log
                .log(|| format!("web remote: no new pairing code ({error})"));
        }
    }

    fn new_remote_code(&self) {
        self.rotate_remote_code();
        self.log.log(|| "web remote: new pairing code".to_owned());
        self.show_remote_state();
    }

    /// Forgets every paired device, through the server while it runs, else
    /// in the file it reads at its next start.
    fn forget_remote_devices(&self) {
        let saved = {
            let host = self.remote.borrow();
            match host.server.as_ref() {
                Some(server) => server.forget_all(),
                None => {
                    let (mut store, _) = TokenStore::open(host.devices_path.clone());
                    DeviceFile::default().save(store.clear())
                }
            }
        };
        match &saved {
            Ok(()) => self
                .log
                .log(|| "web remote: every device forgotten".to_owned()),
            Err(error) => self
                .log
                .log(|| format!("web remote: the forgotten devices weren't saved ({error})")),
        }
        {
            let mut host = self.remote.borrow_mut();
            host.forget_failed = saved.is_err();
            host.devices_offline = None;
        }
        self.show_remote_state();
    }

    fn use_remote_port(&self, text: &str) {
        let Some(port) = remote::parse_port(text) else {
            self.remote.borrow_mut().port_invalid = true;
            self.show_remote_state();
            return;
        };
        self.ui.borrow_mut().remote = RemoteSettings { on: true, port };
        self.remote.borrow_mut().debug_port = None;
        self.remember_view();
        self.start_remote();
        self.show_remote_state();
    }

    /// Applies the commands the page sent since the last call, then shows
    /// and publishes the result.
    pub(super) fn take_remote_commands(&self) {
        let commands: Vec<Command> = match self.remote.borrow().commands.as_ref() {
            Some(receiver) => receiver.try_iter().collect(),
            None => return,
        };
        if commands.is_empty() {
            return;
        }
        for command in &commands {
            let state = self.snapshot();
            let actions = {
                let ui = self.ui.borrow();
                remote::actions(command, &state, |speaker| {
                    ui.listed_virtual.lists(speaker, ui.show_virtual)
                })
            };
            match actions {
                Ok(actions) => {
                    let mut backend = self.backend.borrow_mut();
                    for action in actions {
                        match action {
                            Action::Enable { id, on } => backend.set_speaker_enabled(&id, on),
                            Action::Volume { id, volume } => {
                                backend.set_speaker_volume(&id, volume);
                            }
                            Action::Delay { id, ms } => backend.set_speaker_delay(&id, ms),
                            Action::AirPlayDelay { ms } => backend.set_airplay_delay(ms),
                            Action::Master { volume } => backend.set_master_volume(volume),
                            Action::ToggleZone { id, keep_off } => {
                                backend.toggle_zone_from_phone(&id, &keep_off);
                            }
                            Action::ZoneVolume { id, volume } => {
                                backend.set_zone_volume(&id, volume);
                            }
                            Action::RecallPreset { id, keep_off } => {
                                backend.recall_preset_from_phone(&id, &keep_off);
                            }
                            Action::Source { id } => backend.select_source(&id),
                        }
                    }
                }
                Err(refusal) => self
                    .log
                    .log(|| format!("web remote: a change was refused ({refusal:?})")),
            }
        }
        self.refresh();
    }

    /// Hands the page the snapshot of `state`, while the remote listens.
    pub(super) fn publish_remote(&self, state: &AppState) {
        let host = self.remote.borrow();
        let Some(server) = host.server.as_ref() else {
            return;
        };
        let snapshot = {
            let ui = self.ui.borrow();
            let card = self.now_playing.borrow();
            remote::snapshot(
                state,
                |speaker| ui.listed_virtual.lists(speaker, ui.show_virtual),
                card.track(),
                &host.computer_name,
                &StringsText(self.strings()),
            )
        };
        server.publish(snapshot);
    }

    /// What the poll timer does for the remote: takes commands, logs what
    /// the server reports, and keeps the open window current (a code expires,
    /// a phone pairs).
    pub(super) fn poll_remote(&self) {
        self.take_remote_commands();
        if let Some(server) = self.remote.borrow().server.as_ref() {
            self.log_remote_events(server);
        }
        self.read_network_profiles();
        if self.remote_window.window().is_visible() {
            self.show_remote_state();
        }
    }

    /// Takes the newest profile reading, and shows what it changed.
    fn read_network_profiles(&self) {
        let public = {
            let mut host = self.remote.borrow_mut();
            let Some(reading) = host.watch.as_ref().and_then(Watch::latest) else {
                return;
            };
            if reading.is_none() {
                if !host.profiles_unread {
                    host.profiles_unread = true;
                    self.log.log(|| {
                        "web remote: Windows didn't say which networks are Public".to_owned()
                    });
                }
                return;
            }
            host.profiles_unread = false;
            let was = host.public;
            if !host.public.read(reading.as_deref()) {
                return;
            }
            if host.public.notice() && !was.notice() {
                host.public_since = crate::platform::notice_time();
            }
            (host.public.public() != was.public()).then_some(host.public.public())
        };
        if let Some(public) = public {
            self.log.log(|| {
                format!(
                    "web remote: {} connected network is Public",
                    if public { "a" } else { "no" }
                )
            });
        }
        self.refresh();
    }

    /// The notice while the remote listens on a Public network.
    pub(super) fn remote_public_notice(&self) -> Option<Notice> {
        let host = self.remote.borrow();
        (host.server.is_some() && host.public.notice()).then(|| Notice {
            id: REMOTE_PUBLIC_NOTICE_ID,
            subject: "web remote on a public network".to_owned(),
            message: NoticeMessage::RemoteOnPublicNetwork,
            time: host.public_since.clone(),
        })
    }

    /// Dismisses the Public network notice if `id` is its id; returns
    /// whether it was showing.
    pub(super) fn dismiss_remote_notice(&self, id: u64) -> bool {
        id == REMOTE_PUBLIC_NOTICE_ID && self.remote.borrow_mut().public.dismiss()
    }

    /// Fills the window from the setting and the server.
    pub(super) fn show_remote_state(&self) {
        let settings = self.ui.borrow().remote;
        let window = &self.remote_window;
        let mut host = self.remote.borrow_mut();
        window.set_remote_on(settings.on);
        window.set_port(i32::from(settings.port));
        window.set_port_problem(matches!(host.problem, Some(Problem::Port { .. })));
        window.set_port_in_use(host.problem == Some(Problem::Port { in_use: true }));
        window.set_port_invalid(host.port_invalid);
        window.set_problem(match host.problem {
            Some(Problem::Failed) => self.strings().get_remote_failed(),
            _ => SharedString::default(),
        });
        window.set_forget_failed(host.forget_failed);
        let devices = match host.server.as_ref() {
            Some(server) => server.device_count(),
            None => {
                let path = host.devices_path.clone();
                *host
                    .devices_offline
                    .get_or_insert_with(|| TokenStore::open(path).0.len())
            }
        };
        window.set_devices(i32::try_from(devices).unwrap_or(i32::MAX));
        window.set_public_network(host.server.is_some() && host.public.public());
        let Some(server) = host.server.as_ref() else {
            window.set_listening(false);
            return;
        };
        let port = server.port();
        let code = server.pairing_code().unwrap_or_default();
        window.set_code(code.as_str().into());
        window.set_code_spoken(remote::spaced_digits(&code).into());
        window.set_locked(server.pairing_locked());
        let local = host
            .host
            .as_deref()
            .map(|name| remote::local_url(name, port));
        window.set_local_address(local.clone().unwrap_or_default().into());
        // The PC may have moved to another network since the remote started.
        host.lan_address = remote::lan_address();
        window.set_ip_address(
            host.lan_address
                .map(|address| remote::ip_url(address, port))
                .unwrap_or_default()
                .into(),
        );
        let drawn = host.qr.as_ref().map(|(text, _)| text.clone());
        if local != drawn {
            host.qr = local.and_then(|address| {
                let (side, pixels) = remote::qr_pixels(&address, QR_SCALE)?;
                let side = u32::try_from(side).ok()?;
                let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side, side);
                for (pixel, [r, g, b]) in buffer.make_mut_slice().iter_mut().zip(pixels) {
                    *pixel = slint::Rgb8Pixel { r, g, b };
                }
                Some((address, slint::Image::from_rgb8(buffer)))
            });
        }
        window.set_has_qr(host.qr.is_some());
        window.set_qr(
            host.qr
                .as_ref()
                .map(|(_, image)| image.clone())
                .unwrap_or_default(),
        );
        window.set_listening(true);
    }

    /// Stops the server as the app quits, so no command arrives after the backend.
    pub fn stop_remote_at_exit(&self) {
        self.stop_remote();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    use slint::ComponentHandle;

    use slint::platform::Key;

    use std::sync::Arc;

    use super::super::tests::{app_over, draw_frames, press};
    use crate::network_profile::{Category, ProfileSource};
    use crate::remote::RemoteSettings;
    use crate::ui_model::{MockBackend, NoticeMessage};

    fn free_port() -> u16 {
        // The port is free once this listener is dropped (and stays so in a test, likely enough).
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a port");
        listener.local_addr().expect("its address").port()
    }

    fn over_loopback(backend: MockBackend) -> std::rc::Rc<super::App> {
        let app = app_over(backend);
        app.remote.borrow_mut().listen = vec![Ipv4Addr::LOCALHOST.into()];
        app
    }

    fn get_state(port: u16) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connected");
        write!(
            stream,
            "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .expect("sent");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("read");
        answer
    }

    #[test]
    fn escape_closes_the_window_from_the_switch() {
        let app = over_loopback(MockBackend::new());
        let window = app.remote_window.window();
        app.main.invoke_remote_requested();
        draw_frames();
        assert!(window.is_visible());
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape closes it");
        app.main.invoke_remote_requested();
        assert!(window.is_visible(), "it opens again");
    }

    #[test]
    fn file_web_remote_opens_the_window_and_the_switch_is_kept() {
        let app = over_loopback(MockBackend::new());
        app.main.invoke_remote_requested();
        assert!(app.remote_window.window().is_visible());
        assert!(!app.remote_window.get_remote_on(), "off by default");
        assert!(!app.remote_listening());

        let port = free_port();
        app.ui.borrow_mut().remote.port = port;
        app.remote_window.invoke_remote_toggled(true);
        assert!(app.remote_listening(), "on, it listens");
        assert!(app.remote_window.get_listening());
        assert_eq!(app.remote_window.get_code().len(), 6);
        assert_eq!(app.remote_window.get_devices(), 0);
        assert_eq!(
            app.backend.borrow().view_settings().remote,
            RemoteSettings { on: true, port },
            "kept with the windows' settings"
        );
        // Not paired, so the API turns the request down: the server answers.
        assert!(get_state(port).starts_with("HTTP/1.1 401"));

        app.remote_window.invoke_remote_toggled(false);
        assert!(!app.remote_listening());
        assert!(!app.remote_window.get_listening());
        assert!(!app.backend.borrow().view_settings().remote.on);
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
            "closed"
        );
    }

    // covers: S212
    #[test]
    fn a_remote_setting_read_after_launch_starts_or_stops_the_remote() {
        let backend = MockBackend::new();
        let late_view = backend.late_view.clone();
        let app = over_loopback(backend);
        let port = free_port();
        *late_view.borrow_mut() = Some(crate::ui_model::ViewSettings {
            remote: RemoteSettings { on: true, port },
            ..crate::ui_model::ViewSettings::default()
        });
        app.poll();
        assert!(app.remote_listening(), "on in the file, it listens");
        *late_view.borrow_mut() = Some(crate::ui_model::ViewSettings::default());
        app.poll();
        assert!(!app.remote_listening(), "off in the file, it stops");
    }

    #[test]
    fn opening_the_window_again_makes_a_new_code() {
        let app = over_loopback(MockBackend::new());
        app.ui.borrow_mut().remote.port = free_port();
        app.main.invoke_remote_requested();
        app.remote_window.invoke_remote_toggled(true);
        let first = app.remote_window.get_code();
        app.remote_window.invoke_close_requested();
        assert!(!app.remote_window.window().is_visible());
        assert!(
            app.remote_listening(),
            "closing the window keeps the remote on"
        );
        app.main.invoke_remote_requested();
        let second = app.remote_window.get_code();
        assert_eq!(second.len(), 6);
        assert_ne!(first, second, "a new code (one in a million to fail)");
        app.remote_window.invoke_new_code_requested();
        assert_ne!(app.remote_window.get_code(), second);
    }

    #[test]
    fn a_port_in_use_is_said_and_another_port_can_be_picked() {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a port");
        let busy = taken.local_addr().expect("its address").port();
        let app = over_loopback(MockBackend::new());
        app.ui.borrow_mut().remote.port = busy;
        app.main.invoke_remote_requested();
        app.remote_window.invoke_remote_toggled(true);
        assert!(app.remote_window.get_remote_on(), "the setting stays on");
        assert!(app.remote_window.get_port_problem());
        assert!(app.remote_window.get_port_in_use());
        assert_eq!(app.remote_window.get_port(), i32::from(busy));
        assert!(!app.remote_window.get_listening());

        app.remote_window.invoke_port_submitted("80".into());
        assert!(app.remote_window.get_port_invalid());
        assert!(app.remote_window.get_port_in_use(), "nothing changed");
        let use_port = |text: &str| {
            app.remote_window.invoke_use_port_enabled(
                text.into(),
                app.remote_window.get_port(),
                app.remote_window.get_port_problem(),
            )
        };
        assert!(!use_port("80"));
        assert!(use_port(&busy.to_string()), "the same port again retries");

        let free = free_port();
        assert!(use_port(&free.to_string()));
        app.remote_window
            .invoke_port_submitted(free.to_string().into());
        assert!(!app.remote_window.get_port_in_use());
        assert!(!app.remote_window.get_port_invalid());
        assert!(app.remote_window.get_listening());
        assert_eq!(app.backend.borrow().view_settings().remote.port, free);
    }

    #[test]
    fn any_bind_error_offers_another_port_and_only_a_taken_one_says_another_app() {
        use std::io::{Error, ErrorKind};

        use audiosync_remote::RemoteError;

        use super::{Problem, problem_for};
        let bind = |kind| RemoteError::Bind(Error::from(kind));
        // A port Hyper-V or WSL reserves fails with WSAEACCES.
        assert_eq!(
            problem_for(&bind(ErrorKind::PermissionDenied)),
            Problem::Port { in_use: false }
        );
        assert_eq!(
            problem_for(&bind(ErrorKind::AddrInUse)),
            Problem::Port { in_use: true }
        );
        assert_eq!(problem_for(&RemoteError::Random), Problem::Failed);
    }

    #[test]
    fn a_port_windows_refuses_shows_the_port_field_without_the_raw_error() {
        let app = over_loopback(MockBackend::new());
        // Not this machine's address: binding fails, and not because the port is taken.
        app.remote.borrow_mut().listen = vec![Ipv4Addr::new(192, 0, 2, 1).into()];
        app.main.invoke_remote_requested();
        app.remote_window.invoke_remote_toggled(true);
        assert!(app.remote_window.get_port_problem());
        assert!(!app.remote_window.get_port_in_use());
        assert_eq!(app.remote_window.get_problem(), "", "no raw OS error");
        assert!(!app.remote_window.get_listening());

        app.remote.borrow_mut().listen = vec![Ipv4Addr::LOCALHOST.into()];
        let free = free_port();
        app.remote_window
            .invoke_port_submitted(free.to_string().into());
        assert!(!app.remote_window.get_port_problem());
        assert!(app.remote_window.get_listening());
    }

    #[test]
    fn forget_all_works_while_the_remote_is_off_and_says_when_it_wasnt_saved() {
        let dir = crate::debug_log::test_dir("remote-forget");
        let path = dir.join("remote-devices.json");
        std::fs::write(
            &path,
            format!(r#"{{"version": 1, "devices": ["{}"]}}"#, "ab".repeat(32)),
        )
        .expect("written");
        let app = over_loopback(MockBackend::new());
        app.remote.borrow_mut().devices_path = Some(path.clone());
        app.main.invoke_remote_requested();
        assert!(!app.remote_window.get_remote_on());
        assert_eq!(app.remote_window.get_devices(), 1, "read from the file");
        app.remote_window.invoke_forget_all_requested();
        assert_eq!(app.remote_window.get_devices(), 0);
        assert!(!app.remote_window.get_forget_failed());
        let kept = std::fs::read_to_string(&path).expect("read");
        assert!(!kept.contains(&"ab".repeat(32)), "the file forgets it too");

        // A file where a folder should be: the list can't be saved.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "").expect("written");
        app.remote.borrow_mut().devices_path = Some(blocker.join("remote-devices.json"));
        app.remote_window.invoke_forget_all_requested();
        assert!(app.remote_window.get_forget_failed());
    }

    fn current_code(app: &super::App) -> String {
        let host = app.remote.borrow();
        host.server
            .as_ref()
            .and_then(audiosync_remote::RemoteServer::pairing_code)
            .expect("a code in force")
    }

    // covers: S288
    #[test]
    fn the_debug_switch_starts_the_remote_for_this_run_and_saves_nothing() {
        use crate::debug_log::{self, DebugLog};

        let path = debug_log::test_dir("remote-switch").join(debug_log::FILE_NAME);
        let log = DebugLog::new(Some(path.clone()));
        let app = super::super::tests::app_logging(MockBackend::new(), log.clone());
        app.remote.borrow_mut().listen = vec![Ipv4Addr::LOCALHOST.into()];
        let before = app.backend.borrow().view_settings();
        assert!(!before.remote.on);

        app.restore_remote_with(None);
        assert!(!app.remote_listening(), "no switch, no remote");

        let port = free_port();
        app.restore_remote_with(Some(port));
        assert!(app.remote_listening());
        assert_eq!(
            app.remote.borrow().server.as_ref().map(|s| s.port()),
            Some(port)
        );
        app.remember_view();
        assert_eq!(
            app.backend.borrow().view_settings(),
            before,
            "the saved setting stays off with its port"
        );
        assert!(log.flush(debug_log::TEST_FLUSH));
        debug_log::assert_in_order(
            &debug_log::read_messages(&path),
            &[
                "remote: on for this run (debug switch)",
                &format!("web remote: listening on port {port}"),
            ],
        );
    }

    #[test]
    fn the_debug_log_gets_each_pairing_code_and_a_normal_run_none() {
        use crate::debug_log::{self, DebugLog};

        let path = debug_log::test_dir("remote-codes").join(debug_log::FILE_NAME);
        let log = DebugLog::new(Some(path.clone()));
        let app = super::super::tests::app_logging(MockBackend::new(), log.clone());
        app.remote.borrow_mut().listen = vec![Ipv4Addr::LOCALHOST.into()];
        app.ui.borrow_mut().remote.port = free_port();
        app.remote_window.invoke_remote_toggled(true);
        let first = current_code(&app);
        app.remote_window.invoke_new_code_requested();
        let second = current_code(&app);
        app.poll_remote();
        app.remote_window.invoke_remote_toggled(false);
        assert!(log.flush(debug_log::TEST_FLUSH));
        let messages = debug_log::read_messages(&path);
        debug_log::assert_in_order(
            &messages,
            &[
                &format!("remote: pairing code {first} (debug only)"),
                &format!("remote: pairing code {second} (debug only)"),
            ],
        );

        let quiet = over_loopback(MockBackend::new());
        quiet.ui.borrow_mut().remote.port = free_port();
        quiet.remote_window.invoke_remote_toggled(true);
        quiet.remote_window.invoke_new_code_requested();
        let host = quiet.remote.borrow();
        let server = host.server.as_ref().expect("listening");
        let events: Vec<String> = std::iter::from_fn(|| server.poll_event())
            .map(|event| event.to_string())
            .collect();
        assert!(
            events.iter().all(|line| !line.contains("pairing code")),
            "{events:?}"
        );
    }

    #[test]
    fn a_command_from_the_page_works_like_the_windows_controls() {
        let app = over_loopback(MockBackend::new());
        app.ui.borrow_mut().remote.port = free_port();
        app.remote_window.invoke_remote_toggled(true);
        let state = app.snapshot();
        let speaker = state
            .speakers
            .iter()
            .find(|speaker| {
                !speaker.enabled
                    && speaker.availability == crate::ui_model::Availability::Open
                    && speaker.kind != crate::ui_model::SpeakerKind::Virtual
            })
            .expect("a speaker that is off")
            .id
            .clone();
        let sender = {
            // The channel the server's callback feeds.
            let (sender, receiver) = std::sync::mpsc::channel();
            app.remote.borrow_mut().commands = Some(receiver);
            sender
        };
        sender
            .send(audiosync_remote::Command::Speaker {
                id: speaker.clone(),
                change: audiosync_remote::SpeakerChange {
                    on: Some(true),
                    volume: Some(20.0),
                    ..Default::default()
                },
            })
            .expect("sent");
        sender
            .send(audiosync_remote::Command::Master { volume: 40.0 })
            .expect("sent");
        sender
            .send(audiosync_remote::Command::Speaker {
                id: "gone".to_owned(),
                change: audiosync_remote::SpeakerChange {
                    on: Some(true),
                    ..Default::default()
                },
            })
            .expect("sent");
        app.poll_remote();
        let state = app.snapshot();
        let turned = state.speaker(&speaker).expect("still there");
        assert!(turned.enabled);
        assert_eq!(turned.volume, 20.0);
        assert!(state.running, "turning a speaker on starts streaming");
        assert_eq!(state.master_volume, 40.0);
    }

    #[test]
    fn a_source_picked_on_the_page_is_the_windows_pick() {
        let app = over_loopback(MockBackend::new());
        let sender = {
            let (sender, receiver) = std::sync::mpsc::channel();
            app.remote.borrow_mut().commands = Some(receiver);
            sender
        };
        let pick = |id: &str| audiosync_remote::Command::Source { id: id.to_owned() };
        sender.send(pick("input:line")).expect("sent");
        sender.send(pick("app:gone")).expect("sent");
        app.poll_remote();
        assert_eq!(app.snapshot().source_id, "input:line");
    }

    #[test]
    fn the_page_sets_the_airplay_delay_and_cant_turn_on_a_speaker_that_asks_for_a_code() {
        let app = over_loopback(MockBackend::new());
        let state = app.snapshot();
        let airplay = |availability| {
            state
                .speakers
                .iter()
                .find(|speaker| {
                    speaker.kind == crate::ui_model::SpeakerKind::AirPlay
                        && speaker.availability == availability
                        && !speaker.enabled
                })
                .expect("an AirPlay speaker")
                .id
                .clone()
        };
        let open = airplay(crate::ui_model::Availability::Open);
        let asks = airplay(crate::ui_model::Availability::NeedsCode);
        let (sender, receiver) = std::sync::mpsc::channel();
        app.remote.borrow_mut().commands = Some(receiver);
        let send = |id: &str, change| {
            sender
                .send(audiosync_remote::Command::Speaker {
                    id: id.to_owned(),
                    change,
                })
                .expect("sent");
        };
        send(
            &open,
            audiosync_remote::SpeakerChange {
                delay_ms: Some(2100),
                ..Default::default()
            },
        );
        send(
            &asks,
            audiosync_remote::SpeakerChange {
                on: Some(true),
                ..Default::default()
            },
        );
        app.take_remote_commands();
        let state = app.snapshot();
        assert_eq!(state.airplay_delay_ms, Some(2100.0));
        assert!(!state.speaker(&asks).expect("listed").enabled);
    }

    /// The mock with a zone of the kitchen and living room speakers and a
    /// preset of the kitchen and desk speakers.
    fn downstairs() -> MockBackend {
        let mut mock = MockBackend::new();
        mock.add_zone(
            "zone:1",
            "Downstairs",
            &["airplay:kitchen", "airplay:living"],
        );
        mock.add_preset("preset:1", "Evening", &["airplay:kitchen", "local:desk"]);
        mock
    }

    // covers: S257, S266
    #[test]
    fn phone_zone_and_preset_commands_apply_without_showing_the_main_window() {
        let app = over_loopback(downstairs());
        let main_shown = app.main.window().is_visible();
        let (sender, receiver) = std::sync::mpsc::channel();
        app.remote.borrow_mut().commands = Some(receiver);
        for command in [
            audiosync_remote::Command::Zone {
                id: "zone:1".into(),
                change: audiosync_remote::ZoneChange {
                    toggle: true,
                    volume: Some(30.0),
                },
            },
            audiosync_remote::Command::Preset {
                id: "preset:1".into(),
            },
        ] {
            sender.send(command).expect("sent");
        }
        app.backend
            .borrow_mut()
            .set_speaker_enabled("airplay:kitchen", false);
        app.take_remote_commands();
        let state = app.snapshot();
        for key in ["airplay:kitchen", "local:desk"] {
            assert!(state.speaker(key).expect("listed").enabled, "{key}");
        }
        assert_eq!(state.zones[0].volume, 30.0);
        assert!(state.presets[0].current);
        assert_eq!(
            app.main.window().is_visible(),
            main_shown,
            "a phone command leaves the main window as it was"
        );
    }

    // covers: S257
    #[test]
    fn a_phone_zone_toggle_or_recall_leaves_a_code_asking_member_off_and_opens_no_pin_dialog() {
        let mut backend = downstairs();
        backend.add_preset("preset:2", "Night", &["airplay:living", "local:desk"]);
        let app = over_loopback(backend);
        app.backend
            .borrow_mut()
            .set_speaker_enabled("airplay:kitchen", false);
        let (sender, receiver) = std::sync::mpsc::channel();
        app.remote.borrow_mut().commands = Some(receiver);
        let zone = audiosync_remote::Command::Zone {
            id: "zone:1".into(),
            change: audiosync_remote::ZoneChange {
                toggle: true,
                volume: None,
            },
        };
        let recall = audiosync_remote::Command::Preset {
            id: "preset:2".into(),
        };
        let left_off = |app: &super::App| {
            let state = app.snapshot();
            !state.speaker("airplay:living").expect("listed").enabled
                && state.pin_prompt.is_none()
                && !app.pin_dialog.window().is_visible()
        };
        sender.send(zone).expect("sent");
        app.take_remote_commands();
        assert!(app.snapshot().speaker("airplay:kitchen").unwrap().enabled);
        assert!(
            left_off(&app),
            "the zone plays without the member that asks for a code"
        );
        sender.send(recall).expect("sent");
        app.take_remote_commands();
        assert!(app.snapshot().speaker("local:desk").unwrap().enabled);
        assert!(left_off(&app), "the preset plays without it too");

        app.backend.borrow_mut().recall_preset("preset:2");
        app.refresh();
        assert!(
            app.snapshot().pin_prompt.is_some(),
            "a recall at the PC still turns it on and asks for the code"
        );
    }

    struct FakeProfiles(std::sync::Mutex<Vec<Category>>);

    impl ProfileSource for FakeProfiles {
        fn connected(&self) -> Option<Vec<Category>> {
            Some(self.0.lock().unwrap().clone())
        }
    }

    fn public_notices(app: &super::App) -> usize {
        app.snapshot()
            .notices
            .iter()
            .filter(|notice| notice.message == NoticeMessage::RemoteOnPublicNetwork)
            .count()
    }

    /// Polls until the window's Public line reads `public`.
    fn poll_until_public(app: &super::App, public: bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            app.poll_remote();
            if app.remote_window.get_public_network() == public {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "no reading");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_public_network_shows_in_the_window_and_as_a_notice_until_dismissed() {
        let app = over_loopback(MockBackend::new());
        let fake = Arc::new(FakeProfiles(std::sync::Mutex::new(vec![
            Category::Private,
            Category::Public,
        ])));
        app.remote.borrow_mut().profiles = Some(fake.clone());
        app.ui.borrow_mut().remote.port = free_port();
        app.main.invoke_remote_requested();
        app.remote_window.invoke_remote_toggled(true);
        poll_until_public(&app, true);
        assert_eq!(public_notices(&app), 1);

        app.main
            .invoke_notice_dismissed(super::REMOTE_PUBLIC_NOTICE_ID as i32);
        assert_eq!(public_notices(&app), 0, "dismissed");
        assert!(
            app.remote_window.get_public_network(),
            "the window still says so"
        );

        *fake.0.lock().unwrap() = vec![Category::Private];
        poll_until_public(&app, false);
        *fake.0.lock().unwrap() = vec![Category::Public];
        poll_until_public(&app, true);
        assert_eq!(public_notices(&app), 1, "back on a Public network");

        app.remote_window.invoke_remote_toggled(false);
        assert_eq!(public_notices(&app), 0, "the remote is off");
        assert!(!app.remote_window.get_public_network());
    }
}
