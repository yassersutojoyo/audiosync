//! Track info after every start of the pipe (S168).
//!
//! Each start of the pipe, by the host's request (S99) or by the engine itself,
//! is a new queue item, and receivers show the pipe's file name until the track
//! info goes into the metadata pipe (S82) again. While it sends track info, the
//! host asks the engine's player status every [`TRACK_INFO_INTERVAL`]; this
//! watcher decides when it asks and when a status means a new start, so that
//! the host sends the current track info again from scratch. The host runs the
//! timer, sends the question, decodes the answer and writes the metadata pipe.

use super::Seconds;
use super::engine::PlayerStatus;

/// While sending track info, the host asks `GET`
/// [`PLAYER_REQUEST`](super::engine::PLAYER_REQUEST) this often, with
/// [`PLAYER_STATUS_TIMEOUT`](super::engine::PLAYER_STATUS_TIMEOUT) (S168).
pub const TRACK_INFO_INTERVAL: Seconds = 2.5;

/// Whether the host sends track info: while a session runs with at least one
/// AirPlay speaker enabled and this build bundles the engine (S168). The host
/// checks it at every model update and passes it to [`TrackInfoWatch::update`].
pub fn track_info_sending(
    session_running: bool,
    airplay_enabled: bool,
    engine_bundled: bool,
) -> bool {
    session_running && airplay_enabled && engine_bundled
}

/// A queue item of one engine run (S168). The engine's API documents queue
/// item ids without promising anything across runs, so an item of another run
/// (S102) is another item whatever its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueItem {
    /// The engine run the question went to ([`EngineLifecycle::run`](super::engine::EngineLifecycle::run)).
    pub run: u64,
    /// The `item_id` the player named.
    pub item_id: u64,
}

/// The track info watcher (S168).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackInfoWatch {
    sending: bool,
    /// The unanswered question: the engine run it went to.
    asking: Option<u64>,
    sent_for: Option<QueueItem>,
}

impl TrackInfoWatch {
    /// Whether track info is being sent.
    pub fn sending(&self) -> bool {
        self.sending
    }

    /// The item the track info was last sent for, if any was seen since
    /// sending began.
    pub fn sent_for(&self) -> Option<QueueItem> {
        self.sent_for
    }

    /// A model update: `sending` is [`track_info_sending`] now. When sending
    /// ends, the questions stop and the item is forgotten; the host forgets
    /// what it sent, so the next start of sending sends everything again.
    pub fn update(&mut self, sending: bool) {
        if !sending {
            self.sent_for = None;
        }
        self.sending = sending;
    }

    /// The timer fired: whether the host asks `GET /api/player` now, of engine
    /// run `run`. Only while sending, and never while the last question is
    /// unanswered: that turn is skipped.
    pub fn ask(&mut self, run: u64) -> bool {
        if !self.sending || self.asking.is_some() {
            return false;
        }
        self.asking = Some(run);
        true
    }

    /// The answer to the last question: its player status, or `None` for no
    /// answer (none within the timeout, or one that isn't a player status,
    /// S99). Returns true when the player holds a queue item other than the
    /// one the track info was last sent for, or any item when none has been
    /// seen since sending began: the host forgets what it sent and sends the
    /// current track info again from scratch, and the watcher remembers that
    /// item with the run it asked. A stopped player, the same item of the same
    /// run or no answer changes nothing; so does an answer while not sending.
    /// An answer to a question asked before sending last started counts like
    /// any other: it is the player's status now, and what was sent before is
    /// already forgotten.
    pub fn answered(&mut self, status: Option<PlayerStatus>) -> bool {
        let Some(run) = self.asking.take() else {
            return false;
        };
        if !self.sending {
            return false;
        }
        let Some(item_id) = status.and_then(|status| status.item) else {
            return false;
        };
        let item = QueueItem { run, item_id };
        if self.sent_for == Some(item) {
            return false;
        }
        self.sent_for = Some(item);
        true
    }
}
