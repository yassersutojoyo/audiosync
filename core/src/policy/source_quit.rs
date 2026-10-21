//! What a running session does when the app it captures quits (S226): stop,
//! as S142 always did, or go on with whole-system audio.

use super::source::AudioSource;

/// The user's setting for a source app quitting (S226).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WhenSourceQuits {
    /// Stop the session (S142).
    #[default]
    Stop,
    /// Go on capturing the whole system.
    SwitchToSystem,
}

impl WhenSourceQuits {
    /// Reads the stored form: `"system"` is [`WhenSourceQuits::SwitchToSystem`];
    /// anything else, a missing value included, is the default, so a value
    /// written by a newer copy never switches an older one by surprise.
    pub fn from_stored(value: Option<&str>) -> WhenSourceQuits {
        match value {
            Some("system") => WhenSourceQuits::SwitchToSystem,
            _ => WhenSourceQuits::Stop,
        }
    }

    /// The stored form.
    pub fn stored(self) -> &'static str {
        match self {
            WhenSourceQuits::Stop => "stop",
            WhenSourceQuits::SwitchToSystem => "system",
        }
    }
}

/// Whether streaming is on when an app quits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Streaming {
    /// No session runs.
    Off,
    /// A session runs.
    Running,
    /// A session is being rebuilt (a capture format change, or on Windows a
    /// capture starting again); streaming counts as on.
    Rebuilding,
}

/// What the host does about an event (S226).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceQuitAction {
    /// Nothing changes.
    Keep,
    /// Stop as S142 says, dropping any pending rebuild.
    Stop,
    /// Change the source to the whole system as a source change does (S141,
    /// S186), store it, and post the informational "source" notice; a pending
    /// rebuild starts with the whole system instead.
    SwitchToSystem,
}

/// The decision for one host, fed with the setting and the app events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceQuitWatch {
    setting: WhenSourceQuits,
    switched_from: Option<String>,
}

impl SourceQuitWatch {
    /// A watch with the stored setting.
    pub fn new(setting: WhenSourceQuits) -> SourceQuitWatch {
        SourceQuitWatch {
            setting,
            switched_from: None,
        }
    }

    /// The setting in force.
    pub fn setting(&self) -> WhenSourceQuits {
        self.setting
    }

    /// The app the session switched away from when it quit, until the user
    /// picks a source; the notice names it.
    pub fn switched_from(&self) -> Option<&str> {
        self.switched_from.as_deref()
    }

    /// The user changed the setting. It decides the next quit only and never
    /// undoes a switch already made.
    pub fn set_setting(&mut self, setting: WhenSourceQuits) -> SourceQuitAction {
        self.setting = setting;
        SourceQuitAction::Keep
    }

    /// An app quit: `quit_app` is its ID as the host knows it (the bundle ID
    /// on the Mac), `source` the session's source. A repeated report finds
    /// the session stopped or the source switched, so it acts only once.
    pub fn app_exited(
        &mut self,
        source: &AudioSource,
        quit_app: Option<&str>,
        streaming: Streaming,
    ) -> SourceQuitAction {
        if streaming == Streaming::Off || !source.depends_on_app(quit_app) {
            return SourceQuitAction::Keep;
        }
        match self.setting {
            WhenSourceQuits::Stop => SourceQuitAction::Stop,
            WhenSourceQuits::SwitchToSystem => {
                self.switched_from = quit_app.map(str::to_owned);
                SourceQuitAction::SwitchToSystem
            }
        }
    }

    /// An app started. AudioSync never switches back by itself, even to the
    /// app it switched away from: other apps may be playing by then.
    pub fn app_relaunched(&mut self, app: Option<&str>) -> SourceQuitAction {
        let _ = app;
        SourceQuitAction::Keep
    }

    /// The user picked a source, so no switch is in force any more.
    pub fn source_picked(&mut self) {
        self.switched_from = None;
    }
}
