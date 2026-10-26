//! The order and times at which new sessions open their connections
//! (P222). Not spaced yet: every start connects as soon as it falls due.

use std::time::{Duration, Instant};

/// The least time between two connect starts (P222).
pub const CONNECT_SPACING: Duration = Duration::from_millis(500);

/// The longest a connect waits after its start fell due (P222).
pub const MAX_CONNECT_WAIT: Duration = Duration::from_secs(10);

/// Who asked for a start (P222).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartKind {
    /// The host's own start, one handed on by P169 or P204, or a target's
    /// move to the AirPlay 2 path (P177).
    Host,
    /// A retry of P142 or P181 whose time has come.
    Retry,
    /// A session that goes on over a connection already open (P153).
    OpenConnection,
}

#[derive(Debug)]
struct Waiting<K> {
    key: K,
    kind: StartKind,
    due: Instant,
}

/// The starts waiting for their connect.
#[derive(Debug)]
pub struct ConnectQueue<K> {
    waiting: Vec<Waiting<K>>,
    last_connect: Option<Instant>,
}

impl<K: PartialEq> Default for ConnectQueue<K> {
    fn default() -> ConnectQueue<K> {
        ConnectQueue {
            waiting: Vec::new(),
            last_connect: None,
        }
    }
}

impl<K: PartialEq> ConnectQueue<K> {
    pub fn new() -> ConnectQueue<K> {
        ConnectQueue::default()
    }

    /// A start of `key` fell due at `due`.
    pub fn push(&mut self, key: K, kind: StartKind, due: Instant) {
        if !self.contains(&key) {
            self.waiting.push(Waiting { key, kind, due });
        }
    }

    /// Takes a waiting start off the queue; true when it waited.
    pub fn remove(&mut self, key: &K) -> bool {
        let before = self.waiting.len();
        self.waiting.retain(|waiting| waiting.key != *key);
        self.waiting.len() != before
    }

    pub fn contains(&self, key: &K) -> bool {
        self.waiting.iter().any(|waiting| waiting.key == *key)
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    /// Drops every waiting start, returning their keys in queue order.
    pub fn clear(&mut self) -> Vec<K> {
        self.waiting.drain(..).map(|waiting| waiting.key).collect()
    }

    /// When the last connect started.
    pub fn last_connect(&self) -> Option<Instant> {
        self.last_connect
    }

    /// The next time a waiting start may connect.
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.waiting.iter().map(|waiting| waiting.due).min()
    }

    /// The start whose turn has come at `now`, taken off the queue.
    pub fn pop(&mut self, now: Instant) -> Option<(K, StartKind)> {
        let index = self.waiting.iter().position(|waiting| waiting.due <= now)?;
        let waiting = self.waiting.remove(index);
        Some((waiting.key, waiting.kind))
    }

    /// A connect started at `at`.
    pub fn connected(&mut self, at: Instant) {
        self.last_connect = Some(at);
    }
}
