//! Settings > Check Setup… (`ui/setup.slint`, S435). The rows' states come from
//! `setup_check.rs`: the network's from the connected networks' profiles,
//! read on a thread while the window is open, and the microphone's from every
//! refresh, open or not, since Windows says it blocks the microphone only
//! when an input opens. A state that changes while the window shows is
//! announced to Narrator, and so is the line under the heading.

use std::rc::Rc;

use slint::ComponentHandle;

use super::{App, remote_window, with_app};
use crate::network_profile::Watch;
use crate::setup_check::{self, Microphone, Network};
use crate::ui_model::AppState;
use crate::{SetupMicrophone, SetupNetwork, Strings};

/// What the window shows, kept while it is closed.
#[derive(Default)]
pub(super) struct SetupCheck {
    watch: Option<Watch>,
    network: Option<Network>,
    microphone: Microphone,
}

impl App {
    pub(super) fn connect_setup(self: &Rc<Self>) {
        self.follow_frame(&self.setup(), "Check Setup");
        let weak = Rc::downgrade(self);
        let w = weak.clone();
        self.setup()
            .on_open_network_settings(move || with_app(&w, App::open_network_settings));
        let w = weak.clone();
        self.setup()
            .on_open_privacy_settings(move || with_app(&w, App::open_microphone_privacy));
        let w = weak.clone();
        self.setup()
            .on_close_requested(move || with_app(&w, App::hide_setup));
        self.setup().on_exit_requested(|| {
            let _ = slint::quit_event_loop();
        });
        let w = weak;
        self.setup().window().on_close_requested(move || {
            with_app(&w, App::hide_setup);
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// Opens Check Setup, or brings it to the front when it is open.
    pub fn show_setup(&self) {
        self.setup().set_checking(true);
        self.open_setup();
    }

    /// Opens it by itself once, with the first install's heading
    /// (AppDelegate+Windows.swift:234-238).
    pub fn show_setup_at_first_install(&self) {
        self.log.log(|| "Setup: first install".to_owned());
        self.setup().set_checking(false);
        self.open_setup();
    }

    fn open_setup(&self) {
        let opening = !self.setup.is_visible();
        if opening {
            self.setup().set_network_changes(0);
            self.setup().set_microphone_changes(0);
            self.setup().set_announcement("".into());
            // An earlier opening's reading may be stale: the row waits for a new one.
            self.setup().set_network(SetupNetwork::Unknown);
            self.setup().set_network_needs_attention(false);
            let profiles = self.remote.borrow().profiles.clone();
            let mut check = self.setup_check.borrow_mut();
            check.network = None;
            check.watch = profiles.map(|source| Watch::start(source, remote_window::PROFILE_CHECK));
            // A rebuilt window has not seen the last microphone reading.
            let attention = setup_check::attention(None, check.microphone);
            self.setup()
                .set_microphone(setup_microphone(check.microphone));
            self.setup()
                .set_microphone_needs_attention(attention.microphone);
        }
        if let Err(error) = self.setup().show() {
            self.show_window_error(&error);
            return;
        }
        if opening {
            self.log.log(|| "Check Setup: opened".to_owned());
        }
        self.setup().invoke_take_focus();
        crate::platform::bring_to_front(self.setup().window());
    }

    pub(super) fn hide_setup(&self) {
        // A window that fails to hide stays open; the user can still close it.
        if let Some(setup) = self.setup.built() {
            let _ = setup.hide();
        }
        self.setup_check.borrow_mut().watch = None;
    }

    /// Takes the newest reading of the networks' profiles, on every poll.
    pub(super) fn read_setup_network(&self) {
        let reading = self
            .setup_check
            .borrow()
            .watch
            .as_ref()
            .and_then(Watch::latest);
        if let Some(Some(connected)) = reading {
            let microphone = self.setup_check.borrow().microphone;
            self.show_setup_check(Some(Network::from_reading(&connected)), microphone);
        }
    }

    pub(super) fn refresh_setup(&self, state: &AppState) {
        let (network, microphone) = {
            let check = self.setup_check.borrow();
            (check.network, check.microphone.after(state))
        };
        self.show_setup_check(network, microphone);
    }

    pub(super) fn show_setup_check(&self, network: Option<Network>, microphone: Microphone) {
        let (network_before, microphone_before) = {
            let mut check = self.setup_check.borrow_mut();
            let before = (check.network, check.microphone);
            check.network = network;
            check.microphone = microphone;
            before
        };
        if (network, microphone) == (network_before, microphone_before) {
            return;
        }
        let attention = setup_check::attention(network, microphone);
        let attention_before = setup_check::attention(network_before, microphone_before);
        let Some(setup) = self.setup.built() else {
            return;
        };
        let setup = &setup;
        setup.set_network(setup_network(network));
        setup.set_microphone(setup_microphone(microphone));
        setup.set_network_needs_attention(attention.network);
        setup.set_microphone_needs_attention(attention.microphone);
        // The first reading after opening fills the row; it isn't a change.
        if !setup.window().is_visible() || (network_before.is_none() && network != network_before) {
            return;
        }
        let strings = setup.global::<Strings>();
        let mut said = Vec::new();
        if network != network_before {
            setup.set_network_changes(setup.get_network_changes() + 1);
            said.push(format!(
                "{}: {}",
                strings.get_setup_network(),
                setup.invoke_network_status(setup_network(network))
            ));
        }
        if microphone != microphone_before {
            setup.set_microphone_changes(setup.get_microphone_changes() + 1);
            said.push(format!(
                "{}: {}",
                strings.get_setup_microphone(),
                setup.invoke_microphone_status(setup_microphone(microphone))
            ));
        }
        if attention != attention_before && setup.get_checking() {
            said.push(format!(
                "{}. {}",
                strings.invoke_setup_summary_heading(attention.network),
                strings.invoke_setup_summary(attention.network, attention.microphone)
            ));
        }
        setup.set_announcement(said.join(" ").into());
    }
}

fn setup_network(network: Option<Network>) -> SetupNetwork {
    match network {
        None => SetupNetwork::Unknown,
        Some(Network::Private) => SetupNetwork::Private,
        Some(Network::Public) => SetupNetwork::Public,
        Some(Network::Offline) => SetupNetwork::Offline,
    }
}

fn setup_microphone(microphone: Microphone) -> SetupMicrophone {
    match microphone {
        Microphone::NotUsed => SetupMicrophone::NotUsed,
        Microphone::Working => SetupMicrophone::Working,
        Microphone::Blocked => SetupMicrophone::Blocked,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use slint::platform::Key;

    use super::super::tests::{app_over, draw_frames, leaked, press};
    use crate::network_profile::{Category, ProfileSource};
    use crate::ui_model::{Backend, MockBackend, NoticeMessage};
    use crate::{SetupMicrophone, SetupNetwork};

    struct Profiles(Mutex<Vec<Category>>);

    impl ProfileSource for Profiles {
        fn connected(&self) -> Option<Vec<Category>> {
            Some(self.0.lock().expect("not poisoned").clone())
        }
    }

    fn poll_until(app: &super::App, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "no reading came in");
            std::thread::sleep(Duration::from_millis(5));
            app.poll();
        }
    }

    // covers: S435
    #[test]
    fn check_setup_opens_from_the_file_menu_and_escape_closes_it() {
        let app = app_over(MockBackend::new());
        let window = leaked(app.setup());
        assert!(!window.is_visible());
        app.main.invoke_setup_requested();
        draw_frames();
        assert!(window.is_visible(), "Settings > Check Setup… opens it");
        assert_eq!(
            app.strings().invoke_setup_summary(false, false),
            "Nothing here needs your attention."
        );
        press(window, Key::Escape);
        assert!(!window.is_visible(), "Escape closes it");
        app.main.invoke_setup_requested();
        press(window, Key::Return);
        assert!(
            !window.is_visible(),
            "Return presses Done (SetupWindow.swift:106-107)"
        );
        app.main.invoke_setup_requested();
        app.setup().invoke_close_requested();
        assert!(!window.is_visible(), "Close closes it");
    }

    // covers: S435
    #[test]
    fn the_network_row_follows_the_profile_and_a_change_is_announced() {
        let profiles = Arc::new(Profiles(Mutex::new(vec![Category::Public])));
        let app = app_over(MockBackend::new());
        app.remote.borrow_mut().profiles = Some(profiles.clone());
        app.show_setup();
        poll_until(&app, || app.setup().get_network() == SetupNetwork::Public);
        assert!(app.setup().get_network_needs_attention());
        assert_eq!(
            app.setup().get_network_changes(),
            0,
            "the first reading fills it"
        );
        assert_eq!(app.setup().get_announcement(), "");
        assert_eq!(
            app.strings().invoke_setup_summary(true, false),
            "Network type needs your attention."
        );

        *profiles.0.lock().expect("not poisoned") = vec![Category::Private];
        poll_until(&app, || app.setup().get_network() == SetupNetwork::Private);
        assert!(!app.setup().get_network_needs_attention());
        assert_eq!(app.setup().get_network_changes(), 1);
        assert_eq!(
            app.setup().get_announcement(),
            "Network type: Private network Ready to stream. Nothing here needs your attention."
        );

        *profiles.0.lock().expect("not poisoned") = Vec::new();
        poll_until(&app, || app.setup().get_network() == SetupNetwork::Offline);
        assert!(app.setup().get_network_needs_attention());
        app.setup().invoke_close_requested();
        assert!(
            app.setup_check.borrow().watch.is_none(),
            "no reading while closed"
        );
    }

    // covers: S435
    #[test]
    fn the_microphone_row_turns_blocked_with_the_notice_and_names_it_in_the_summary() {
        let mut backend = MockBackend::new();
        backend.select_source("input:line");
        backend.post(
            "source",
            NoticeMessage::MicrophoneBlocked {
                source: "Line In".to_owned(),
            },
            "09.40",
        );
        let app = app_over(backend);
        app.show_setup();
        assert_eq!(app.setup().get_microphone(), SetupMicrophone::Blocked);
        assert!(app.setup().get_microphone_needs_attention());
        assert_eq!(
            app.strings().invoke_setup_summary(true, true),
            "Network type needs your attention. Not allowed, used only when you need them: Microphone."
        );

        app.update(|b| b.dismiss_all_notices());
        assert_eq!(
            app.setup().get_microphone(),
            SetupMicrophone::Blocked,
            "kept"
        );
        app.update(|b| b.set_running(true));
        assert_eq!(app.setup().get_microphone(), SetupMicrophone::Working);
        assert_eq!(app.setup().get_microphone_changes(), 1);
        assert!(
            app.setup()
                .get_announcement()
                .starts_with("Microphone: Working")
        );
    }

    // The Mac's names (Localization.swift:1471-1472, 1519, 1562-1563,
    // SetupWindow.swift:61, 92): window "Setup", Settings box "Permissions",
    // "Before you start" when it opens by itself, ready or not from Check Setup.
    #[test]
    fn the_window_box_and_heading_carry_the_macs_names() {
        let strings = include_str!("../../ui/strings.slint");
        for line in [
            "setup-title: @tr(\"Permissions\");",
            "setup-window-title: @tr(\"Setup\");",
            "setup-heading: @tr(\"Before you start\");",
            "setup-done: @tr(\"Done\");",
            "@tr(\"Ready to stream\")",
            "@tr(\"Not ready to stream\")",
        ] {
            assert!(strings.contains(line), "{line}");
        }
        let setup = include_str!("../../ui/setup.slint");
        assert!(setup.contains("title: Strings.setup-window-title;"));
        assert!(setup.contains(
            "root.checking ? Strings.setup-summary-heading(root.network-needs-attention) : Strings.setup-heading"
        ));
    }

    #[test]
    fn the_first_install_shows_the_intro_and_check_setup_the_summary() {
        let app = app_over(MockBackend::new());
        app.show_setup_at_first_install();
        assert!(app.setup.is_visible());
        assert!(!app.setup().get_checking(), "Before you start");
        app.setup().invoke_close_requested();
        app.main.invoke_setup_requested();
        assert!(app.setup().get_checking(), "Ready to stream or not");
    }

    // AppDelegate.swift:477, AppDelegate+Windows.swift:234-238: a first
    // install opens it once by itself, after the main window.
    #[test]
    fn a_first_install_opens_setup_by_itself_after_the_main_window() {
        let main = include_str!("../main.rs").replace("\r\n", "\n");
        let shown = main.find("app.show_main();").expect("main shows");
        let first = main
            .find("app.show_setup_at_first_install();")
            .expect("first install opens Setup");
        assert!(first > shown);
    }

    // SetupWindow.swift:99-100, 114, 162, 115/158/181 and PermissionFlow.swift:154-156:
    // two groups (network before capture), a fixed 480 width, teal row icons,
    // and no flash or state fade when animations are off.
    #[test]
    fn the_rows_sit_in_the_macs_two_groups_with_its_width_icons_and_motion() {
        let setup = include_str!("../../ui/setup.slint").replace("\r\n", "\n");
        let at = |needle: &str| setup.find(needle).unwrap_or_else(|| panic!("{needle}"));
        assert!(at("Strings.setup-needed") < at("title: Strings.setup-network;"));
        assert!(at("title: Strings.setup-network;") < at("title: Strings.setup-capture;"));
        assert!(at("title: Strings.setup-capture;") < at("Strings.setup-later"));
        assert!(at("Strings.setup-later") < at("title: Strings.setup-microphone;"));
        assert!(setup.contains("    width: 480px;\n"));
        assert!(!setup.contains("preferred-width: 520px;"));
        assert!(setup.contains(
            "stroke: Tokens.teal-text;\n                stroke-width: root.icon-stroke;"
        ));
        assert!(setup.contains("if Motion.on && root.changes > 0"));
        assert!(setup.contains("enter: Motion.on;"));
    }
}
