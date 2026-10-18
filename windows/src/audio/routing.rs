//! Double audio: whether the user hears the source straight from Windows as
//! well as through AudioSync's delayed copies.
//!
//! On the Mac, a process tap mutes the app it captures. Windows can't: muting
//! the app's session (`ISimpleAudioVolume`) silences the process loopback
//! capture too, while an endpoint's own mute (`IAudioEndpointVolume`) applies
//! after the capture taps the app (docs/WINDOWS_AUDIO_ROUTING.md, "Test 1
//! results"). So while AudioSync plays to at least one output, the source is
//! heard a second time, undelayed, from every render endpoint where it plays
//! and that isn't muted or at volume 0. An output AudioSync plays to counts
//! too: the original and the delayed copy then land on the same speaker. A
//! software device (a virtual cable) counts only while AudioSync plays to it:
//! otherwise it is taken for the output nobody listens to that the notice
//! suggests, since what passes its sound on to speakers is out of sight.
//!
//! - One app: an endpoint where a process of the captured tree plays.
//! - The whole system: Windows' default output, whatever plays there now,
//!   and any other endpoint where a process outside AudioSync's tree plays.
//!
//! The source plays on an endpoint while it has an active audio session
//! there, and for [`HOLD_CHECKS`] checks after its last one: sessions turn
//! inactive whenever an app pauses or goes quiet for a moment, and the
//! answer shouldn't flip with them. One app that is silent everywhere (most
//! likely paused) keeps counting on the endpoints AudioSync doesn't play to
//! for as long as it stays silent: it will play there again, and muting them
//! shows at once. An endpoint AudioSync plays to ages as usual, since moving
//! the app away from it shows only once the app plays again.
//!
//! [`check`] is the decision, plain Rust over a snapshot the driver takes;
//! the controller runs it every few seconds while streaming and reports a
//! change as [`BackendEvent::HeardTwice`](super::backend::BackendEvent).

use std::collections::BTreeSet;

/// How many checks an endpoint keeps counting after the source's last active
/// session there: about 15 seconds at the controller's pace.
pub const HOLD_CHECKS: u32 = 5;

/// What the capture takes in, by process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Captured {
    /// One app: these processes, its tree.
    Only(BTreeSet<u32>),
    /// The whole system: every process except these, AudioSync's own tree.
    AllExcept(BTreeSet<u32>),
}

impl Captured {
    /// Whether the capture takes in process `pid`.
    pub fn captures(&self, pid: u32) -> bool {
        match self {
            Captured::Only(pids) => pids.contains(&pid),
            Captured::AllExcept(pids) => !pids.contains(&pid),
        }
    }
}

/// An endpoint's mute and master volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EndpointVolume {
    /// Whether the endpoint is muted.
    pub muted: bool,
    /// The master volume, 0 to 1 (`GetMasterVolumeLevelScalar`).
    pub level: f32,
}

/// One active render endpoint, as the double-audio check reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct EndpointAudio {
    /// The endpoint ID.
    pub id: String,
    /// The name Windows shows for it; empty when it couldn't be read.
    pub name: String,
    /// Whether it is Windows' default output.
    pub is_default: bool,
    /// Whether it is a software device, such as a virtual cable
    /// ([`OutputKind::Virtual`](super::backend::OutputKind::Virtual)).
    pub virtual_device: bool,
    /// Its mute and volume, or `None` when they couldn't be read: then it
    /// counts as audible.
    pub volume: Option<EndpointVolume>,
    /// The processes with an active audio session on it.
    pub active_pids: Vec<u32>,
}

impl EndpointAudio {
    /// Whether what plays there is heard: not muted, and a volume above 0.
    pub fn audible(&self) -> bool {
        self.volume
            .is_none_or(|volume| !volume.muted && volume.level > 0.0)
    }
}

/// An endpoint the source plays on, carried from one check to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayedOn {
    /// The endpoint ID.
    pub endpoint_id: String,
    /// Checks since the source's last active session there: 0 while it has
    /// one.
    pub idle_checks: u32,
}

/// Why the source plays straight from an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectReason {
    /// The source plays there (see [`PlayedOn`]).
    Session,
    /// The whole system is captured and this is Windows' default output.
    DefaultEndpoint,
}

/// An endpoint the source is heard from straight, undelayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectPath {
    /// The endpoint ID.
    pub endpoint_id: String,
    /// Its name, for the notice.
    pub endpoint_name: String,
    /// Why the source plays there.
    pub reason: DirectReason,
    /// Whether AudioSync plays to it too, so muting it would silence
    /// AudioSync's copy as well.
    pub audiosync_plays_here: bool,
}

/// The answer of [`check`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutingCheck {
    /// The endpoints the source is heard from straight, by endpoint ID;
    /// empty when it is heard only through AudioSync.
    pub paths: Vec<DirectPath>,
    /// The endpoints the source plays on, by endpoint ID, for the next
    /// check's `last_played_on`.
    pub played_on: Vec<PlayedOn>,
}

/// Where the source captured as `captured` is heard straight, from the
/// active render endpoints in `endpoints`, while AudioSync plays to the
/// endpoints in `rendered`, after the last check's `played_on`. See the
/// module documentation.
pub fn check(
    captured: &Captured,
    endpoints: &[EndpointAudio],
    rendered: &[String],
    last_played_on: &[PlayedOn],
) -> RoutingCheck {
    let played_on = played_on(captured, endpoints, rendered, last_played_on);
    if rendered.is_empty() {
        // Without a copy, nothing is heard twice.
        return RoutingCheck {
            paths: Vec::new(),
            played_on,
        };
    }
    let whole_system = matches!(captured, Captured::AllExcept(_));
    let mut paths: Vec<DirectPath> = endpoints
        .iter()
        .filter_map(|endpoint| {
            let audiosync_plays_here = rendered.contains(&endpoint.id);
            if !endpoint.audible() || (endpoint.virtual_device && !audiosync_plays_here) {
                return None;
            }
            let reason = if whole_system && endpoint.is_default {
                DirectReason::DefaultEndpoint
            } else if played_on.iter().any(|p| p.endpoint_id == endpoint.id) {
                DirectReason::Session
            } else {
                return None;
            };
            Some(DirectPath {
                endpoint_id: endpoint.id.clone(),
                endpoint_name: endpoint.name.clone(),
                reason,
                audiosync_plays_here,
            })
        })
        .collect();
    paths.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    paths.dedup_by(|a, b| a.endpoint_id == b.endpoint_id);
    RoutingCheck { paths, played_on }
}

/// The endpoints the source plays on now, after `last`: see the module
/// documentation.
fn played_on(
    captured: &Captured,
    endpoints: &[EndpointAudio],
    rendered: &[String],
    last: &[PlayedOn],
) -> Vec<PlayedOn> {
    let _ = rendered;
    let mut active: Vec<PlayedOn> = endpoints
        .iter()
        .filter(|endpoint| {
            endpoint
                .active_pids
                .iter()
                .any(|&pid| captured.captures(pid))
        })
        .map(|endpoint| PlayedOn {
            endpoint_id: endpoint.id.clone(),
            idle_checks: 0,
        })
        .collect();
    if active.is_empty() {
        active = last.to_vec();
    }
    active.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    active.dedup_by(|a, b| a.endpoint_id == b.endpoint_id);
    active
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: u32 = 1000;
    const PLAYER: u32 = 2000;
    const PLAYER_HELPER: u32 = 2001;
    const OTHER: u32 = 3000;

    fn endpoint(id: &str, active_pids: &[u32]) -> EndpointAudio {
        EndpointAudio {
            id: id.to_owned(),
            name: id.to_uppercase(),
            is_default: false,
            virtual_device: false,
            volume: Some(EndpointVolume {
                muted: false,
                level: 0.5,
            }),
            active_pids: active_pids.to_vec(),
        }
    }

    fn default_endpoint(id: &str, active_pids: &[u32]) -> EndpointAudio {
        EndpointAudio {
            is_default: true,
            ..endpoint(id, active_pids)
        }
    }

    fn player() -> Captured {
        Captured::Only(BTreeSet::from([PLAYER, PLAYER_HELPER]))
    }

    fn whole_system() -> Captured {
        Captured::AllExcept(BTreeSet::from([OWN]))
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    /// Where the source played, with the checks since its last session.
    fn played(list: &[(&str, u32)]) -> Vec<PlayedOn> {
        list.iter()
            .map(|(id, idle_checks)| PlayedOn {
                endpoint_id: (*id).to_owned(),
                idle_checks: *idle_checks,
            })
            .collect()
    }

    /// The endpoint IDs and reasons of the answer.
    fn heard(check: &RoutingCheck) -> Vec<(&str, DirectReason)> {
        check
            .paths
            .iter()
            .map(|path| (path.endpoint_id.as_str(), path.reason))
            .collect()
    }

    /// Runs `checks` checks over the same endpoints from `last`, and returns
    /// the last answer.
    fn checks(
        checks: u32,
        captured: &Captured,
        endpoints: &[EndpointAudio],
        rendered: &[String],
        last: &[PlayedOn],
    ) -> RoutingCheck {
        let mut answer = RoutingCheck {
            paths: Vec::new(),
            played_on: last.to_vec(),
        };
        for _ in 0..checks {
            answer = check(captured, endpoints, rendered, &answer.played_on);
        }
        answer
    }

    #[test]
    fn an_app_playing_on_an_audible_endpoint_is_heard_twice() {
        let endpoints = [endpoint("speakers", &[PLAYER]), endpoint("tv", &[OWN])];
        let answer = check(&player(), &endpoints, &ids(&["tv"]), &[]);
        assert_eq!(
            answer.paths,
            [DirectPath {
                endpoint_id: "speakers".to_owned(),
                endpoint_name: "SPEAKERS".to_owned(),
                reason: DirectReason::Session,
                audiosync_plays_here: false,
            }]
        );
        assert_eq!(answer.played_on, played(&[("speakers", 0)]));
    }

    #[test]
    fn a_helper_process_of_the_app_counts_and_other_apps_dont() {
        let endpoints = [
            endpoint("speakers", &[OTHER]),
            endpoint("headphones", &[PLAYER_HELPER]),
            endpoint("tv", &[OWN]),
        ];
        let answer = check(&player(), &endpoints, &ids(&["tv"]), &[]);
        assert_eq!(heard(&answer), [("headphones", DirectReason::Session)]);
    }

    #[test]
    fn nothing_is_heard_twice_while_audiosync_plays_nowhere() {
        let endpoints = [endpoint("speakers", &[PLAYER])];
        let answer = check(&player(), &endpoints, &[], &[]);
        assert!(answer.paths.is_empty(), "{answer:?}");
        assert_eq!(
            answer.played_on,
            played(&[("speakers", 0)]),
            "where it plays still counts"
        );
    }

    #[test]
    fn a_muted_or_silent_endpoint_isnt_heard() {
        let mut muted = endpoint("speakers", &[PLAYER]);
        muted.volume = Some(EndpointVolume {
            muted: true,
            level: 1.0,
        });
        let mut silent = endpoint("headphones", &[PLAYER]);
        silent.volume = Some(EndpointVolume {
            muted: false,
            level: 0.0,
        });
        let endpoints = [muted, silent, endpoint("tv", &[OWN])];
        let answer = check(&player(), &endpoints, &ids(&["tv"]), &[]);
        assert!(answer.paths.is_empty(), "{answer:?}");
    }

    #[test]
    fn an_endpoint_whose_volume_cant_be_read_counts_as_audible() {
        let mut unknown = endpoint("speakers", &[PLAYER]);
        unknown.volume = None;
        let answer = check(&player(), &[unknown], &ids(&["tv"]), &[]);
        assert_eq!(heard(&answer), [("speakers", DirectReason::Session)]);
    }

    #[test]
    fn an_output_audiosync_plays_to_counts_too() {
        let endpoints = [endpoint("speakers", &[PLAYER, OWN])];
        let answer = check(&player(), &endpoints, &ids(&["speakers"]), &[]);
        assert_eq!(heard(&answer), [("speakers", DirectReason::Session)]);
        assert!(answer.paths[0].audiosync_plays_here);
    }

    #[test]
    fn a_virtual_device_is_heard_only_while_audiosync_plays_to_it() {
        let mut cable = endpoint("cable", &[PLAYER]);
        cable.virtual_device = true;
        let endpoints = [cable, endpoint("tv", &[OWN])];
        let answer = check(&player(), &endpoints, &ids(&["tv"]), &[]);
        assert!(answer.paths.is_empty(), "{answer:?}");
        assert_eq!(answer.played_on, played(&[("cable", 0)]));
        let answer = check(&player(), &endpoints, &ids(&["cable", "tv"]), &[]);
        assert_eq!(heard(&answer), [("cable", DirectReason::Session)]);
    }

    #[test]
    fn a_paused_app_still_counts_where_audiosync_doesnt_play() {
        let endpoints = [endpoint("speakers", &[OWN]), endpoint("tv", &[])];
        let last = played(&[("speakers", 0), ("tv", 0)]);
        let answer = check(&player(), &endpoints, &ids(&["speakers"]), &last);
        assert_eq!(
            heard(&answer),
            [
                ("speakers", DirectReason::Session),
                ("tv", DirectReason::Session)
            ]
        );
        // The TV counts for as long as the app stays silent; the speakers,
        // which AudioSync plays to, only for a few checks.
        let answer = checks(
            HOLD_CHECKS * 3,
            &player(),
            &endpoints,
            &ids(&["speakers"]),
            &last,
        );
        assert_eq!(heard(&answer), [("tv", DirectReason::Session)]);
        assert_eq!(answer.played_on, played(&[("tv", 0)]));
    }

    #[test]
    fn an_app_moved_to_another_output_drops_the_old_one_after_a_few_checks() {
        let endpoints = [
            endpoint("speakers", &[]),
            endpoint("cable", &[PLAYER]),
            endpoint("tv", &[OWN]),
        ];
        let last = played(&[("speakers", 0)]);
        let answer = checks(HOLD_CHECKS, &player(), &endpoints, &ids(&["tv"]), &last);
        assert_eq!(
            heard(&answer),
            [
                ("cable", DirectReason::Session),
                ("speakers", DirectReason::Session)
            ]
        );
        assert_eq!(
            answer.played_on,
            played(&[("cable", 0), ("speakers", HOLD_CHECKS)])
        );
        let answer = check(&player(), &endpoints, &ids(&["tv"]), &answer.played_on);
        assert_eq!(heard(&answer), [("cable", DirectReason::Session)]);
        assert_eq!(answer.played_on, played(&[("cable", 0)]));
    }

    #[test]
    fn another_app_pausing_now_and_then_doesnt_flip_the_whole_system_answer() {
        // The player plays on the TV all along, another app on the
        // headphones now and then.
        let playing = [
            default_endpoint("tv", &[OWN, PLAYER]),
            endpoint("headphones", &[OTHER]),
        ];
        let quiet = [
            default_endpoint("tv", &[OWN, PLAYER]),
            endpoint("headphones", &[]),
        ];
        let rendered = ids(&["tv"]);
        let both = [
            ("headphones", DirectReason::Session),
            ("tv", DirectReason::DefaultEndpoint),
        ];
        let mut answer = check(&whole_system(), &playing, &rendered, &[]);
        assert_eq!(heard(&answer), both);
        for _ in 0..3 {
            answer = checks(
                HOLD_CHECKS,
                &whole_system(),
                &quiet,
                &rendered,
                &answer.played_on,
            );
            assert_eq!(heard(&answer), both, "held while it is quiet");
            answer = check(&whole_system(), &playing, &rendered, &answer.played_on);
            assert_eq!(heard(&answer), both);
        }
        // Quiet for longer, it drops out.
        let answer = checks(
            HOLD_CHECKS + 1,
            &whole_system(),
            &quiet,
            &rendered,
            &answer.played_on,
        );
        assert_eq!(heard(&answer), [("tv", DirectReason::DefaultEndpoint)]);
        assert_eq!(answer.played_on, played(&[("tv", 0)]));
    }

    #[test]
    fn an_endpoint_that_went_away_drops_out_after_a_few_checks() {
        let endpoints = [endpoint("tv", &[OWN])];
        let last = played(&[("buds", 0)]);
        let answer = checks(HOLD_CHECKS + 1, &player(), &endpoints, &ids(&["tv"]), &last);
        assert!(answer.played_on.is_empty(), "{answer:?}");
    }

    #[test]
    fn the_whole_system_is_heard_from_the_default_output() {
        let endpoints = [
            default_endpoint("speakers", &[]),
            endpoint("tv", &[OWN]),
            endpoint("headphones", &[]),
        ];
        let answer = check(&whole_system(), &endpoints, &ids(&["tv"]), &[]);
        assert_eq!(
            heard(&answer),
            [("speakers", DirectReason::DefaultEndpoint)]
        );
    }

    #[test]
    fn the_whole_system_is_heard_where_another_app_plays_but_not_audiosync() {
        let endpoints = [
            default_endpoint("tv", &[OWN]),
            endpoint("headphones", &[OTHER]),
            endpoint("speakers", &[OWN]),
        ];
        let mut tv_muted = endpoints.clone();
        tv_muted[0].volume = Some(EndpointVolume {
            muted: true,
            level: 1.0,
        });
        let answer = check(&whole_system(), &tv_muted, &ids(&["speakers", "tv"]), &[]);
        assert_eq!(heard(&answer), [("headphones", DirectReason::Session)]);
        let answer = check(&whole_system(), &endpoints, &ids(&["speakers", "tv"]), &[]);
        assert_eq!(
            heard(&answer),
            [
                ("headphones", DirectReason::Session),
                ("tv", DirectReason::DefaultEndpoint),
            ],
            "sorted by endpoint ID"
        );
        assert!(answer.paths[1].audiosync_plays_here);
    }

    #[test]
    fn an_app_on_the_default_output_is_heard_for_its_session_only() {
        let endpoints = [default_endpoint("speakers", &[]), endpoint("tv", &[OWN])];
        let answer = check(&player(), &endpoints, &ids(&["tv"]), &[]);
        assert!(answer.paths.is_empty(), "{answer:?}");
    }
}
