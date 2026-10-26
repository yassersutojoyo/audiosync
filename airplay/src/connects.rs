//! The order and times at which new sessions open their connections
//! (P222). Starts wait in one queue and connect at least
//! [`CONNECT_SPACING`] apart, none more than [`MAX_CONNECT_WAIT`] after it
//! fell due; a waiting host start always gets the next turn ahead of a
//! retry. Sans-I/O: the sender's driver pushes each start as it falls due,
//! takes the turns that have come and reports the connects that began, so
//! the schedule is a pure function of the starts and the last connect.

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
    /// A session that goes on over a connection already open (P153): it
    /// takes no turn.
    OpenConnection,
}

#[derive(Debug)]
struct Waiting<K> {
    key: K,
    kind: StartKind,
    due: Instant,
    /// Breaks ties between starts that fell due at the same instant.
    order: u64,
}

impl<K> Waiting<K> {
    fn rank(&self) -> (Instant, u64) {
        (self.due, self.order)
    }
}

/// The starts waiting for their connect.
#[derive(Debug)]
pub struct ConnectQueue<K> {
    waiting: Vec<Waiting<K>>,
    last_connect: Option<Instant>,
    pushed: u64,
}

impl<K: PartialEq> Default for ConnectQueue<K> {
    fn default() -> ConnectQueue<K> {
        ConnectQueue {
            waiting: Vec::new(),
            last_connect: None,
            pushed: 0,
        }
    }
}

impl<K: PartialEq> ConnectQueue<K> {
    pub fn new() -> ConnectQueue<K> {
        ConnectQueue::default()
    }

    /// A start of `key` fell due at `due`. A key already waiting keeps its
    /// place; a host start for it makes it a host start.
    pub fn push(&mut self, key: K, kind: StartKind, due: Instant) {
        if let Some(waiting) = self.waiting.iter_mut().find(|waiting| waiting.key == key) {
            if kind != StartKind::Retry {
                waiting.kind = kind;
            }
            return;
        }
        self.pushed += 1;
        self.waiting.push(Waiting {
            key,
            kind,
            due,
            order: self.pushed,
        });
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

    /// Drops every waiting start, returning their keys in the order they
    /// fell due.
    pub fn clear(&mut self) -> Vec<K> {
        self.waiting.sort_by_key(Waiting::rank);
        self.waiting.drain(..).map(|waiting| waiting.key).collect()
    }

    /// When the last connect started.
    pub fn last_connect(&self) -> Option<Instant> {
        self.last_connect
    }

    /// When the spacing lets the next connect start.
    fn next_turn(&self) -> Option<Instant> {
        self.last_connect.map(|last| last + CONNECT_SPACING)
    }

    /// The next time a waiting start may connect.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let turn = self.next_turn();
        self.waiting
            .iter()
            .map(|waiting| match waiting.kind {
                StartKind::OpenConnection => waiting.due,
                StartKind::Host | StartKind::Retry => {
                    let spaced = turn.map_or(waiting.due, |turn| turn.max(waiting.due));
                    spaced.min(waiting.due + MAX_CONNECT_WAIT)
                }
            })
            .min()
    }

    /// The start whose turn has come at `now`, taken off the queue. The
    /// caller reports through [`ConnectQueue::connected`] when it connects;
    /// one that doesn't (refused before connecting) leaves the turn open.
    pub fn pop(&mut self, now: Instant) -> Option<(K, StartKind)> {
        let due = |waiting: &&Waiting<K>| waiting.due <= now;
        let open = self
            .waiting
            .iter()
            .filter(due)
            .filter(|waiting| waiting.kind == StartKind::OpenConnection)
            .min_by_key(|waiting| waiting.rank());
        let overdue = || {
            self.waiting
                .iter()
                .filter(|waiting| waiting.due + MAX_CONNECT_WAIT <= now)
                .min_by_key(|waiting| waiting.rank())
        };
        let in_turn = || {
            if self.next_turn().is_some_and(|turn| now < turn) {
                return None;
            }
            let first_of = |kind: StartKind| {
                self.waiting
                    .iter()
                    .filter(due)
                    .filter(|waiting| waiting.kind == kind)
                    .min_by_key(|waiting| waiting.rank())
            };
            first_of(StartKind::Host).or_else(|| first_of(StartKind::Retry))
        };
        let order = open.or_else(overdue).or_else(in_turn)?.order;
        let index = self
            .waiting
            .iter()
            .position(|waiting| waiting.order == order)?;
        let waiting = self.waiting.remove(index);
        Some((waiting.key, waiting.kind))
    }

    /// A connect started at `at`: the next turn comes [`CONNECT_SPACING`]
    /// later.
    pub fn connected(&mut self, at: Instant) {
        self.last_connect = Some(self.last_connect.map_or(at, |last| last.max(at)));
    }
}
