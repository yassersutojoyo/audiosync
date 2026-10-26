//! The DACP command server's sockets and threads (P189, P190, P194): one
//! listener on each local address the running sessions use, all on one
//! port, a thread per connection, and the table of running sessions the
//! connection threads answer from without asking the driver, so every
//! answer goes out within 100 ms whatever the driver is doing.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, SocketAddrV6, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::dacp::{
    self, ACCEPT_BACKOFF, ActiveRemotes, AdvertiseError, DacpAdvertiser, DacpId, DacpRecord,
    HEAD_TIMEOUT, HeadRead, IDLE_TIMEOUT, LINGER, MAX_CONNECTIONS, NameConflict, RateLimit,
    RemoteCommand, SessionCheck,
};
use crate::discovery::DeviceKey;
use crate::ids::{Entropy, EntropyUnavailable, SessionIds};
use crate::targets::TargetId;

/// How often an accept thread looks at its stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(10);
/// The longest an answer's write may take (P189).
const WRITE_TIMEOUT: Duration = Duration::from_millis(100);
/// How often the driver rolls the rate limits over (P195).
const ROLL_EVERY: Duration = Duration::from_millis(250);
/// 403s a peer may get within `STRIKE_WINDOW` of its first one before its
/// connections are closed unread for the rest of the window (P191): room
/// for a receiver retrying a stale value, about 42 years on average to
/// guess a 32-bit Active-Remote.
const STRIKE_LIMIT: u32 = 8;
const STRIKE_WINDOW: Duration = Duration::from_secs(5);
/// Peers tracked at once; the oldest window gives way to a new one.
const STRIKE_PEERS: usize = 64;

/// The command server's counters (P144, P195), read without locks.
#[derive(Debug, Default)]
pub struct DacpCounters {
    accepted: AtomicU64,
    forbidden: AtomicU64,
    bad: AtomicU64,
    over_limit: AtomicU64,
    port: AtomicU16,
}

impl DacpCounters {
    /// Requests answered 204 within the rate limit.
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// Requests answered 403.
    pub fn forbidden(&self) -> u64 {
        self.forbidden.load(Ordering::Relaxed)
    }

    /// Requests answered 400 or 404.
    pub fn bad(&self) -> u64 {
        self.bad.load(Ordering::Relaxed)
    }

    /// Requests over the rate limit.
    pub fn over_limit(&self) -> u64 {
        self.over_limit.load(Ordering::Relaxed)
    }

    /// The server's port while it runs, 0 otherwise.
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    fn count(&self, status: u16) {
        let counter = match status {
            204 => &self.accepted,
            403 => &self.forbidden,
            400 | 404 => &self.bad,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// What the connection threads hand the driver.
#[derive(Debug)]
pub(crate) enum RemoteInput {
    /// An answered command for the session with this Active-Remote; `who`
    /// and `what` name it in the log line of a drop.
    Command {
        remote: u32,
        command: RemoteCommand,
        who: String,
        what: String,
    },
    /// A line for the host's debug log (P195).
    Log(String),
}

/// A running session as the connection threads see it.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) key: DeviceKey,
    pub(crate) name: String,
    /// The addresses discovery reported for the speaker, and the one its
    /// RTSP connection reached.
    pub(crate) peers: Vec<IpAddr>,
    pub(crate) target: Option<TargetId>,
}

impl Entry {
    fn has(&self, peer: IpAddr) -> bool {
        self.peers.contains(&peer.to_canonical())
    }
}

#[derive(Default)]
struct Table {
    sessions: HashMap<u32, Entry>,
    /// Each speaker's rate limit, with its name for the summary line.
    rates: HashMap<DeviceKey, (String, RateLimit)>,
}

impl Table {
    /// A connection from `peer` may be served: some running session's
    /// speaker, or a member of its target, has that address (P190).
    fn admits(&self, peer: IpAddr) -> bool {
        self.sessions.values().any(|entry| entry.has(peer))
    }

    /// The session `remote` names has `peer`, itself or through another
    /// member of its target (P191).
    fn check(&self, remote: u32, peer: IpAddr) -> SessionCheck {
        let Some(entry) = self.sessions.get(&remote) else {
            return SessionCheck::Mismatch;
        };
        let member = |other: &Entry| other.target.is_some() && other.target == entry.target;
        if entry.has(peer)
            || self
                .sessions
                .values()
                .any(|other| member(other) && other.has(peer))
        {
            SessionCheck::Match
        } else {
            SessionCheck::Mismatch
        }
    }
}

/// Each peer's 403s in its current window (P191).
#[derive(Default)]
struct Strikes(HashMap<IpAddr, (Instant, u32)>);

impl Strikes {
    fn blocked(&self, peer: IpAddr, now: Instant) -> bool {
        self.0
            .get(&peer)
            .is_some_and(|(since, count)| *count >= STRIKE_LIMIT && now < *since + STRIKE_WINDOW)
    }

    /// Counts a 403 for `peer`; true when this one reaches the limit.
    fn strike(&mut self, peer: IpAddr, now: Instant) -> bool {
        self.0.retain(|_, (since, _)| now < *since + STRIKE_WINDOW);
        if self.0.len() >= STRIKE_PEERS
            && !self.0.contains_key(&peer)
            && let Some(oldest) = self
                .0
                .iter()
                .min_by_key(|(_, (since, _))| *since)
                .map(|(ip, _)| *ip)
        {
            self.0.remove(&oldest);
        }
        let (_, count) = self.0.entry(peer).or_insert((now, 0));
        *count += 1;
        *count == STRIKE_LIMIT
    }
}

type Sink = Arc<dyn Fn(RemoteInput) -> bool + Send + Sync>;

struct Shared {
    table: Mutex<Table>,
    /// Held from an answer's write to its event's hand-over, so events
    /// reach the driver in the order the requests were answered (P193).
    deliver: Mutex<()>,
    strikes: Mutex<Strikes>,
    open: AtomicUsize,
    counters: Arc<DacpCounters>,
    sink: Sink,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn log(&self, line: String) {
        (self.sink)(RemoteInput::Log(line));
    }
}

struct Listener {
    local: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The Active-Remote values of the sessions on this address.
    users: BTreeSet<u32>,
}

impl Listener {
    fn close(mut self) -> Option<JoinHandle<()>> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take()
    }
}

/// What attaching a session's connection to the server ran into.
#[derive(Debug, Default)]
pub(crate) struct Attached {
    /// The port couldn't be bound on the session's local address (P190).
    pub(crate) unavailable: bool,
    /// The registration failed (P188).
    pub(crate) not_advertised: Option<AdvertiseError>,
}

/// The command server, owned by the driver.
pub(crate) struct Server {
    shared: Arc<Shared>,
    id: Option<DacpId>,
    remotes: ActiveRemotes,
    /// The port of this run, kept across stops while it is free (P189).
    port: u16,
    running: bool,
    listeners: Vec<Listener>,
    stop_at: Option<Instant>,
    advertiser: Option<Box<dyn DacpAdvertiser + Send>>,
    advertised: bool,
    /// The Active-Remote values of the sessions in the table.
    live: BTreeSet<u32>,
    next_roll: Option<Instant>,
}

impl Server {
    pub(crate) fn new(sink: Sink) -> Server {
        Server {
            shared: Arc::new(Shared {
                table: Mutex::new(Table::default()),
                deliver: Mutex::new(()),
                strikes: Mutex::new(Strikes::default()),
                open: AtomicUsize::new(0),
                counters: Arc::new(DacpCounters::default()),
                sink,
            }),
            id: None,
            remotes: ActiveRemotes::new(),
            port: 0,
            running: false,
            listeners: Vec::new(),
            stop_at: None,
            advertiser: None,
            advertised: false,
            live: BTreeSet::new(),
            next_roll: None,
        }
    }

    pub(crate) fn counters(&self) -> Arc<DacpCounters> {
        self.shared.counters.clone()
    }

    /// Puts the run's DACP ID and a fresh Active-Remote into `ids` (P187).
    pub(crate) fn fill_ids(
        &mut self,
        ids: &mut SessionIds,
        entropy: &mut impl Entropy,
    ) -> Result<(), EntropyUnavailable> {
        let id = match self.id {
            Some(id) => id,
            None => {
                let id = DacpId::generate(entropy)?;
                self.id = Some(id);
                id
            }
        };
        ids.client_instance = id.value();
        ids.active_remote = self.remotes.issue(entropy)?;
        Ok(())
    }

    /// A session starts: the server runs from now on (P189), and commands
    /// with its Active-Remote are answered.
    pub(crate) fn open(&mut self, remote: u32, entry: Entry, now: Instant) {
        self.running = true;
        self.stop_at = None;
        self.next_roll.get_or_insert(now + ROLL_EVERY);
        self.live.insert(remote);
        lock(&self.shared.table).sessions.insert(remote, entry);
    }

    /// The session's RTSP connection runs from `local` to `peer`: the
    /// server listens on `local`'s address, registering its record when
    /// this is its first listener (P188, P190).
    pub(crate) fn connected(&mut self, remote: u32, local: SocketAddr, peer: IpAddr) -> Attached {
        let mut attached = Attached::default();
        if !self.live.contains(&remote) {
            return attached;
        }
        if let Some(entry) = lock(&self.shared.table).sessions.get_mut(&remote) {
            let peer = peer.to_canonical();
            if !entry.peers.contains(&peer) {
                entry.peers.push(peer);
            }
        }
        let ip = local.ip();
        let scope = match local {
            SocketAddr::V6(address) => address.scope_id(),
            SocketAddr::V4(_) => 0,
        };
        if let Some(listener) = self
            .listeners
            .iter_mut()
            .find(|listener| listener.local.ip() == ip)
        {
            listener.users.insert(remote);
            return attached;
        }
        let first = self.listeners.is_empty();
        let bound = match (first, self.port) {
            (true, 0) => bind(ip, scope, 0),
            (true, port) => bind(ip, scope, port).or_else(|_| bind(ip, scope, 0)),
            (false, port) => bind(ip, scope, port),
        };
        let listener = match bound {
            Ok(listener) => listener,
            Err(_) => {
                attached.unavailable = true;
                return attached;
            }
        };
        let Ok(address) = listener.local_addr() else {
            attached.unavailable = true;
            return attached;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = spawn_accept(listener, self.shared.clone(), stop.clone());
        let Ok(thread) = thread else {
            attached.unavailable = true;
            return attached;
        };
        let changed = self.port != address.port();
        self.port = address.port();
        self.shared
            .counters
            .port
            .store(self.port, Ordering::Relaxed);
        self.listeners.push(Listener {
            local: address,
            stop,
            thread: Some(thread),
            users: BTreeSet::from([remote]),
        });
        if !self.advertised || changed {
            attached.not_advertised = self.advertise().err();
        }
        attached
    }

    fn advertise(&mut self) -> Result<(), AdvertiseError> {
        let (Some(id), Some(advertiser)) = (self.id, self.advertiser.as_mut()) else {
            return Ok(());
        };
        if self.port == 0 || !self.running {
            return Ok(());
        }
        let result = advertiser.register(&DacpRecord::new(id, self.port));
        self.advertised = result.is_ok();
        result
    }

    /// Sets the advertiser; a running server registers with it at once.
    pub(crate) fn set_advertiser(
        &mut self,
        advertiser: Box<dyn DacpAdvertiser + Send>,
    ) -> Result<(), AdvertiseError> {
        if let Some(mut old) = self.advertiser.take()
            && self.advertised
        {
            old.withdraw();
        }
        self.advertised = false;
        self.advertiser = Some(advertiser);
        if self.listeners.is_empty() {
            return Ok(());
        }
        self.advertise()
    }

    /// The Active-Remote values in the table, so the driver sees whether
    /// its running sessions changed.
    pub(crate) fn live(&self) -> &BTreeSet<u32> {
        &self.live
    }

    /// Only the sessions in `running` are left in the table: a listener no
    /// session uses closes, unless every listener is unused, and the server
    /// stops [`LINGER`] after the last session ended (P189, P190).
    pub(crate) fn retain(&mut self, running: &BTreeSet<u32>, now: Instant) -> Vec<JoinHandle<()>> {
        let gone: Vec<u32> = self.live.difference(running).copied().collect();
        if !gone.is_empty() {
            let mut table = lock(&self.shared.table);
            for remote in &gone {
                table.sessions.remove(remote);
            }
        }
        for remote in &gone {
            self.live.remove(remote);
            for listener in &mut self.listeners {
                listener.users.remove(remote);
            }
        }
        let mut closed = Vec::new();
        if self
            .listeners
            .iter()
            .any(|listener| !listener.users.is_empty())
        {
            let (keep, unused): (Vec<Listener>, Vec<Listener>) = self
                .listeners
                .drain(..)
                .partition(|listener| !listener.users.is_empty());
            self.listeners = keep;
            closed.extend(unused.into_iter().filter_map(Listener::close));
        }
        if self.running && self.live.is_empty() && self.stop_at.is_none() {
            self.stop_at = Some(now + LINGER);
        }
        closed
    }

    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        match (self.stop_at, self.next_roll) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Stops the server once it lingered long enough, and logs the
    /// requests over the rate limit of each second that ended (P195).
    pub(crate) fn handle_timeout(&mut self, now: Instant) -> Vec<JoinHandle<()>> {
        if self.next_roll.is_some_and(|at| at <= now) {
            self.next_roll = self.running.then_some(now + ROLL_EVERY);
            self.roll(now);
            if let Some(advertiser) = self.advertiser.as_mut() {
                for conflict in advertiser.conflicts() {
                    self.shared.log(name_conflict_line(&conflict));
                }
            }
        }
        match self.stop_at {
            Some(at) if at <= now && self.live.is_empty() => self.stop(),
            _ => Vec::new(),
        }
    }

    fn roll(&self, now: Instant) {
        let mut lines = Vec::new();
        {
            let mut table = lock(&self.shared.table);
            for (name, rate) in table.rates.values_mut() {
                if let Some(over) = rate.roll(now) {
                    lines.push(over_limit_line(name, over));
                }
            }
        }
        for line in lines {
            self.shared.log(line);
        }
    }

    /// Closes every listener and withdraws the record; the port is kept
    /// for a restart in this run (P188, P189).
    pub(crate) fn stop(&mut self) -> Vec<JoinHandle<()>> {
        self.running = false;
        self.stop_at = None;
        self.next_roll = None;
        if self.advertised
            && let Some(advertiser) = self.advertiser.as_mut()
        {
            advertiser.withdraw();
        }
        self.advertised = false;
        self.shared.counters.port.store(0, Ordering::Relaxed);
        self.listeners
            .drain(..)
            .filter_map(Listener::close)
            .collect()
    }
}

fn over_limit_line(name: &str, over: u32) -> String {
    format!("DACP {name}: {over} requests over the limit -> 204, no event")
}

/// Leaves both names out: they carry the DACP ID (P195).
fn name_conflict_line(conflict: &NameConflict) -> String {
    let which = if conflict.host {
        "host name"
    } else {
        "service name"
    };
    format!(
        "DACP: another device on {} claimed the remote-control {which}; it is announced with a suffix, so speakers may not find their remote buttons",
        conflict.interface
    )
}

fn bind(ip: IpAddr, scope: u32, port: u16) -> io::Result<TcpListener> {
    let address = match ip {
        IpAddr::V4(_) => SocketAddr::new(ip, port),
        IpAddr::V6(v6) => SocketAddr::V6(SocketAddrV6::new(v6, port, 0, scope)),
    };
    let listener = TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn spawn_accept(
    listener: TcpListener,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("airplay-dacp-accept".to_owned())
        .spawn(move || {
            // A bug here ends this listener only; the sessions play on.
            let _ =
                panic::catch_unwind(AssertUnwindSafe(|| accept_loop(&listener, &shared, &stop)));
        })
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let peer = peer.ip().to_canonical();
                if !lock(&shared.table).admits(peer)
                    || lock(&shared.strikes).blocked(peer, Instant::now())
                {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                if shared.open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    shared.open.fetch_sub(1, Ordering::SeqCst);
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let guard = OpenGuard(shared.clone());
                let spawned = thread::Builder::new()
                    .name("airplay-dacp-connection".to_owned())
                    .spawn(move || {
                        let shared = guard.0.clone();
                        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                            serve(stream, peer, &shared);
                        }));
                        drop(guard);
                    });
                if spawned.is_err() {
                    thread::sleep(ACCEPT_BACKOFF);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => thread::sleep(ACCEPT_BACKOFF),
        }
    }
}

/// Counts a connection open until its thread ends, however it ends.
struct OpenGuard(Arc<Shared>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How a read for a request ended.
enum Fill {
    More,
    /// The peer closed, the deadline passed or the socket failed.
    Done,
}

/// Reads what arrives by `deadline` into `buffer`.
fn fill(stream: &mut TcpStream, buffer: &mut Vec<u8>, deadline: Instant) -> Fill {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
        return Fill::Done;
    }
    let mut chunk = [0u8; 2048];
    match stream.read(&mut chunk) {
        Ok(0) => Fill::Done,
        Ok(read) => {
            buffer.extend_from_slice(&chunk[..read]);
            Fill::More
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Fill::More,
        Err(_) => Fill::Done,
    }
}

/// Serves one connection until it closes (P189, P194).
fn serve(mut stream: TcpStream, peer: IpAddr, shared: &Shared) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut buffer: Vec<u8> = Vec::new();
    // The head must be in within 2 s of the connection opening, or of its
    // first byte after an answer; with nothing sent, a connection closes
    // after 5 s idle.
    let mut started = Some(Instant::now());
    let mut answered = Instant::now();
    loop {
        if started.is_none() && !buffer.is_empty() {
            started = Some(Instant::now());
        }
        let (head, length) = match dacp::read_head(&buffer) {
            HeadRead::Partial => {
                let deadline = started.map_or(answered + IDLE_TIMEOUT, |at| at + HEAD_TIMEOUT);
                match fill(&mut stream, &mut buffer, deadline) {
                    Fill::More => continue,
                    Fill::Done => return,
                }
            }
            HeadRead::Complete { head, length } => (head, length),
            HeadRead::Malformed | HeadRead::TooLarge => {
                refuse_malformed(&mut stream, peer, shared);
                return;
            }
        };
        let deadline = started.unwrap_or(answered) + HEAD_TIMEOUT;
        while buffer.len() < length + head.content_length {
            if let Fill::Done = fill(&mut stream, &mut buffer, deadline) {
                return;
            }
        }
        buffer.drain(..length + head.content_length);
        if lock(&shared.strikes).blocked(peer, Instant::now()) {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        if handle(&mut stream, peer, &head, shared) {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        answered = Instant::now();
        started = None;
    }
}

fn refuse_malformed(stream: &mut TcpStream, peer: IpAddr, shared: &Shared) {
    shared.counters.count(400);
    let _ = stream.write_all(&dacp::response(400, true));
    let _ = stream.shutdown(Shutdown::Both);
    shared.log(format!("DACP {peer}: malformed request -> 400"));
}

/// Answers one request, then hands its event to the driver (P189, P193);
/// true when the connection closes after it (P191).
fn handle(stream: &mut TcpStream, peer: IpAddr, head: &dacp::RequestHead, shared: &Shared) -> bool {
    let now = Instant::now();
    let what = dacp::log_command(&head.target);
    let (answer, who, admitted, ended) = {
        let mut table = lock(&shared.table);
        let answer = dacp::answer(head, |remote| table.check(remote, peer));
        let named = head
            .header("Active-Remote")
            .and_then(dacp::parse_active_remote)
            .filter(|remote| table.check(*remote, peer) == SessionCheck::Match)
            .and_then(|remote| table.sessions.get(&remote))
            .map(|entry| (entry.name.clone(), entry.key.clone()));
        let who = named
            .as_ref()
            .map_or_else(|| peer.to_string(), |(name, _)| name.clone());
        let (admitted, ended) = match (&answer.command, named) {
            (Some(_), Some((name, key))) => {
                let (_, rate) = table
                    .rates
                    .entry(key)
                    .or_insert_with(|| (name, RateLimit::default()));
                rate.admit(now)
            }
            _ => (true, None),
        };
        (answer, who, admitted, ended)
    };
    let forbidden = answer.status == 403;
    let close = forbidden || head.wants_close();
    // Counted before the answer goes out, so the peer's next connection
    // already sees the cap.
    let capped = forbidden && lock(&shared.strikes).strike(peer, now);
    if !admitted {
        shared.counters.over_limit.fetch_add(1, Ordering::Relaxed);
    } else {
        shared.counters.count(answer.status);
    }
    {
        let _order = lock(&shared.deliver);
        let _ = stream.write_all(&dacp::response(answer.status, close));
        if admitted && let (Some(command), Some(remote)) = (answer.command, answer.remote) {
            (shared.sink)(RemoteInput::Command {
                remote,
                command,
                who: who.clone(),
                what: what.clone(),
            });
        }
    }
    if let Some(over) = ended {
        shared.log(over_limit_line(&who, over));
    }
    if answer.command.is_none() {
        let note = if answer.status == 204 {
            ", no event"
        } else {
            ""
        };
        shared.log(format!("DACP {who}: {what} -> {}{note}", answer.status));
    }
    if capped {
        shared.log(format!(
            "DACP {peer}: {STRIKE_LIMIT} requests refused with 403 -> connections closed for {} s",
            STRIKE_WINDOW.as_secs()
        ));
    }
    close
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn peer(n: usize) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n as u32))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // covers: P191
    #[test]
    fn strikes_block_a_peer_at_the_limit_for_the_rest_of_its_window() {
        let t0 = Instant::now();
        let mut strikes = Strikes::default();
        for n in 1..STRIKE_LIMIT {
            assert!(!strikes.strike(peer(1), t0 + ms(u64::from(n))));
            assert!(!strikes.blocked(peer(1), t0 + ms(u64::from(n))));
        }
        assert!(strikes.strike(peer(1), t0 + ms(100)));
        assert!(strikes.blocked(peer(1), t0 + ms(100)));
        assert!(!strikes.strike(peer(1), t0 + ms(200)), "reported once");
        assert!(!strikes.blocked(peer(2), t0 + ms(200)));
        let window_end = t0 + ms(1) + STRIKE_WINDOW;
        assert!(strikes.blocked(peer(1), window_end - ms(1)));
        assert!(!strikes.blocked(peer(1), window_end));
    }

    #[test]
    fn strikes_start_a_fresh_window_once_the_old_one_ends() {
        let t0 = Instant::now();
        let mut strikes = Strikes::default();
        for _ in 1..STRIKE_LIMIT {
            strikes.strike(peer(1), t0);
        }
        let later = t0 + STRIKE_WINDOW;
        assert!(!strikes.strike(peer(1), later));
        assert_eq!(strikes.0[&peer(1)], (later, 1));
    }

    #[test]
    fn strikes_prune_windows_that_ended() {
        let t0 = Instant::now();
        let mut strikes = Strikes::default();
        strikes.strike(peer(1), t0);
        strikes.strike(peer(2), t0 + ms(3000));
        strikes.strike(peer(3), t0 + STRIKE_WINDOW);
        assert!(!strikes.0.contains_key(&peer(1)));
        assert!(strikes.0.contains_key(&peer(2)));
        assert_eq!(strikes.0.len(), 2);
    }

    #[test]
    fn strikes_track_at_most_the_peer_cap() {
        let t0 = Instant::now();
        let mut strikes = Strikes::default();
        for n in 0..STRIKE_PEERS * 2 {
            strikes.strike(peer(n), t0 + ms(n as u64));
        }
        assert_eq!(strikes.0.len(), STRIKE_PEERS);
        strikes.strike(peer(STRIKE_PEERS * 2 - 1), t0 + ms(1000));
        assert_eq!(
            strikes.0.len(),
            STRIKE_PEERS,
            "a tracked peer evicts no one"
        );
    }

    #[test]
    fn strikes_evict_the_oldest_window_when_full() {
        let t0 = Instant::now();
        let mut strikes = Strikes::default();
        // Inserted out of order, so map order can't stand in for age.
        strikes.strike(peer(0), t0 + ms(10));
        strikes.strike(peer(1), t0);
        for n in 2..STRIKE_PEERS {
            strikes.strike(peer(n), t0 + ms(10 + n as u64));
        }
        strikes.strike(peer(1), t0 + ms(500));
        strikes.strike(peer(STRIKE_PEERS), t0 + ms(1000));
        assert!(
            !strikes.0.contains_key(&peer(1)),
            "oldest window start goes"
        );
        assert!(strikes.0.contains_key(&peer(0)));
        assert_eq!(strikes.0[&peer(STRIKE_PEERS)], (t0 + ms(1000), 1));
        strikes.strike(peer(STRIKE_PEERS + 1), t0 + ms(1001));
        assert!(!strikes.0.contains_key(&peer(0)));
    }
}
