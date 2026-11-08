//! File > Check Setup… (S435): what the window's network and microphone
//! rows say, and which of them the line under its heading names. Windows
//! asks no permission to capture sound or to find speakers, so only these
//! two rows can need the user.

use crate::network_profile::Category;
use crate::ui_model::{AppState, NoticeMessage, SourceKind};

/// The Network type row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Private,
    Public,
    Offline,
}

impl Network {
    /// From the categories of the connected networks: one Public network is
    /// enough, as for the web remote's warning (S390).
    pub fn from_reading(connected: &[Category]) -> Network {
        if connected.is_empty() {
            Network::Offline
        } else if connected.contains(&Category::Public) {
            Network::Public
        } else {
            Network::Private
        }
    }
}

/// The Microphone row. Windows tells AudioSync it blocks the microphone only
/// when AudioSync opens an input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Microphone {
    #[default]
    NotUsed,
    Working,
    Blocked,
}

impl Microphone {
    /// The row after the app shows `state`: blocked while the notice says
    /// Windows blocks the microphone, working while an input device streams,
    /// else as it was, so a dismissed notice leaves it blocked until an input
    /// opens again.
    pub fn after(self, state: &AppState) -> Microphone {
        let notice = state
            .notices
            .iter()
            .any(|n| matches!(n.message, NoticeMessage::MicrophoneBlocked { .. }));
        let input = state
            .sources
            .iter()
            .any(|s| s.id == state.source_id && s.kind == SourceKind::Input);
        if notice {
            Microphone::Blocked
        } else if state.running && input {
            Microphone::Working
        } else {
            self
        }
    }
}

/// The rows the line under the heading names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attention {
    pub network: bool,
    pub microphone: bool,
}

/// What needs attention, before the first network reading too (`None`).
pub fn attention(network: Option<Network>, microphone: Microphone) -> Attention {
    Attention {
        network: matches!(network, Some(Network::Public | Network::Offline)),
        microphone: microphone == Microphone::Blocked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_profile::Category::{Domain, Private, Public};
    use crate::ui_model::{Backend, MockBackend, NoticeMessage};

    // covers: S435
    #[test]
    fn one_public_network_makes_the_row_public_and_none_is_offline() {
        assert_eq!(Network::from_reading(&[Private]), Network::Private);
        assert_eq!(Network::from_reading(&[Domain]), Network::Private);
        assert_eq!(Network::from_reading(&[Private, Public]), Network::Public);
        assert_eq!(Network::from_reading(&[]), Network::Offline);
    }

    fn blocked() -> NoticeMessage {
        NoticeMessage::MicrophoneBlocked {
            source: "Line In".to_owned(),
        }
    }

    // covers: S435
    #[test]
    fn the_microphone_row_follows_the_input_and_its_notice() {
        let mut backend = MockBackend::new();
        let idle = backend.snapshot();
        assert_eq!(Microphone::NotUsed.after(&idle), Microphone::NotUsed);

        backend.select_source("input:line");
        assert_eq!(
            Microphone::NotUsed.after(&backend.snapshot()),
            Microphone::NotUsed,
            "chosen but not opened"
        );
        backend.set_running(true);
        let streaming = backend.snapshot();
        assert_eq!(Microphone::NotUsed.after(&streaming), Microphone::Working);

        backend.set_running(false);
        backend.post("source", blocked(), "09.40");
        let refused = backend.snapshot();
        assert_eq!(Microphone::Working.after(&refused), Microphone::Blocked);

        backend.dismiss_all_notices();
        assert_eq!(
            Microphone::Blocked.after(&backend.snapshot()),
            Microphone::Blocked,
            "kept until an input opens again"
        );
        assert_eq!(Microphone::Blocked.after(&streaming), Microphone::Working);
        assert_eq!(Microphone::Working.after(&idle), Microphone::Working);
    }

    // covers: S435
    #[test]
    fn the_summary_names_a_public_or_missing_network_and_a_blocked_microphone() {
        let none = Attention::default();
        assert_eq!(attention(Some(Network::Private), Microphone::Working), none);
        assert_eq!(
            attention(None, Microphone::NotUsed),
            none,
            "before a reading"
        );
        assert_eq!(
            attention(Some(Network::Public), Microphone::NotUsed),
            Attention {
                network: true,
                microphone: false
            }
        );
        assert_eq!(
            attention(Some(Network::Offline), Microphone::Blocked),
            Attention {
                network: true,
                microphone: true
            }
        );
        assert_eq!(
            attention(Some(Network::Private), Microphone::Blocked),
            Attention {
                network: false,
                microphone: true
            }
        );
    }
}
