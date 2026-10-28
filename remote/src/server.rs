//! The server: its listeners, one thread per connection, the routes, and
//! the event stream that carries each new snapshot to the open pages.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;

use crate::auth::{
    self, DeviceFile, Limits, LoadProblem, PairOutcome, Pairing, Snapshot, TokenStore,
};
use crate::http::{self, Request, Response};
use crate::net;
use crate::page::{self, PageError};
use crate::state::{self, BodyError, Command, RangeError, State, StateError, ZoneProblem};
use crate::text::{self, ApiError};

/// The port the remote listens on unless the user picks another. Unassigned
/// in the IANA service name and port number registry (range 7649-7662).
pub const DEFAULT_PORT: u16 = 7655;
/// The cookie that carries the device token.
pub const TOKEN_COOKIE: &str = "as_token";
// 400 days, the longest a browser keeps a cookie; the server forgets the device sooner if asked.
const COOKIE_MAX_AGE: u64 = 400 * 24 * 60 * 60;
const ACCEPT_POLL: Duration = Duration::from_millis(20);
/// Connections served at once, event streams included.
const MAX_CONNECTIONS: usize = 32;
/// Connections one peer may hold, so one device can't take every slot.
const MAX_CONNECTIONS_PER_PEER: usize = 8;
/// Slots that connections not yet known to come from a paired device can't fill.
const RESERVED_FOR_PAIRED: usize = 8;
/// Reserved slots one peer may wait in at once.
const RESERVED_PER_PEER: usize = 2;
/// How long a connection in a reserved slot has to send its request; a
/// browser on the same network sends it at once.
const RESERVED_REQUEST: Duration = Duration::from_secs(1);
/// Event streams one paired device may hold; a new one ends the oldest.
const MAX_STREAMS_PER_DEVICE: usize = 2;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a stop waits for the connection threads.
const STOP_GRACE: Duration = Duration::from_millis(500);
/// How long a refused connection is drained before it is closed.
const REFUSED_LINGER: Duration = Duration::from_secs(1);
/// Refused connections drained at once; the oldest is closed for a new one.
const MAX_REFUSED: usize = 16;
/// Snapshots kept so a stream that fell a little behind still gets each one.
const HISTORY: usize = 32;
/// Server events kept for a host that doesn't poll.
pub const MAX_SERVER_EVENTS: usize = 64;

/// The time limits; the defaults are the product's, tests shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// How long a pairing code works.
    pub code_lifetime: Duration,
    /// The window in which one peer may send five wrong codes.
    pub wrong_code_window: Duration,
    /// The longest an event stream stays silent; a comment line fills it.
    pub heartbeat: Duration,
    /// How long a client has to send its whole request; a slower one is closed unanswered.
    pub request: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        let limits = Limits::default();
        Timing {
            code_lifetime: limits.code_lifetime,
            wrong_code_window: limits.window,
            heartbeat: Duration::from_secs(15),
            request: http::REQUEST_TIMEOUT,
        }
    }
}

/// What the server needs from the app.
#[derive(Clone, Debug)]
pub struct Config {
    /// 0 lets the system pick one (tests).
    pub port: u16,
    /// The addresses to listen on; one that can't be bound is skipped as
    /// long as another one is.
    pub listen: Vec<IpAddr>,
    /// The computer's name, so `<local_name>.local:<port>` is an accepted `Host`.
    pub local_name: Option<String>,
    /// The paired-device file; none keeps the devices in memory only.
    pub devices_path: Option<PathBuf>,
    pub timing: Timing,
    pub capacity: Capacity,
    /// Reports each new pairing code as [`ServerEvent::PairingCode`], for a
    /// debug log only: whoever reads that log can pair while the code works.
    pub log_pairing_codes: bool,
}

/// How many connections the server holds; the defaults are the product's,
/// tests shrink them. A peer is one address ([`net::peer_group`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// Connections at once.
    pub connections: usize,
    /// Connections one peer may hold.
    pub per_peer: usize,
    /// Of `connections`, the slots kept for requests that carry a paired
    /// device's token: a connection let in while the others hold the rest
    /// gets a short time to send its request and is answered 503 without one.
    pub reserved: usize,
    /// Reserved slots one peer may wait in at once.
    pub reserved_per_peer: usize,
}

impl Default for Capacity {
    fn default() -> Capacity {
        Capacity {
            connections: MAX_CONNECTIONS,
            per_peer: MAX_CONNECTIONS_PER_PEER,
            reserved: RESERVED_FOR_PAIRED,
            reserved_per_peer: RESERVED_PER_PEER,
        }
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            port: DEFAULT_PORT,
            // IPv6 first: where it is dual-stack the IPv4 bind then fails and is skipped.
            listen: vec![
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ],
            local_name: None,
            devices_path: None,
            timing: Timing::default(),
            capacity: Capacity::default(),
            log_pairing_codes: false,
        }
    }
}

impl Config {
    /// Reads `{"port"?, "local_name"?, "devices_path"?, "listen"?,
    /// "log_pairing_codes"?}`, the defaults filling in what is missing;
    /// `listen` is a list of IP literals. None when a field has the wrong
    /// type or value.
    pub fn from_json(text: &str) -> Option<Config> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        let mut config = Config::default();
        if let Some(port) = object.get("port") {
            config.port = u16::try_from(port.as_u64()?).ok()?;
        }
        if let Some(name) = object.get("local_name") {
            config.local_name = Some(name.as_str()?.to_owned());
        }
        if let Some(path) = object.get("devices_path") {
            config.devices_path =
                Some(PathBuf::from(path.as_str()?)).filter(|path| !path.as_os_str().is_empty());
        }
        if let Some(listen) = object.get("listen") {
            config.listen = listen
                .as_array()?
                .iter()
                .map(|address| address.as_str()?.parse().ok())
                .collect::<Option<_>>()?;
        }
        if let Some(log) = object.get("log_pairing_codes") {
            config.log_pairing_codes = log.as_bool()?;
        }
        Some(config)
    }
}

/// Why the server didn't start, or a code couldn't be made.
#[derive(Debug)]
pub enum RemoteError {
    /// No address could be listened on; [`RemoteError::port_in_use`] tells a busy port.
    Bind(io::Error),
    /// No randomness for a code or a token.
    Random,
    /// The built-in page is malformed (a build mistake the tests catch).
    Page(PageError),
}

impl RemoteError {
    pub fn port_in_use(&self) -> bool {
        matches!(self, RemoteError::Bind(error) if error.kind() == io::ErrorKind::AddrInUse)
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::Bind(error) => write!(f, "could not open the remote's port: {error}"),
            RemoteError::Random => f.write_str("no randomness for a pairing code or token"),
            RemoteError::Page(PageError(tag)) => {
                write!(f, "the remote page needs exactly one plain <{tag}>")
            }
        }
    }
}

impl std::error::Error for RemoteError {}

/// What the server reports for the debug log: who paired, failed or was
/// refused, never a token, and a pairing code only with
/// [`Config::log_pairing_codes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerEvent {
    Paired {
        from: IpAddr,
    },
    /// `why` is never [`PairOutcome::Paired`].
    PairFailed {
        from: IpAddr,
        why: PairOutcome,
    },
    Unpaired {
        from: IpAddr,
    },
    /// A request with a foreign `Host` or `Origin` (403).
    Refused {
        from: IpAddr,
        status: u16,
    },
    /// A peer outside the private, link-local and loopback ranges and not on
    /// this machine's IPv6 /64, closed at accept.
    NotPrivate {
        from: IpAddr,
    },
    /// All connections were taken (503).
    Busy {
        from: IpAddr,
    },
    /// The peer already held its share of the connections (503).
    PeerBusy {
        from: IpAddr,
    },
    /// The paired-device file couldn't be written; the devices work until the app quits.
    DevicesNotSaved(String),
    /// The paired-device file couldn't be read at start; no device is paired.
    DevicesNotRead(String),
    /// Some devices in the paired-device file didn't read and pair again;
    /// the first save copies the file aside before writing over it.
    DevicesPartlyRead {
        skipped: usize,
    },
    /// Accepting a connection failed.
    AcceptFailed(String),
    /// A connection's thread panicked; only that connection ended.
    ConnectionPanicked,
    /// A new pairing code: at start, on a rotation and after a pairing used
    /// the last one. Only with [`Config::log_pairing_codes`].
    PairingCode(String),
}

impl fmt::Display for ServerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerEvent::Paired { from } => write!(f, "remote: a device at {from} paired"),
            ServerEvent::PairFailed { from, why } => {
                let why = match why {
                    PairOutcome::Wrong | PairOutcome::Paired => "wrong code",
                    PairOutcome::Expired => "no code in force",
                    PairOutcome::Limited => "too many wrong codes this minute",
                    PairOutcome::Blocked => "too many wrong codes until the code is renewed",
                    PairOutcome::Locked => "pairing locked until the code is renewed",
                };
                write!(f, "remote: pairing from {from} failed: {why}")
            }
            ServerEvent::Unpaired { from } => write!(f, "remote: the device at {from} unpaired"),
            ServerEvent::Refused { from, status } => {
                write!(f, "remote: refused a request from {from} ({status})")
            }
            ServerEvent::NotPrivate { from } => {
                write!(
                    f,
                    "remote: closed a connection from {from}, not a private or on-link address"
                )
            }
            ServerEvent::Busy { from } => {
                write!(f, "remote: turned {from} away, all connections in use")
            }
            ServerEvent::PeerBusy { from } => {
                write!(
                    f,
                    "remote: turned {from} away, it has too many connections open"
                )
            }
            ServerEvent::DevicesNotSaved(error) => {
                write!(f, "remote: the paired devices were not saved: {error}")
            }
            ServerEvent::DevicesNotRead(error) => {
                write!(
                    f,
                    "remote: the paired devices were not read, starting with none: {error}"
                )
            }
            ServerEvent::DevicesPartlyRead { skipped } => write!(
                f,
                "remote: {skipped} paired devices didn't read and pair again; \
                 the file is copied aside before it is written again, \
                 and isn't written while that copy fails"
            ),
            ServerEvent::AcceptFailed(error) => write!(f, "remote: accepting failed: {error}"),
            ServerEvent::ConnectionPanicked => f.write_str("remote: a connection ended in a panic"),
            ServerEvent::PairingCode(code) => {
                write!(f, "remote: pairing code {code} (debug only)")
            }
        }
    }
}

type CommandSink = Box<dyn Fn(&Command) + Send + Sync>;

#[derive(Default)]
struct Published {
    revision: u64,
    current: Option<State>,
    history: VecDeque<(u64, Arc<str>)>,
}

impl Published {
    /// The snapshots a stream that sent revision `sent` still needs: each
    /// newer one in order, or only the latest for a new stream or one that
    /// fell behind the history.
    fn since(&self, sent: u64) -> Vec<(u64, Arc<str>)> {
        let (Some(oldest), Some(latest)) = (self.history.front(), self.history.back()) else {
            return Vec::new();
        };
        if latest.0 <= sent {
            return Vec::new();
        }
        if sent == 0 || oldest.0 > sent + 1 {
            return vec![latest.clone()];
        }
        self.history
            .iter()
            .filter(|(revision, _)| *revision > sent)
            .cloned()
            .collect()
    }
}

struct Shared {
    stopped: AtomicBool,
    local_name: Mutex<Option<String>>,
    content_security_policy: String,
    heartbeat: Duration,
    request_timeout: Duration,
    capacity: Capacity,
    log_pairing_codes: bool,
    pairing: Mutex<Pairing>,
    devices: Mutex<TokenStore>,
    device_file: DeviceFile,
    published: Mutex<Published>,
    changed: Condvar,
    sink: Mutex<Option<CommandSink>>,
    events: Mutex<VecDeque<ServerEvent>>,
    connections: Mutex<Vec<Connection>>,
    /// The open event streams, oldest first, with the token each belongs to.
    streams: Mutex<Vec<(u64, String)>>,
    next_stream: AtomicU64,
}

struct Connection {
    slot: Slot,
    /// Set once the request carried a paired device's token.
    paired: Arc<AtomicBool>,
    stream: TcpStream,
    thread: JoinHandle<()>,
}

/// What admission needs to know of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    peer: IpAddr,
    reserved: bool,
    paired: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Open,
    /// Let in only to show a paired device's token (see [`Capacity::reserved`]).
    Reserved,
    /// Let in as [`Admission::Reserved`] once the held connection at this index is closed.
    Replace(usize),
    Busy,
    PeerBusy,
}

/// Whether a connection from `peer` is let in beside `held`, oldest first.
/// Connections that haven't shown a paired device's token share all but the
/// reserved slots, so slow unpaired clients can't keep a paired phone out.
fn admission(held: &[Slot], peer: IpAddr, capacity: Capacity) -> Admission {
    let full = held.len() >= capacity.connections;
    if held.iter().filter(|slot| slot.peer == peer).count() >= capacity.per_peer {
        return if full {
            Admission::Busy
        } else {
            Admission::PeerBusy
        };
    }
    let unpaired = held.iter().filter(|slot| !slot.paired).count();
    if !full && unpaired < capacity.connections.saturating_sub(capacity.reserved) {
        return Admission::Open;
    }
    let waiting = held
        .iter()
        .filter(|slot| slot.peer == peer && slot.reserved && !slot.paired)
        .count();
    if waiting >= capacity.reserved_per_peer {
        return Admission::Busy;
    }
    if !full {
        return Admission::Reserved;
    }
    // A host with many addresses is many peers; the oldest waiter makes room
    // so such a host can't keep every reserved slot.
    held.iter()
        .position(|slot| slot.reserved && !slot.paired)
        .map_or(Admission::Busy, Admission::Replace)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn report(&self, event: ServerEvent) {
        let mut events = lock(&self.events);
        if events.len() >= MAX_SERVER_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// Wakes every event stream to look at the state, the devices and the stop flag again.
    fn wake_streams(&self) {
        // Taking the lock first means no stream is between its check and its wait.
        drop(lock(&self.published));
        self.changed.notify_all();
    }

    /// Writes a change to the paired devices; called with no lock held.
    fn save(&self, snapshot: Snapshot) {
        if let Err(problem) = self.device_file.save(snapshot) {
            self.report(ServerEvent::DevicesNotSaved(problem.to_string()));
        }
    }

    /// Registers a new event stream for `token`, ending the device's oldest
    /// beyond [`MAX_STREAMS_PER_DEVICE`].
    fn open_stream(&self, token: &str) -> u64 {
        let id = self.next_stream.fetch_add(1, Ordering::SeqCst);
        let ended = {
            let mut streams = lock(&self.streams);
            streams.push((id, token.to_owned()));
            let mine: Vec<u64> = streams
                .iter()
                .filter(|(_, owner)| owner == token)
                .map(|(id, _)| *id)
                .collect();
            let excess = mine.len().saturating_sub(MAX_STREAMS_PER_DEVICE);
            streams.retain(|(id, _)| !mine[..excess].contains(id));
            excess > 0
        };
        if ended {
            self.wake_streams();
        }
        id
    }

    fn stream_open(&self, id: u64) -> bool {
        lock(&self.streams).iter().any(|(open, _)| *open == id)
    }

    fn close_stream(&self, id: u64) {
        lock(&self.streams).retain(|(open, _)| *open != id);
    }

    /// Marks the connection that owns `paired` as paired; false when it was
    /// already closed to make room, so its request is not acted on.
    fn mark_paired(&self, paired: &Arc<AtomicBool>) -> bool {
        // Under the lock admit decides and closes under, so it never closes a connection marked here.
        let connections = lock(&self.connections);
        let listed = connections
            .iter()
            .any(|connection| Arc::ptr_eq(&connection.paired, paired));
        if listed {
            paired.store(true, Ordering::SeqCst);
        }
        listed
    }

    /// The last published snapshot says a microphone run is in progress.
    fn calibrating(&self) -> bool {
        lock(&self.published)
            .current
            .as_ref()
            .is_some_and(|state| state.calibrating)
    }

    fn paired(&self, request: &Request) -> Option<String> {
        let token = request.cookie(TOKEN_COOKIE)?;
        let (paired, snapshot) = lock(&self.devices).check(token, SystemTime::now());
        if let Some(snapshot) = snapshot {
            self.save(snapshot);
        }
        paired.then(|| token.to_owned())
    }

    /// Hands `command` to the app; false once the server is stopping.
    fn deliver(&self, command: &Command) -> bool {
        match lock(&self.sink).as_ref() {
            Some(sink) => {
                sink(command);
                true
            }
            None => false,
        }
    }
}

/// The running remote.
pub struct RemoteServer {
    shared: Arc<Shared>,
    port: u16,
    accept: Option<JoinHandle<()>>,
}

impl fmt::Debug for RemoteServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteServer")
            .field("port", &self.port)
            .finish()
    }
}

fn bind_all(config: &Config) -> Result<(Vec<TcpListener>, u16), RemoteError> {
    let mut listeners = Vec::new();
    let mut port = config.port;
    let mut first_error = None;
    for address in &config.listen {
        match TcpListener::bind((*address, port)) {
            Ok(listener) => {
                let bound = listener.local_addr().map_err(RemoteError::Bind)?;
                listener.set_nonblocking(true).map_err(RemoteError::Bind)?;
                port = bound.port();
                listeners.push(listener);
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if listeners.is_empty() {
        let error = first_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no address to listen on")
        });
        return Err(RemoteError::Bind(error));
    }
    Ok((listeners, port))
}

fn own_name(name: Option<&str>) -> Option<String> {
    name.map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

impl RemoteServer {
    /// Opens the listeners and makes the first pairing code. `on_command`
    /// runs on a connection thread for each accepted change, one call at a
    /// time; it must not wait for the thread that calls [`RemoteServer::stop`].
    pub fn start(
        config: Config,
        on_command: impl Fn(&Command) + Send + Sync + 'static,
    ) -> Result<RemoteServer, RemoteError> {
        let content_security_policy =
            page::content_security_policy(page::PAGE).map_err(RemoteError::Page)?;
        let limits = Limits {
            code_lifetime: config.timing.code_lifetime,
            window: config.timing.wrong_code_window,
        };
        let now = Instant::now();
        let pairing = Pairing::new(limits, now).map_err(|_| RemoteError::Random)?;
        let first_code = config
            .log_pairing_codes
            .then(|| pairing.current(now).map(str::to_owned))
            .flatten();
        let (devices, problem) = TokenStore::open(config.devices_path.clone());
        let (listeners, port) = bind_all(&config)?;
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            local_name: Mutex::new(own_name(config.local_name.as_deref())),
            content_security_policy,
            heartbeat: config.timing.heartbeat,
            request_timeout: config.timing.request,
            capacity: config.capacity,
            log_pairing_codes: config.log_pairing_codes,
            pairing: Mutex::new(pairing),
            devices: Mutex::new(devices),
            device_file: DeviceFile::default(),
            published: Mutex::new(Published::default()),
            changed: Condvar::new(),
            sink: Mutex::new(Some(Box::new(on_command))),
            events: Mutex::new(VecDeque::new()),
            connections: Mutex::new(Vec::new()),
            streams: Mutex::new(Vec::new()),
            next_stream: AtomicU64::new(0),
        });
        match problem {
            Some(LoadProblem::Unreadable(error)) => {
                shared.report(ServerEvent::DevicesNotRead(error.to_string()));
            }
            Some(LoadProblem::NotInFormat) => {
                shared.report(ServerEvent::DevicesNotRead(
                    "not in the expected format".to_owned(),
                ));
            }
            Some(LoadProblem::Skipped { skipped }) => {
                shared.report(ServerEvent::DevicesPartlyRead { skipped });
            }
            None => {}
        }
        if let Some(code) = first_code {
            shared.report(ServerEvent::PairingCode(code));
        }
        let rewrite = lock(&shared.devices).take_rewrite();
        if let Some(snapshot) = rewrite {
            shared.save(snapshot);
        }
        let accept = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("remote-accept".to_owned())
                .spawn(move || {
                    let ended = panic::catch_unwind(AssertUnwindSafe(|| {
                        accept_loop(&listeners, &shared);
                    }));
                    if ended.is_err() {
                        shared.stopped.store(true, Ordering::SeqCst);
                        shared.report(ServerEvent::AcceptFailed("a panic".to_owned()));
                    }
                })
                .map_err(RemoteError::Bind)?
        };
        Ok(RemoteServer {
            shared,
            port,
            accept: Some(accept),
        })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Makes `state` the current snapshot and streams it to the open pages;
    /// returns its revision. A snapshot equal to the current one keeps the
    /// current revision and sends nothing.
    pub fn publish(&self, state: State) -> u64 {
        let mut published = lock(&self.shared.published);
        if published.current.as_ref() == Some(&state) {
            return published.revision;
        }
        published.revision += 1;
        let revision = published.revision;
        published
            .history
            .push_back((revision, Arc::from(state.to_json(revision))));
        if published.history.len() > HISTORY {
            published.history.pop_front();
        }
        published.current = Some(state);
        drop(published);
        self.shared.changed.notify_all();
        revision
    }

    /// [`RemoteServer::publish`] for a snapshot in JSON ([`State::from_json`]).
    pub fn publish_json(&self, text: &str) -> Result<u64, StateError> {
        State::from_json(text).map(|state| self.publish(state))
    }

    /// The pairing code that works now, if any.
    pub fn pairing_code(&self) -> Option<String> {
        lock(&self.shared.pairing)
            .current(Instant::now())
            .map(str::to_owned)
    }

    /// A new pairing code, for when the user opens the setting: the old one
    /// stops working and a lockout ends.
    pub fn rotate_code(&self) -> Result<String, RemoteError> {
        let code = lock(&self.shared.pairing)
            .rotate(Instant::now())
            .map_err(|_| RemoteError::Random)?;
        if self.shared.log_pairing_codes {
            self.shared.report(ServerEvent::PairingCode(code.clone()));
        }
        Ok(code)
    }

    /// Makes `<name>.local` the accepted `Host` name in place of the one
    /// before (none: only the address), for when the computer is renamed.
    /// The pairing code, its age and the wrong-code counts stay as they are.
    pub fn set_local_name(&self, name: Option<&str>) {
        *lock(&self.shared.local_name) = own_name(name);
    }

    /// Whether too many wrong codes locked pairing until the next rotation.
    pub fn pairing_locked(&self) -> bool {
        lock(&self.shared.pairing).locked()
    }

    pub fn device_count(&self) -> usize {
        lock(&self.shared.devices).count_at(SystemTime::now())
    }

    /// Forgets every paired device; their next request gets 401 and their
    /// event streams end. In memory even when the file can't be written.
    pub fn forget_all(&self) -> io::Result<()> {
        let snapshot = lock(&self.shared.devices).clear();
        self.shared.wake_streams();
        self.shared.device_file.save(snapshot)
    }

    /// The oldest event not taken yet.
    pub fn poll_event(&self) -> Option<ServerEvent> {
        lock(&self.shared.events).pop_front()
    }

    /// Closes the listeners and every connection. Once it returns, the
    /// command callback is never called again.
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// [`RemoteServer::stop`] for an owner that keeps the server.
    pub fn stop_in_place(&mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        // Waits for a callback in progress.
        drop(lock(&self.shared.sink).take());
        self.shared.wake_streams();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let connections = std::mem::take(&mut *lock(&self.shared.connections));
        for connection in &connections {
            let _ = connection.stream.shutdown(Shutdown::Both);
        }
        // A send blocked on a peer that stopped reading may not return when
        // its socket is shut (WinSock); it ends within its write timeout.
        let until = Instant::now() + STOP_GRACE;
        for Connection { thread, .. } in connections {
            while !thread.is_finished() && Instant::now() < until {
                thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for RemoteServer {
    fn drop(&mut self) {
        if self.accept.is_some() {
            self.shutdown();
        }
    }
}

fn accept_loop(listeners: &[TcpListener], shared: &Arc<Shared>) {
    let mut refused = VecDeque::new();
    while !shared.stopped.load(Ordering::SeqCst) {
        drain_refused(&mut refused);
        let mut idle = true;
        for listener in listeners {
            match listener.accept() {
                Ok((stream, peer)) => {
                    idle = false;
                    if let Some(stream) = admit(stream, peer, shared) {
                        if refused.len() >= MAX_REFUSED {
                            refused.pop_front();
                        }
                        refused.push_back((stream, Instant::now() + REFUSED_LINGER));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => idle = false,
                Err(error) => shared.report(ServerEvent::AcceptFailed(error.to_string())),
            }
        }
        if idle {
            thread::sleep(ACCEPT_POLL);
        }
    }
}

/// Reads and drops what refused clients still send; closes each once the
/// client closed its side or its time is up.
fn drain_refused(refused: &mut VecDeque<(TcpStream, Instant)>) {
    let now = Instant::now();
    let mut buffer = [0u8; 1024];
    refused.retain_mut(|(stream, until)| {
        if now >= *until {
            return false;
        }
        for _ in 0..8 {
            match stream.read(&mut buffer) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        true
    });
}

/// Starts serving a connection, or answers 503 and hands the connection
/// back to linger: closing with an unread request would reset it, and a
/// reset can discard the answer before the client reads it.
fn admit(stream: TcpStream, peer: SocketAddr, shared: &Arc<Shared>) -> Option<TcpStream> {
    let from = peer.ip().to_canonical();
    let admitted = stream
        .local_addr()
        .is_ok_and(|local| net::admits_peer(from, local.ip()));
    if !admitted {
        shared.report(ServerEvent::NotPrivate { from });
        let _ = stream.shutdown(Shutdown::Both);
        return None;
    }
    // Accepted sockets inherit the listener's non-blocking mode on WinSock and BSD.
    let _ = stream.set_nonblocking(false);
    let peer = net::peer_group(from);
    // Held through the push, so serve marks a connection paired only while it is listed.
    let mut connections = lock(&shared.connections);
    connections.retain(|connection| !connection.thread.is_finished());
    let held: Vec<Slot> = connections
        .iter()
        .map(|connection| Slot {
            paired: connection.paired.load(Ordering::SeqCst),
            ..connection.slot
        })
        .collect();
    let admitted = admission(&held, peer, shared.capacity);
    if let Admission::Replace(oldest) = admitted {
        let _ = connections.remove(oldest).stream.shutdown(Shutdown::Both);
    }
    let refused = match admitted {
        Admission::Open | Admission::Reserved | Admission::Replace(_) => None,
        Admission::Busy => Some(ServerEvent::Busy { from }),
        Admission::PeerBusy => Some(ServerEvent::PeerBusy { from }),
    };
    if let Some(event) = refused {
        drop(connections);
        shared.report(event);
        let mut stream = stream;
        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
        let _ = http::write_response(
            &mut stream,
            &Response::error(&text::busy()),
            &shared.content_security_policy,
        );
        let _ = stream.shutdown(Shutdown::Write);
        return stream.set_nonblocking(true).is_ok().then_some(stream);
    }
    let Ok(handle) = stream.try_clone() else {
        return None;
    };
    let slot = Slot {
        peer,
        reserved: matches!(admitted, Admission::Reserved | Admission::Replace(_)),
        paired: false,
    };
    let paired = Arc::new(AtomicBool::new(false));
    let connection = Arc::clone(shared);
    let marks = Arc::clone(&paired);
    let spawned = thread::Builder::new()
        .name("remote-http".to_owned())
        .spawn(move || {
            let closer = stream.try_clone();
            let served = panic::catch_unwind(AssertUnwindSafe(|| {
                serve(stream, from, slot.reserved, &marks, &connection);
            }));
            if served.is_err() {
                connection.report(ServerEvent::ConnectionPanicked);
            }
            // The clone kept for the teardown would hold the connection open.
            if let Ok(closer) = closer {
                let _ = closer.shutdown(Shutdown::Both);
            }
        });
    if let Ok(thread) = spawned {
        connections.push(Connection {
            slot,
            paired,
            stream: handle,
            thread,
        });
    }
    None
}

/// What a request gets: one response, or the event stream.
enum Answer {
    Response(Response),
    Events { token: String },
}

impl From<Response> for Answer {
    fn from(response: Response) -> Answer {
        Answer::Response(response)
    }
}

fn error(error: ApiError) -> Answer {
    Answer::Response(Response::error(&error))
}

/// Answers one request. In a reserved slot only a request with a paired
/// device's token is served; `paired` is set once one shows it.
fn serve(
    mut stream: TcpStream,
    from: IpAddr,
    reserved: bool,
    paired: &Arc<AtomicBool>,
    shared: &Shared,
) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let Ok(local) = stream.local_addr() else {
        return;
    };
    let csp = shared.content_security_policy.as_str();
    let timeout = if reserved {
        shared.request_timeout.min(RESERVED_REQUEST)
    } else {
        shared.request_timeout
    };
    let request = match http::read_request(&mut stream, timeout) {
        Ok(request) => request,
        Err(problem) => {
            let answer = match problem.status() {
                Some(413) => text::too_large(),
                Some(_) => text::bad_request(),
                None => return,
            };
            let mut response = Response::error(&answer);
            response.status = problem.status().unwrap_or(400);
            let _ = http::write_response(&mut stream, &response, csp);
            drain_before_close(&mut stream);
            return;
        }
    };
    if shared.stopped.load(Ordering::SeqCst) {
        return;
    }
    if shared.paired(&request).is_some() {
        if !shared.mark_paired(paired) {
            return;
        }
    } else if reserved {
        shared.report(ServerEvent::Busy { from });
        let _ = http::write_response(&mut stream, &Response::error(&text::busy()), csp);
        return;
    }
    match route(&request, from, local, shared) {
        Answer::Response(response) => {
            let _ = http::write_response(&mut stream, &response, csp);
        }
        Answer::Events { token } => stream_events(stream, &token, shared),
    }
}

/// Reads and drops what the client still sends after an early answer:
/// closing with unread data would reset the connection, and a reset can
/// discard the answer before the client reads it.
fn drain_before_close(stream: &mut TcpStream) {
    const LIMIT: usize = 64 * 1024;
    let _ = stream.shutdown(Shutdown::Write);
    let until = Instant::now() + Duration::from_secs(1);
    let mut buffer = [0u8; 4096];
    let mut drained = 0;
    while drained < LIMIT {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return;
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => drained += count,
        }
    }
}

fn method_not_allowed(allow: &'static str) -> Answer {
    Response::error(&text::method_not_allowed())
        .with("Allow", allow)
        .into()
}

enum ApiRoute {
    State,
    Events,
    Master,
    Unpair,
    Speaker(Option<String>),
    Zone(Option<String>),
    Preset(Option<String>),
    Source,
}

impl ApiRoute {
    /// A change to the speakers, the volumes or the source, which waits out a
    /// microphone run (S258).
    fn writes(&self) -> bool {
        matches!(
            self,
            ApiRoute::Master
                | ApiRoute::Speaker(_)
                | ApiRoute::Zone(_)
                | ApiRoute::Preset(_)
                | ApiRoute::Source
        )
    }
}

fn route(request: &Request, from: IpAddr, local: SocketAddr, shared: &Shared) -> Answer {
    let host = match request.header("host") {
        Some(host)
            if request.header_count("host") == 1
                && net::host_allowed(host, local, lock(&shared.local_name).as_deref()) =>
        {
            host
        }
        _ => {
            shared.report(ServerEvent::Refused { from, status: 403 });
            return error(text::bad_host());
        }
    };
    let path = request.path.as_str();
    if !path.starts_with("/api/") {
        return static_route(request, path);
    }
    if request.method == "POST"
        && !(request
            .header("origin")
            .is_some_and(|origin| net::origin_matches(origin, host))
            && request.content_type_is("application/json"))
    {
        shared.report(ServerEvent::Refused { from, status: 403 });
        return error(text::bad_origin());
    }
    if path == "/api/pair" {
        return match request.method.as_str() {
            "POST" => pair(request, from, shared),
            _ => method_not_allowed("POST"),
        };
    }
    let api = match path {
        "/api/state" => ApiRoute::State,
        "/api/events" => ApiRoute::Events,
        "/api/master" => ApiRoute::Master,
        "/api/unpair" => ApiRoute::Unpair,
        "/api/source" => ApiRoute::Source,
        _ => {
            if let Some(id) = path.strip_prefix("/api/speakers/") {
                ApiRoute::Speaker(percent_decode(id))
            } else if let Some(id) = path.strip_prefix("/api/zones/") {
                ApiRoute::Zone(percent_decode(id))
            } else if let Some(id) = path.strip_prefix("/api/presets/") {
                ApiRoute::Preset(percent_decode(id))
            } else {
                return error(text::not_found());
            }
        }
    };
    let (method, allow) = match api {
        ApiRoute::State | ApiRoute::Events => ("GET", "GET"),
        _ => ("POST", "POST"),
    };
    if request.method != method {
        return method_not_allowed(allow);
    }
    let Some(token) = shared.paired(request) else {
        return error(text::not_paired());
    };
    if api.writes() && shared.calibrating() {
        return error(text::calibrating());
    }
    match api {
        ApiRoute::State => match lock(&shared.published).history.back() {
            Some((_, json)) => Response::json(200, json.to_string()).into(),
            None => error(text::not_ready()),
        },
        ApiRoute::Events => Answer::Events { token },
        ApiRoute::Master => master(request, shared),
        ApiRoute::Unpair => unpair(&token, from, shared),
        ApiRoute::Speaker(id) => speaker(request, id, shared),
        ApiRoute::Zone(id) => zone(request, id, shared),
        ApiRoute::Preset(id) => preset(request, id, shared),
        ApiRoute::Source => source(request, shared),
    }
}

fn static_route(request: &Request, path: &str) -> Answer {
    let found = match path {
        "/" => Some(("text/html; charset=utf-8", page::PAGE.as_bytes())),
        "/manifest.webmanifest" => Some(("application/manifest+json", page::MANIFEST.as_bytes())),
        "/icon-180.png" => Some(("image/png", page::ICON_180)),
        "/icon-512.png" => Some(("image/png", page::ICON_512)),
        _ => None,
    };
    let Some((content_type, body)) = found else {
        return error(text::not_found());
    };
    if request.method != "GET" {
        return method_not_allowed("GET");
    }
    Response::new(200, content_type, body)
        .with("Cache-Control", "no-cache")
        .into()
}

/// A path segment with its `%XX` escapes decoded (RFC 3986 §2.1); none
/// when an escape is broken or the result isn't UTF-8.
pub(crate) fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = segment.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(pair, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok().filter(|id| !id.is_empty())
}

fn token_cookie(value: &str, max_age: u64) -> String {
    format!("{TOKEN_COOKIE}={value}; Max-Age={max_age}; Path=/; HttpOnly; SameSite=Strict")
}

fn pair(request: &Request, from: IpAddr, shared: &Shared) -> Answer {
    let Ok(code) = state::pairing_code_from_body(&request.body) else {
        return error(text::bad_json());
    };
    // Made first, so a right code is never used up without a token to show for it.
    let Ok(token) = auth::random_token() else {
        return error(text::not_ready());
    };
    let (outcome, next_code) = {
        let mut pairing = lock(&shared.pairing);
        let now = Instant::now();
        let outcome = pairing.attempt(from, &code, now);
        let next_code = (shared.log_pairing_codes && outcome == PairOutcome::Paired)
            .then(|| pairing.current(now).map(str::to_owned))
            .flatten();
        (outcome, next_code)
    };
    let failure = match outcome {
        PairOutcome::Paired => None,
        PairOutcome::Wrong => Some(text::wrong_code()),
        PairOutcome::Expired => Some(text::code_expired()),
        PairOutcome::Limited => Some(text::too_many_tries()),
        PairOutcome::Blocked => Some(text::pairing_locked()),
        PairOutcome::Locked => Some(text::pairing_locked()),
    };
    if let Some(failure) = failure {
        shared.report(ServerEvent::PairFailed { from, why: outcome });
        return error(failure);
    }
    let snapshot = lock(&shared.devices).add(&token, SystemTime::now());
    shared.save(snapshot);
    shared.report(ServerEvent::Paired { from });
    if let Some(next_code) = next_code {
        shared.report(ServerEvent::PairingCode(next_code));
    }
    Response::json(200, json!({ "paired": true }).to_string())
        .with("Set-Cookie", token_cookie(&token, COOKIE_MAX_AGE))
        .into()
}

fn unpair(token: &str, from: IpAddr, shared: &Shared) -> Answer {
    let snapshot = lock(&shared.devices).remove(token);
    shared.wake_streams();
    if let Some(snapshot) = snapshot {
        shared.save(snapshot);
    }
    shared.report(ServerEvent::Unpaired { from });
    Response::json(200, json!({ "paired": false }).to_string())
        .with("Set-Cookie", token_cookie("", 0))
        .into()
}

fn body_error(problem: BodyError) -> Answer {
    match problem {
        BodyError::Malformed => error(text::bad_json()),
        BodyError::Empty => error(text::nothing_to_change()),
    }
}

fn range_error(problem: RangeError) -> ApiError {
    match problem {
        RangeError::Volume => text::volume_out_of_range(),
        RangeError::Delay { min_ms, max_ms } => text::delay_out_of_range(min_ms, max_ms),
    }
}

fn accepted(command: &Command, shared: &Shared) -> Answer {
    if shared.deliver(command) {
        Response::json(202, json!({ "accepted": true }).to_string()).into()
    } else {
        error(text::not_ready())
    }
}

fn master(request: &Request, shared: &Shared) -> Answer {
    let volume = match state::master_volume_from_body(&request.body) {
        Ok(volume) => volume,
        Err(problem) => return body_error(problem),
    };
    if let Err(problem) = state::check_master_volume(volume) {
        return error(range_error(problem));
    }
    accepted(&Command::Master { volume }, shared)
}

fn speaker(request: &Request, id: Option<String>, shared: &Shared) -> Answer {
    let change = match state::speaker_change_from_body(&request.body) {
        Ok(change) => change,
        Err(problem) => return body_error(problem),
    };
    let Some(id) = id else {
        return error(text::unknown_speaker());
    };
    let checked = match lock(&shared.published)
        .current
        .as_ref()
        .and_then(|state| state.speaker(&id))
    {
        None => Err(text::unknown_speaker()),
        Some(speaker) if !speaker.available => Err(text::speaker_unavailable()),
        Some(speaker) => speaker.check(&change).map_err(range_error),
    };
    match checked {
        Ok(()) => accepted(&Command::Speaker { id, change }, shared),
        Err(problem) => error(problem),
    }
}

fn zone(request: &Request, id: Option<String>, shared: &Shared) -> Answer {
    let change = match state::zone_change_from_body(&request.body) {
        Ok(change) => change,
        Err(problem) => return body_error(problem),
    };
    let Some(id) = id else {
        return error(text::unknown_zone());
    };
    let checked = match lock(&shared.published)
        .current
        .as_ref()
        .and_then(|state| state.zone(&id).map(|zone| state.check_zone(zone, &change)))
    {
        None => Err(text::unknown_zone()),
        Some(result) => result.map_err(|problem| match problem {
            ZoneProblem::Unavailable => text::zone_unavailable(),
            ZoneProblem::Range(range) => range_error(range),
        }),
    };
    match checked {
        Ok(()) => accepted(&Command::Zone { id, change }, shared),
        Err(problem) => error(problem),
    }
}

/// `POST /api/source` `{id}`: a pick in the app's source picker. The app
/// checks it again when it applies it.
fn source(request: &Request, shared: &Shared) -> Answer {
    let id = match state::source_id_from_body(&request.body) {
        Ok(id) => id,
        Err(problem) => return body_error(problem),
    };
    let checked = match lock(&shared.published)
        .current
        .as_ref()
        .and_then(|state| state.source(&id))
    {
        None => Err(text::unknown_source()),
        Some(source) if !source.available => Err(text::source_unavailable()),
        Some(_) => Ok(()),
    };
    match checked {
        Ok(()) => accepted(&Command::Source { id }, shared),
        Err(problem) => error(problem),
    }
}

fn preset(request: &Request, id: Option<String>, shared: &Shared) -> Answer {
    if let Err(problem) = state::preset_recall_from_body(&request.body) {
        return body_error(problem);
    }
    let known = id.as_deref().is_some_and(|id| {
        lock(&shared.published)
            .current
            .as_ref()
            .is_some_and(|state| state.preset(id).is_some())
    });
    match id {
        Some(id) if known => accepted(&Command::Preset { id }, shared),
        _ => error(text::unknown_preset()),
    }
}

/// The event stream (HTML Living Standard §9.2): the current snapshot at
/// once, then each new revision once, a comment line when it has been quiet
/// for a heartbeat. It ends when the server stops, the device is forgotten
/// or the device opens a stream too many, which sends a last `replaced` event.
fn stream_events(stream: TcpStream, token: &str, shared: &Shared) {
    let id = shared.open_stream(token);
    send_events(stream, token, id, shared);
    shared.close_stream(id);
}

fn send_events(mut stream: TcpStream, token: &str, id: u64, shared: &Shared) {
    let headers = [
        ("Content-Type", "text/event-stream".to_owned()),
        ("Cache-Control", "no-store".to_owned()),
    ];
    let head = http::head(200, &headers, &shared.content_security_policy) + "\r\n";
    if stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.flush())
        .is_err()
    {
        return;
    }
    let mut sent = 0u64;
    let mut last_write = Instant::now();
    loop {
        let batch = {
            let mut published = lock(&shared.published);
            loop {
                if shared.stopped.load(Ordering::SeqCst)
                    || !lock(&shared.devices).contains(token, SystemTime::now())
                {
                    return;
                }
                if !shared.stream_open(id) {
                    drop(published);
                    // Without it the page would reconnect and end the device's next stream in turn.
                    let _ = stream
                        .write_all(b"event: replaced\ndata: {}\n\n")
                        .and_then(|()| stream.flush());
                    return;
                }
                let batch = published.since(sent);
                if !batch.is_empty() {
                    break batch;
                }
                let left = shared.heartbeat.saturating_sub(last_write.elapsed());
                if left.is_zero() {
                    break Vec::new();
                }
                published = shared
                    .changed
                    .wait_timeout(published, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        let mut text = String::new();
        if batch.is_empty() {
            text.push_str(": heartbeat\n\n");
        }
        for (revision, json) in &batch {
            text.push_str(&format!("event: state\nid: {revision}\ndata: {json}\n\n"));
            sent = *revision;
        }
        if stream
            .write_all(text.as_bytes())
            .and_then(|()| stream.flush())
            .is_err()
        {
            return;
        }
        last_write = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(peer: IpAddr, count: usize, reserved: bool, paired: bool) -> Vec<Slot> {
        vec![
            Slot {
                peer,
                reserved,
                paired
            };
            count
        ]
    }

    fn address(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    // Private addresses are built from their parts: published files never spell one.
    fn home(subnet: u8, host: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, subnet, host))
    }

    #[test]
    fn two_ipv6_hosts_in_one_64_have_a_connection_cap_each() {
        let capacity = Capacity::default();
        let laptop = net::peer_group(address("2001:db8:1:2::1"));
        let mut slots = held(laptop, MAX_CONNECTIONS_PER_PEER, false, false);
        assert_eq!(admission(&slots, laptop, capacity), Admission::PeerBusy);
        let phone = net::peer_group(address("2001:db8:1:2::2"));
        assert_eq!(admission(&slots, phone, capacity), Admission::Open);
        slots.extend(held(phone, 1, false, true));
        assert_eq!(admission(&slots, phone, capacity), Admission::Open);
    }

    #[test]
    fn unpaired_connections_leave_the_reserved_slots_to_paired_devices() {
        let capacity = Capacity::default();
        let peers: Vec<IpAddr> = (1..=4).map(|host| home(0, host)).collect();
        let mut slots: Vec<Slot> = peers[..3]
            .iter()
            .flat_map(|peer| held(*peer, MAX_CONNECTIONS_PER_PEER, false, false))
            .collect();
        assert_eq!(slots.len(), MAX_CONNECTIONS - RESERVED_FOR_PAIRED);
        let fourth = peers[3];
        for _ in 0..RESERVED_PER_PEER {
            assert_eq!(admission(&slots, fourth, capacity), Admission::Reserved);
            slots.extend(held(fourth, 1, true, false));
        }
        assert_eq!(admission(&slots, fourth, capacity), Admission::Busy);
        // A reserved slot that showed a token holds it like any paired connection.
        let paired_phone = home(0, 9);
        slots.extend(held(paired_phone, 1, true, true));
        assert_eq!(
            admission(&slots, paired_phone, capacity),
            Admission::Reserved
        );
        slots.extend(held(paired_phone, 1, true, true));
        assert_eq!(
            admission(&slots, paired_phone, capacity),
            Admission::Reserved
        );
        let full: Vec<Slot> = (0..MAX_CONNECTIONS)
            .map(|index| Slot {
                peer: home(1, index as u8),
                reserved: false,
                paired: true,
            })
            .collect();
        assert_eq!(admission(&full, paired_phone, capacity), Admission::Busy);
    }

    #[test]
    fn a_host_with_many_addresses_cannot_hold_every_reserved_slot() {
        let capacity = Capacity::default();
        let mut slots: Vec<Slot> = (1..=3)
            .flat_map(|host| held(home(0, host), MAX_CONNECTIONS_PER_PEER, false, false))
            .collect();
        let shared = slots.len();
        // Link-local addresses are peers of their own, so one host can bring many.
        for host in 1..=RESERVED_FOR_PAIRED / RESERVED_PER_PEER {
            let peer = net::peer_group(address(&format!("fe80::{host}")));
            for _ in 0..RESERVED_PER_PEER {
                assert_eq!(admission(&slots, peer, capacity), Admission::Reserved);
                slots.extend(held(peer, 1, true, false));
            }
        }
        assert_eq!(slots.len(), MAX_CONNECTIONS);
        let phone = home(0, 9);
        assert_eq!(
            admission(&slots, phone, capacity),
            Admission::Replace(shared)
        );
        // A peer already waiting in its share of reserved slots gets no more.
        let last = slots[MAX_CONNECTIONS - 1].peer;
        assert_eq!(admission(&slots, last, capacity), Admission::Busy);
    }

    #[test]
    fn paired_connections_leave_the_shared_slots_open() {
        let capacity = Capacity::default();
        let mut slots = Vec::new();
        for host in 1..=3 {
            slots.extend(held(home(0, host), MAX_CONNECTIONS_PER_PEER, false, true));
        }
        assert_eq!(admission(&slots, home(0, 4), capacity), Admission::Open);
    }

    fn published(revisions: std::ops::RangeInclusive<u64>) -> Published {
        let mut published = Published::default();
        for revision in revisions {
            published.revision = revision;
            published
                .history
                .push_back((revision, Arc::from(revision.to_string())));
        }
        published
    }

    fn revisions(batch: Vec<(u64, Arc<str>)>) -> Vec<u64> {
        batch.into_iter().map(|(revision, _)| revision).collect()
    }

    #[test]
    fn a_stream_gets_each_revision_once() {
        let kept = published(3..=6);
        assert_eq!(revisions(kept.since(0)), [6]);
        assert_eq!(revisions(kept.since(4)), [5, 6]);
        assert_eq!(revisions(kept.since(2)), [3, 4, 5, 6]);
        assert_eq!(revisions(kept.since(1)), [6], "fell behind the history");
        assert!(kept.since(6).is_empty());
        assert!(Published::default().since(0).is_empty());
    }

    #[test]
    fn speaker_ids_are_percent_decoded() {
        assert_eq!(percent_decode("den"), Some("den".to_owned()));
        assert_eq!(
            percent_decode("AppleUSB%3A12%2Fx%20y"),
            Some("AppleUSB:12/x y".to_owned())
        );
        assert_eq!(percent_decode("caf%C3%A9"), Some("café".to_owned()));
        assert_eq!(percent_decode("bad%2"), None);
        assert_eq!(percent_decode("bad%zz"), None);
        assert_eq!(percent_decode("%FF"), None);
        assert_eq!(percent_decode(""), None);
    }

    fn pair_request(host: &str, code: &str) -> Request {
        let head = format!(
            "POST /api/pair HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: application/json"
        );
        let mut request = http::parse_head(head.as_bytes()).unwrap();
        request.body = json!({ "code": code }).to_string().into_bytes();
        request
    }

    fn status(answer: Answer) -> u16 {
        match answer {
            Answer::Response(response) => response.status,
            Answer::Events { .. } => 200,
        }
    }

    // Through route, not sockets: a test can't connect from 127.0.0.2 on macOS.
    #[test]
    fn a_new_local_name_keeps_the_pairing_lockout_and_the_code() {
        let config = Config {
            port: 0,
            listen: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            local_name: Some("kitchen".to_owned()),
            ..Config::default()
        };
        let server = RemoteServer::start(config, |_: &Command| {}).unwrap();
        let port = server.port();
        let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let named = |name: &str| format!("{name}.local:{port}");
        let code = server.pairing_code().unwrap();
        let wrong: String = code
            .chars()
            .map(|digit| if digit == '0' { '1' } else { '0' })
            .collect();
        for index in 0..auth::WRONG_BEFORE_LOCK {
            let host = 1 + (index as usize / auth::WRONG_PER_WINDOW) as u8;
            let from = IpAddr::V4(Ipv4Addr::new(127, 0, 0, host));
            let request = pair_request(&named("kitchen"), &wrong);
            assert_eq!(status(route(&request, from, local, &server.shared)), 401);
        }
        assert!(server.pairing_locked());

        server.set_local_name(Some("kitchen-2"));

        assert!(server.pairing_locked());
        assert_eq!(server.pairing_code().as_deref(), Some(code.as_str()));
        let from = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 100));
        let renamed = pair_request(&named("kitchen-2"), &code);
        assert_eq!(status(route(&renamed, from, local, &server.shared)), 429);
        let old = pair_request(&named("kitchen"), &code);
        assert_eq!(status(route(&old, from, local, &server.shared)), 403);
        server.stop();
    }

    #[test]
    fn log_lines_never_carry_a_code() {
        let from = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let line = ServerEvent::PairFailed {
            from,
            why: PairOutcome::Wrong,
        }
        .to_string();
        assert_eq!(line, "remote: pairing from 127.0.0.1 failed: wrong code");
    }

    #[test]
    fn a_config_reads_from_json_with_defaults() {
        let config = Config::from_json("{}").unwrap();
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.listen.len(), 2);
        assert_eq!(config.devices_path, None);
        let config = Config::from_json(
            r#"{"port": 0, "local_name": "kitchen", "devices_path": "remote-devices.json", "listen": ["127.0.0.1", "::1"]}"#,
        )
        .unwrap();
        assert_eq!(config.port, 0);
        assert_eq!(config.local_name.as_deref(), Some("kitchen"));
        assert_eq!(
            config.devices_path,
            Some(PathBuf::from("remote-devices.json"))
        );
        assert_eq!(
            config.listen,
            [
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            ]
        );
        for bad in [
            "[]",
            "not json",
            r#"{"port": 70000}"#,
            r#"{"port": "80"}"#,
            r#"{"local_name": 1}"#,
            r#"{"listen": ["nowhere"]}"#,
        ] {
            assert!(Config::from_json(bad).is_none(), "{bad}");
        }
    }
}
