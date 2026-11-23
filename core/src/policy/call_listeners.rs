//! Apps listening to a microphone while the whole system streams (S485): a
//! call app is left out of the stream, so the call plays at once on the
//! user's own output instead of on the network speakers a fraction of a
//! second late, and rejoins once it has stopped listening for [`HOLD`].
//!
//! The host reports facts on each poll and acts on what comes back: Windows
//! pins the app's output to the recorded original device (S486), Linux
//! targets its playback at the recorded sink (S487), the Mac adds it to the
//! tap's excluded processes.

use std::collections::BTreeMap;

use super::Seconds;

/// How long an app must have stopped listening before it rejoins: a call
/// app reopening its microphone on a device change or a mute toggle stays
/// out, a finished call rejoins within a few seconds.
pub const HOLD: Seconds = 3.0;

/// How long after the act the host reads whether the app still plays on the
/// silent endpoint (Windows): an app bound to that endpoint never moves.
pub const STAYED_AFTER: Seconds = 0.25;

/// What the host saw on one poll. Apps are keyed as the host knows them
/// (Windows: the executable path; Linux and Mac: the process).
#[derive(Clone, Copy, Debug)]
pub struct Facts<'a> {
    /// The whole-system source streams to at least one output.
    pub whole_system: bool,
    /// Apps with an active microphone stream, AudioSync left out.
    pub listening: &'a [String],
    /// Apps with an active stream on the silent endpoint; empty where no
    /// app can stay bound to it (Linux, Mac).
    pub on_silent: &'a [String],
}

/// What the host does for one app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenerAction {
    /// Take the app out of the stream: its sound plays on the user's output.
    LeaveOut(String),
    /// Undo [`ListenerAction::LeaveOut`]: the app is streamed again.
    Rejoin(String),
    /// The app still plays on the silent endpoint after the act, so it stays
    /// in the stream; the host logs this once and acts no further.
    Stayed(String),
}

#[derive(Clone, Debug, PartialEq)]
struct Out {
    acted_at: Seconds,
    heard_at: Seconds,
    checked: bool,
}

/// The apps left out, across a stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallListeners {
    out: BTreeMap<String, Out>,
}

impl CallListeners {
    /// The apps the host has been told to leave out and not yet to rejoin.
    pub fn left_out(&self) -> impl Iterator<Item = &str> {
        self.out.keys().map(String::as_str)
    }

    /// The host acted for `app` at `now`, later than the poll that left it
    /// out (Windows pins it once it plays): the stayed check counts from
    /// here.
    pub fn acted(&mut self, app: &str, now: Seconds) {
        let _ = (app, now);
    }

    /// One poll at `now`.
    pub fn update(&mut self, facts: Facts<'_>, now: Seconds) -> Vec<ListenerAction> {
        let mut actions = Vec::new();
        if !facts.whole_system {
            let out = std::mem::take(&mut self.out);
            actions.extend(out.into_keys().map(ListenerAction::Rejoin));
            return actions;
        }
        for app in facts.listening {
            match self.out.get_mut(app) {
                Some(out) => out.heard_at = now,
                None => {
                    self.out.insert(
                        app.clone(),
                        Out {
                            acted_at: now,
                            heard_at: now,
                            checked: false,
                        },
                    );
                    actions.push(ListenerAction::LeaveOut(app.clone()));
                }
            }
        }
        self.out.retain(|app, out| {
            if now - out.heard_at >= HOLD {
                actions.push(ListenerAction::Rejoin(app.clone()));
                return false;
            }
            if !out.checked && now - out.acted_at >= STAYED_AFTER {
                out.checked = true;
                if facts.on_silent.contains(app) {
                    actions.push(ListenerAction::Stayed(app.clone()));
                }
            }
            true
        });
        actions
    }
}
