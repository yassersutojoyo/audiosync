//! The guide behind the double-audio notice's Show How button: which steps
//! it shows, picked from what the notice says.
//!
//! Windows keeps playing the source on its own outputs next to AudioSync's
//! delayed copies, and muting the app in the Volume mixer silences what
//! AudioSync captures too. So the fix is routing: send the app (or, while
//! the whole system is captured, Windows' default output) to an output the
//! user doesn't listen to. Muting an output helps only where AudioSync
//! doesn't play. AudioSync changes none of these settings itself: a step
//! can open the Settings page where the user makes the change.
//!
//! The steps carry names only; the UI words them (`ui/strings.slint`).

use crate::ui_model::{Notice, NoticeMessage};

/// A Settings page a step opens, by its documented `ms-settings:` URI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsPage {
    /// System > Sound > Volume mixer, where each app's output is chosen.
    VolumeMixer,
    /// System > Sound, with the default output and each output's volume.
    Sound,
    /// Privacy & security > Microphone, where desktop apps get microphone
    /// access. Opened by the microphone notice, not by a guide step.
    MicrophonePrivacy,
}

impl SettingsPage {
    pub fn uri(self) -> &'static str {
        match self {
            SettingsPage::VolumeMixer => "ms-settings:apps-volume",
            SettingsPage::Sound => "ms-settings:sound",
            SettingsPage::MicrophonePrivacy => "ms-settings:privacy-microphone",
        }
    }
}

/// One paragraph of the guide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuideStep {
    /// Send `app` to an output the user doesn't listen to.
    RouteApp { app: String },
    /// Make an output the user doesn't listen to the Windows default,
    /// instead of `current`.
    ChangeDefault { current: String },
    /// Apps play straight to `outputs` (the whole system, outputs other than
    /// the default): send them elsewhere too.
    RouteOtherApps { outputs: Vec<String> },
    /// Or mute `outputs`, which AudioSync doesn't play to. Offered only when
    /// that alone is enough.
    MuteOutputs { outputs: Vec<String> },
    /// Muting `outputs` doesn't help: AudioSync plays there too.
    SharedOutputs { outputs: Vec<String> },
    /// Don't mute `app` itself in the Volume mixer: AudioSync would capture
    /// silence.
    KeepAppUnmuted { app: String },
}

impl GuideStep {
    /// The Settings page the step's button opens, if it has one.
    pub fn page(&self) -> Option<SettingsPage> {
        match self {
            GuideStep::RouteApp { .. } | GuideStep::RouteOtherApps { .. } => {
                Some(SettingsPage::VolumeMixer)
            }
            GuideStep::ChangeDefault { .. } | GuideStep::MuteOutputs { .. } => {
                Some(SettingsPage::Sound)
            }
            GuideStep::SharedOutputs { .. } | GuideStep::KeepAppUnmuted { .. } => None,
        }
    }
}

/// What the guide shows for one double-audio notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guide {
    /// The app's name, or `None` for the whole system.
    pub source: Option<String>,
    /// Every output the source is heard from straight, for the intro.
    pub outputs: Vec<String>,
    pub steps: Vec<GuideStep>,
}

/// The guide for `message`, or `None` for a notice that has none.
pub fn guide(message: &NoticeMessage) -> Option<Guide> {
    let NoticeMessage::HeardTwice {
        source,
        mute,
        shared,
        default_output,
    } = message
    else {
        return None;
    };
    let outputs: Vec<String> = mute.iter().chain(shared).cloned().collect();
    if outputs.is_empty() {
        return None;
    }
    let mut steps = Vec::new();
    match source {
        Some(app) => steps.push(GuideStep::RouteApp { app: app.clone() }),
        None => {
            if let Some(current) = default_output {
                steps.push(GuideStep::ChangeDefault {
                    current: current.clone(),
                });
            }
            let others: Vec<String> = outputs
                .iter()
                .filter(|name| Some(*name) != default_output.as_ref())
                .cloned()
                .collect();
            if !others.is_empty() {
                steps.push(GuideStep::RouteOtherApps { outputs: others });
            }
        }
    }
    if shared.is_empty() {
        steps.push(GuideStep::MuteOutputs {
            outputs: mute.clone(),
        });
    } else {
        steps.push(GuideStep::SharedOutputs {
            outputs: shared.clone(),
        });
    }
    if let Some(app) = source {
        steps.push(GuideStep::KeepAppUnmuted { app: app.clone() });
    }
    Some(Guide {
        source: source.clone(),
        outputs,
        steps,
    })
}

/// The guide of the double-audio notice among `notices`, if one is
/// showing: what an open guide shows now, or `None` once it should close.
pub fn current(notices: &[Notice]) -> Option<Guide> {
    notices.iter().find_map(|notice| guide(&notice.message))
}

/// The ID of the double-audio notice among `notices`, if one is showing:
/// the notice whose guide Show How in the AudioSync Guide opens.
pub fn notice_with_guide(notices: &[Notice]) -> Option<u64> {
    notices
        .iter()
        .find(|notice| guide(&notice.message).is_some())
        .map(|notice| notice.id)
}

/// A made-up double-audio notice to look at the guide without streaming:
/// `app` for one app, `system` for the whole system. With `AUDIOSYNC_DEBUG=1`,
/// the app opens the guide for it at launch when `AUDIOSYNC_PREVIEW_GUIDE`
/// names one.
pub fn preview(kind: &str) -> Option<NoticeMessage> {
    let names = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect();
    match kind {
        "app" => Some(NoticeMessage::HeardTwice {
            source: Some("Music Player".to_owned()),
            mute: names(&["TV", "Monitor"]),
            shared: Vec::new(),
            default_output: None,
        }),
        "system" => Some(NoticeMessage::HeardTwice {
            source: None,
            mute: names(&["TV"]),
            shared: names(&["Speakers"]),
            default_output: Some("Speakers".to_owned()),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn twice(
        source: Option<&str>,
        mute: &[&str],
        shared: &[&str],
        default_output: Option<&str>,
    ) -> NoticeMessage {
        NoticeMessage::HeardTwice {
            source: source.map(str::to_owned),
            mute: names(mute),
            shared: names(shared),
            default_output: default_output.map(str::to_owned),
        }
    }

    fn steps(message: &NoticeMessage) -> Vec<GuideStep> {
        guide(message).expect("a guide").steps
    }

    #[test]
    fn only_the_double_audio_notice_has_a_guide() {
        assert_eq!(guide(&NoticeMessage::NoSpeaker), None);
        assert_eq!(guide(&twice(Some("Player"), &[], &[], None)), None);
    }

    #[test]
    fn an_app_heard_where_audiosync_doesnt_play_can_be_moved_or_muted() {
        let message = twice(Some("Player"), &["TV"], &[], None);
        let app = "Player".to_owned();
        assert_eq!(
            steps(&message),
            [
                GuideStep::RouteApp { app: app.clone() },
                GuideStep::MuteOutputs {
                    outputs: names(&["TV"])
                },
                GuideStep::KeepAppUnmuted { app },
            ]
        );
        let guide = guide(&message).unwrap();
        assert_eq!(guide.source.as_deref(), Some("Player"));
        assert_eq!(guide.outputs, names(&["TV"]));
    }

    #[test]
    fn an_app_heard_where_audiosync_plays_too_gets_no_mute_advice() {
        let message = twice(Some("Player"), &["TV"], &["Speakers"], None);
        let app = "Player".to_owned();
        assert_eq!(
            steps(&message),
            [
                GuideStep::RouteApp { app: app.clone() },
                GuideStep::SharedOutputs {
                    outputs: names(&["Speakers"])
                },
                GuideStep::KeepAppUnmuted { app },
            ]
        );
        assert_eq!(guide(&message).unwrap().outputs, names(&["TV", "Speakers"]));
    }

    #[test]
    fn the_whole_system_changes_the_default_output() {
        let message = twice(None, &["Speakers"], &[], Some("Speakers"));
        assert_eq!(
            steps(&message),
            [
                GuideStep::ChangeDefault {
                    current: "Speakers".to_owned()
                },
                GuideStep::MuteOutputs {
                    outputs: names(&["Speakers"])
                },
            ]
        );
    }

    #[test]
    fn the_whole_system_moves_apps_that_play_to_other_outputs() {
        let message = twice(None, &["TV"], &["Speakers"], Some("Speakers"));
        assert_eq!(
            steps(&message),
            [
                GuideStep::ChangeDefault {
                    current: "Speakers".to_owned()
                },
                GuideStep::RouteOtherApps {
                    outputs: names(&["TV"])
                },
                GuideStep::SharedOutputs {
                    outputs: names(&["Speakers"])
                },
            ]
        );
        // No default output among them: only apps play there.
        let message = twice(None, &["TV"], &[], None);
        assert_eq!(
            steps(&message),
            [
                GuideStep::RouteOtherApps {
                    outputs: names(&["TV"])
                },
                GuideStep::MuteOutputs {
                    outputs: names(&["TV"])
                },
            ]
        );
    }

    #[test]
    fn steps_open_documented_settings_pages_and_notes_open_none() {
        let pages: Vec<Option<&str>> = steps(&twice(Some("Player"), &["TV"], &[], None))
            .iter()
            .map(|step| step.page().map(SettingsPage::uri))
            .collect();
        assert_eq!(
            pages,
            [
                Some("ms-settings:apps-volume"),
                Some("ms-settings:sound"),
                None
            ]
        );
        assert_eq!(
            GuideStep::ChangeDefault {
                current: String::new()
            }
            .page(),
            Some(SettingsPage::Sound)
        );
        assert_eq!(
            GuideStep::SharedOutputs {
                outputs: Vec::new()
            }
            .page(),
            None
        );
    }

    #[test]
    fn an_open_guide_follows_the_notice_and_closes_without_it() {
        let notice = |id: u64, message: NoticeMessage| Notice {
            id,
            subject: "subject".to_owned(),
            message,
            time: "09.41".to_owned(),
        };
        let mut notices = vec![
            notice(1, NoticeMessage::NoSpeaker),
            notice(2, twice(Some("Player"), &["TV"], &[], None)),
        ];
        assert_eq!(
            current(&notices).map(|guide| guide.outputs),
            Some(names(&["TV"]))
        );
        // The same notice changed in place: AudioSync plays to the TV now.
        notices[1].message = twice(Some("Player"), &[], &["TV"], None);
        assert_eq!(
            current(&notices).map(|guide| guide.steps[1].clone()),
            Some(GuideStep::SharedOutputs {
                outputs: names(&["TV"])
            })
        );
        // The app quit, or streaming stopped: the notice is gone.
        notices.remove(1);
        assert_eq!(current(&notices), None);
    }

    #[test]
    fn show_how_in_the_audiosync_guide_finds_the_double_audio_notice() {
        let notice = |id: u64, message: NoticeMessage| Notice {
            id,
            subject: "subject".to_owned(),
            message,
            time: "09.41".to_owned(),
        };
        let mut notices = vec![notice(4, NoticeMessage::NoSpeaker)];
        assert_eq!(notice_with_guide(&notices), None);
        notices.push(notice(7, twice(None, &["TV"], &[], None)));
        assert_eq!(notice_with_guide(&notices), Some(7));
        // A double-audio notice that names no output has no guide.
        notices[1].message = twice(None, &[], &[], None);
        assert_eq!(notice_with_guide(&notices), None);
    }

    #[test]
    fn the_previews_have_guides() {
        for kind in ["app", "system"] {
            let message = preview(kind).expect("a preview");
            assert!(guide(&message).is_some(), "{kind}");
        }
        assert_eq!(preview("other"), None);
    }
}
