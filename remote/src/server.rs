//! The server: its listeners, one thread per connection, the routes, and
//! the event stream that carries each new snapshot to the open pages.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::auth::{self, Limits, LoadProblem, PairOutcome, Pairing, TokenStore};
use crate::http::{self, Request, Response};
use crate::net;
use crate::page::{self, PageError};
use crate::state::{self, BodyError, Command, RangeError, State, StateError};
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
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a stop waits for the connection threads.
const STOP_GRACE: Duration = Duration::from_millis(500);
/// Snapshots kept so a stream that fell a little behind still gets each one.
const HISTORY: usize = 32;
/// Server events kept for a host that doesn't poll.
pub const MAX_SERVER_EVENTS: usize = 64;

/// The time limits; the defaults are the product's, tests shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// How long a pairing code works.
    pub code_lifetime: Duration,
    /// The window in which one address may send five wrong codes.
    pub wrong_code_window: Duration,
    /// The longest an event stream stays silent; a comment line fills it.
    pub heartbeat: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        let limits = Limits::default();
        Timing {
            code_lifetime: limits.code_lifetime,
            wrong_code_window: limits.window,
            heartbeat: Duration::from_secs(15),
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
        }
    }
}

impl Config {
    /// Reads `{"port"?, "local_name"?, "devices_path"?, "listen"?}`, the
    /// defaults filling in what is missing; `listen` is a list of IP
    /// literals. None when a field has the wrong type or value.
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
/// refused, never a code or a token.
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
    /// The paired-device file couldn't be written; the devices work until the app quits.
    DevicesNotSaved(String),
    /// The paired-device file couldn't be read at start; no device is paired.
    DevicesNotRead(String),
    /// Accepting a connection failed.
    AcceptFailed(String),
    /// A connection's thread panicked; only that connection ended.
    ConnectionPanicked,
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
            ServerEvent::DevicesNotSaved(error) => {
                write!(f, "remote: the paired devices were not saved: {error}")
            }
            ServerEvent::DevicesNotRead(error) => {
                write!(
                    f,
                    "remote: the paired devices were not read, starting with none: {error}"
                )
            }
            ServerEvent::AcceptFailed(error) => write!(f, "remote: accepting failed: {error}"),
            ServerEvent::ConnectionPanicked => f.write_str("remote: a connection ended in a panic"),
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
    local_name: Option<String>,
    content_security_policy: String,
    heartbeat: Duration,
    pairing: Mutex<Pairing>,
    devices: Mutex<TokenStore>,
    published: Mutex<Published>,
    changed: Condvar,
    sink: Mutex<Option<CommandSink>>,
    events: Mutex<VecDeque<ServerEvent>>,
    connections: Mutex<Vec<(TcpStream, JoinHandle<()>)>>,
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

    fn paired(&self, request: &Request) -> Option<String> {
        let token = request.cookie(TOKEN_COOKIE)?;
        lock(&self.devices)
            .contains(token)
            .then(|| token.to_owned())
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
        let pairing = Pairing::new(limits, Instant::now()).map_err(|_| RemoteError::Random)?;
        let (devices, problem) = TokenStore::open(config.devices_path.clone());
        let (listeners, port) = bind_all(&config)?;
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            local_name: config
                .local_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            content_security_policy,
            heartbeat: config.timing.heartbeat,
            pairing: Mutex::new(pairing),
            devices: Mutex::new(devices),
            published: Mutex::new(Published::default()),
            changed: Condvar::new(),
            sink: Mutex::new(Some(Box::new(on_command))),
            events: Mutex::new(VecDeque::new()),
            connections: Mutex::new(Vec::new()),
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
            None => {}
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
        lock(&self.shared.pairing)
            .rotate(Instant::now())
            .map_err(|_| RemoteError::Random)
    }

    /// Whether too many wrong codes locked pairing until the next rotation.
    pub fn pairing_locked(&self) -> bool {
        lock(&self.shared.pairing).locked()
    }

    pub fn device_count(&self) -> usize {
        lock(&self.shared.devices).len()
    }

    /// Forgets every paired device; their next request gets 401 and their
    /// event streams end. In memory even when the file can't be written.
    pub fn forget_all(&self) -> io::Result<()> {
        let saved = lock(&self.shared.devices).clear();
        self.shared.wake_streams();
        saved
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
        for (stream, _) in &connections {
            let _ = stream.shutdown(Shutdown::Both);
        }
        // A send blocked on a peer that stopped reading may not return when
        // its socket is shut (WinSock); it ends within its write timeout.
        let until = Instant::now() + STOP_GRACE;
        for (_, thread) in connections {
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
    while !shared.stopped.load(Ordering::SeqCst) {
        let mut idle = true;
        for listener in listeners {
            match listener.accept() {
                Ok((stream, peer)) => {
                    idle = false;
                    admit(stream, peer, shared);
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

fn admit(stream: TcpStream, peer: SocketAddr, shared: &Arc<Shared>) {
    let from = peer.ip().to_canonical();
    let admitted = stream
        .local_addr()
        .is_ok_and(|local| net::admits_peer(from, local.ip()));
    if !admitted {
        shared.report(ServerEvent::NotPrivate { from });
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    // Accepted sockets inherit the listener's non-blocking mode on WinSock and BSD.
    let _ = stream.set_nonblocking(false);
    let mut connections = lock(&shared.connections);
    connections.retain(|(_, thread)| !thread.is_finished());
    if connections.len() >= MAX_CONNECTIONS {
        drop(connections);
        shared.report(ServerEvent::Busy { from });
        let mut stream = stream;
        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
        let _ = http::write_response(
            &mut stream,
            &Response::error(&text::busy()),
            &shared.content_security_policy,
        );
        return;
    }
    drop(connections);
    let Ok(handle) = stream.try_clone() else {
        return;
    };
    let connection = Arc::clone(shared);
    let spawned = thread::Builder::new()
        .name("remote-http".to_owned())
        .spawn(move || {
            let closer = stream.try_clone();
            let served = panic::catch_unwind(AssertUnwindSafe(|| serve(stream, from, &connection)));
            if served.is_err() {
                connection.report(ServerEvent::ConnectionPanicked);
            }
            // The clone kept for the teardown would hold the connection open.
            if let Ok(closer) = closer {
                let _ = closer.shutdown(Shutdown::Both);
            }
        });
    if let Ok(thread) = spawned {
        let mut connections = lock(&shared.connections);
        connections.retain(|(_, thread)| !thread.is_finished());
        connections.push((handle, thread));
    }
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

fn serve(mut stream: TcpStream, from: IpAddr, shared: &Shared) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let Ok(local) = stream.local_addr() else {
        return;
    };
    let csp = shared.content_security_policy.as_str();
    let request = match http::read_request(&mut stream) {
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
}

fn route(request: &Request, from: IpAddr, local: SocketAddr, shared: &Shared) -> Answer {
    let host = match request.header("host") {
        Some(host)
            if request.header_count("host") == 1
                && net::host_allowed(host, local, shared.local_name.as_deref()) =>
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
        _ => match path.strip_prefix("/api/speakers/") {
            Some(id) => ApiRoute::Speaker(percent_decode(id)),
            None => return error(text::not_found()),
        },
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
    match api {
        ApiRoute::State => match lock(&shared.published).history.back() {
            Some((_, json)) => Response::json(200, json.to_string()).into(),
            None => error(text::not_ready()),
        },
        ApiRoute::Events => Answer::Events { token },
        ApiRoute::Master => master(request, shared),
        ApiRoute::Unpair => unpair(&token, from, shared),
        ApiRoute::Speaker(id) => speaker(request, id, shared),
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
fn percent_decode(segment: &str) -> Option<String> {
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
    let outcome = lock(&shared.pairing).attempt(from, &code, Instant::now());
    let failure = match outcome {
        PairOutcome::Paired => None,
        PairOutcome::Wrong => Some(text::wrong_code()),
        PairOutcome::Expired => Some(text::code_expired()),
        PairOutcome::Limited => Some(text::too_many_tries()),
        PairOutcome::Locked => Some(text::pairing_locked()),
    };
    if let Some(failure) = failure {
        shared.report(ServerEvent::PairFailed { from, why: outcome });
        return error(failure);
    }
    if let Err(problem) = lock(&shared.devices).add(&token) {
        shared.report(ServerEvent::DevicesNotSaved(problem.to_string()));
    }
    shared.report(ServerEvent::Paired { from });
    Response::json(200, json!({ "paired": true }).to_string())
        .with("Set-Cookie", token_cookie(&token, COOKIE_MAX_AGE))
        .into()
}

fn unpair(token: &str, from: IpAddr, shared: &Shared) -> Answer {
    if let Err(problem) = lock(&shared.devices).remove(token) {
        shared.report(ServerEvent::DevicesNotSaved(problem.to_string()));
    }
    shared.wake_streams();
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

/// The event stream (HTML Living Standard §9.2): the current snapshot at
/// once, then each new revision once, a comment line when it has been quiet
/// for a heartbeat. It ends when the server stops or the device is forgotten.
fn stream_events(mut stream: TcpStream, token: &str, shared: &Shared) {
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
                if shared.stopped.load(Ordering::SeqCst) || !lock(&shared.devices).contains(token) {
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
