//! The remote over real sockets on 127.0.0.1, which counts as a private peer.

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_remote::page::{self, hash_source};
use audiosync_remote::{
    Capacity, Command, Config, RemoteServer, ServerEvent, SpeakerChange, State, Timing, ZoneChange,
};
use serde_json::{Value, json};

const SAMPLE: &str = r#"{
    "computer_name": "Den Mac",
    "master": {"volume": 70},
    "now_playing": {"title": "Song", "artist": "Band", "source": "Music"},
    "speakers": [
        {"id": "kitchen", "name": "Kitchen", "kind": "AirPlay 2", "on": true, "volume": 40,
         "delay_ms": 120, "delay_min_ms": 0, "delay_max_ms": 3000, "available": true, "notice": null},
        {"id": "den", "name": "Den", "kind": "Google Cast", "on": false, "volume": 10,
         "delay_ms": 0, "delay_min_ms": -500, "delay_max_ms": 500, "available": false,
         "notice": "Not on the network"},
        {"id": "living room", "name": "Living Room", "kind": "local", "on": true, "volume": 90,
         "delay_ms": 0, "delay_min_ms": 0, "delay_max_ms": 1000, "available": true, "notice": null}
    ]
}"#;

type Received = Arc<Mutex<Vec<Command>>>;

struct Remote {
    server: RemoteServer,
    received: Received,
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("remote-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    dir.join("remote-devices.json")
}

fn config(name: &str, timing: Timing) -> Config {
    Config {
        port: 0,
        listen: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        local_name: Some("kitchen".to_owned()),
        devices_path: Some(scratch(name)),
        timing,
        capacity: Capacity::default(),
        log_pairing_codes: false,
    }
}

fn start_with(config: Config) -> Remote {
    let received: Received = Arc::default();
    let sink = Arc::clone(&received);
    let server = RemoteServer::start(config, move |command: &Command| {
        sink.lock().unwrap().push(command.clone());
    })
    .unwrap();
    Remote { server, received }
}

fn start(name: &str) -> Remote {
    start_with(config(name, Timing::default()))
}

#[derive(Debug)]
struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }

    fn error(&self) -> String {
        self.json()["error"].as_str().unwrap_or_default().to_owned()
    }
}

fn exchange(port: u16, raw: &[u8]) -> Reply {
    let started = Instant::now();
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    // A low-priority test run on a saturated PC can wait seconds for each of
    // the accept and connection threads; 5 s ran out there with no answer yet.
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream.write_all(raw).unwrap();
    let mut data = Vec::new();
    if let Err(error) = stream.read_to_end(&mut data) {
        let request = String::from_utf8_lossy(raw);
        panic!(
            "{}: {error:?} after {:?}, {} bytes read",
            request.lines().next().unwrap_or_default(),
            started.elapsed(),
            data.len()
        );
    }
    let text = String::from_utf8_lossy(&data).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a response head");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

fn host(remote: &Remote) -> String {
    format!("127.0.0.1:{}", remote.server.port())
}

fn get(remote: &Remote, path: &str, token: Option<&str>) -> Reply {
    let cookie = token.map_or(String::new(), |token| {
        format!("Cookie: as_token={token}\r\n")
    });
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\n{cookie}\r\n",
        host(remote)
    );
    exchange(remote.server.port(), raw.as_bytes())
}

fn post(remote: &Remote, path: &str, token: Option<&str>, body: &str) -> Reply {
    let host = host(remote);
    post_with(
        remote,
        path,
        token,
        body,
        &format!("http://{host}"),
        "application/json",
    )
}

fn post_with(
    remote: &Remote,
    path: &str,
    token: Option<&str>,
    body: &str,
    origin: &str,
    content_type: &str,
) -> Reply {
    let cookie = token.map_or(String::new(), |token| {
        format!("Cookie: as_token={token}\r\n")
    });
    let raw = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nOrigin: {origin}\r\nContent-Type: {content_type}\r\n{cookie}Content-Length: {}\r\n\r\n{body}",
        host(remote),
        body.len()
    );
    exchange(remote.server.port(), raw.as_bytes())
}

fn wrong(code: &str) -> String {
    let digit = (code.as_bytes()[0] - b'0' + 1) % 10;
    format!("{digit}{}", &code[1..])
}

fn token_of(reply: &Reply) -> String {
    let cookie = reply.header("set-cookie").expect("a cookie");
    let value = cookie.strip_prefix("as_token=").unwrap();
    value.split(';').next().unwrap().to_owned()
}

fn pair(remote: &Remote) -> String {
    let code = remote.server.pairing_code().unwrap();
    let reply = post(
        remote,
        "/api/pair",
        None,
        &json!({ "code": code }).to_string(),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    token_of(&reply)
}

fn sample() -> State {
    State::from_json(SAMPLE).unwrap()
}

fn assert_security_headers(reply: &Reply) {
    let policy = reply.header("content-security-policy").expect("a CSP");
    assert!(policy.starts_with("default-src 'self'"), "{policy}");
    assert!(!policy.contains("unsafe-inline"));
    assert_eq!(reply.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(reply.header("referrer-policy"), Some("no-referrer"));
}

fn element<'a>(page: &'a str, tag: &str) -> &'a str {
    let start = page.find(&format!("<{tag}>")).unwrap() + tag.len() + 2;
    let end = page[start..].find(&format!("</{tag}>")).unwrap();
    &page[start..start + end]
}

#[test]
fn the_page_and_its_assets_carry_the_hash_policy() {
    let remote = start("page");
    let reply = get(&remote, "/", None);
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(reply.body, page::PAGE);
    assert_security_headers(&reply);
    let policy = reply.header("content-security-policy").unwrap();
    let script = hash_source(element(&reply.body, "script"));
    let style = hash_source(element(&reply.body, "style"));
    assert!(
        policy.contains(&format!("script-src {script};")),
        "{policy}"
    );
    assert!(policy.contains(&format!("style-src {style};")), "{policy}");

    let manifest = get(&remote, "/manifest.webmanifest", None);
    assert_eq!(
        manifest.header("content-type"),
        Some("application/manifest+json")
    );
    assert_eq!(manifest.json()["display"], "standalone");
    assert_security_headers(&manifest);
    for icon in ["/icon-180.png", "/icon-512.png"] {
        let reply = get(&remote, icon, None);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.header("content-type"), Some("image/png"));
    }
    let missing = get(&remote, "/nothing", None);
    assert_eq!(missing.status, 404);
    assert_security_headers(&missing);
    let posted = post(&remote, "/", None, "{}");
    assert_eq!(posted.status, 405);
    assert_eq!(posted.header("allow"), Some("GET"));
}

#[test]
fn a_foreign_host_gets_403_everywhere() {
    let remote = start("host");
    let port = remote.server.port();
    for path in ["/", "/api/state", "/api/pair"] {
        let raw = format!("GET {path} HTTP/1.1\r\nHost: evil.example:{port}\r\n\r\n");
        let reply = exchange(port, raw.as_bytes());
        assert_eq!(reply.status, 403, "{path}");
        assert_eq!(reply.error(), "bad_host");
        assert_security_headers(&reply);
    }
    let wrong_port = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        port.wrapping_add(1)
    );
    assert_eq!(exchange(port, wrong_port.as_bytes()).status, 403);
    assert_eq!(exchange(port, b"GET / HTTP/1.1\r\n\r\n").status, 403);
    let twice = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nHost: evil.example\r\n\r\n");
    assert_eq!(exchange(port, twice.as_bytes()).status, 403);
    let named = format!("GET / HTTP/1.1\r\nHost: Kitchen.local:{port}\r\n\r\n");
    assert_eq!(exchange(port, named.as_bytes()).status, 200);
    assert_eq!(exchange(port, b"GET / HTTP/2\r\n\r\n").status, 400);
}

#[test]
fn a_post_needs_a_matching_origin_and_json() {
    let remote = start("origin");
    let code = remote.server.pairing_code().unwrap();
    let body = json!({ "code": code }).to_string();
    let host = host(&remote);
    let own = format!("http://{host}");
    for (origin, content_type) in [
        ("http://evil.example", "application/json"),
        ("null", "application/json"),
        (own.as_str(), "text/plain"),
        (own.as_str(), "application/x-www-form-urlencoded"),
    ] {
        let reply = post_with(&remote, "/api/pair", None, &body, origin, content_type);
        assert_eq!(reply.status, 403, "{origin} {content_type}");
        assert_eq!(reply.error(), "bad_origin");
    }
    let raw = format!(
        "POST /api/pair HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    assert_eq!(exchange(remote.server.port(), raw.as_bytes()).status, 403);
    // None of these used the code up.
    assert_eq!(remote.server.pairing_code(), Some(code));
    let named = post_with(
        &remote,
        "/api/pair",
        None,
        &body,
        &format!("http://kitchen.local:{}", remote.server.port()),
        "application/json; charset=utf-8",
    );
    // The Host is the IP literal, so an Origin naming the .local name doesn't match it.
    assert_eq!(named.status, 403);
    assert_eq!(post(&remote, "/api/pair", None, &body).status, 200);
}

#[test]
fn the_right_code_pairs_once_with_a_strict_cookie() {
    let remote = start("pair");
    let code = remote.server.pairing_code().unwrap();
    assert_eq!(code.len(), 6);
    let body = json!({ "code": code }).to_string();
    let reply = post(&remote, "/api/pair", None, &body);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    let cookie = reply.header("set-cookie").unwrap();
    for attribute in ["HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(cookie.contains(attribute), "{cookie}");
    }
    let token = token_of(&reply);
    assert_eq!(token.len(), 32);
    assert_eq!(remote.server.device_count(), 1);

    let again = post(&remote, "/api/pair", None, &body);
    assert_eq!(again.status, 401);
    assert_eq!(again.error(), "wrong_code");
    assert_ne!(remote.server.pairing_code(), Some(code.clone()));

    let wrong_code = wrong(&remote.server.pairing_code().unwrap());
    let reply = post(
        &remote,
        "/api/pair",
        None,
        &json!({ "code": wrong_code }).to_string(),
    );
    assert_eq!(reply.status, 401);
    assert_eq!(reply.error(), "wrong_code");
    assert_eq!(
        post(&remote, "/api/pair", None, "{\"code\": 12}").status,
        400
    );
    assert_eq!(post(&remote, "/api/pair", None, "code=1").status, 400);
    assert_eq!(get(&remote, "/api/pair", None).status, 405);

    let events: Vec<String> = std::iter::from_fn(|| remote.server.poll_event())
        .map(|event| event.to_string())
        .collect();
    assert!(
        events.iter().any(|line| line.contains("127.0.0.1 paired")),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .all(|line| !line.contains(&code) && !line.contains(&token))
    );
}

#[test]
fn a_code_expires() {
    let timing = Timing {
        code_lifetime: Duration::from_millis(300),
        ..Timing::default()
    };
    let remote = start_with(config("expiry", timing));
    let code = remote.server.pairing_code().unwrap();
    thread::sleep(Duration::from_millis(400));
    assert_eq!(remote.server.pairing_code(), None);
    let reply = post(
        &remote,
        "/api/pair",
        None,
        &json!({ "code": code }).to_string(),
    );
    assert_eq!(reply.status, 401);
    assert_eq!(reply.error(), "code_expired");
    let fresh = remote.server.rotate_code().unwrap();
    let reply = post(
        &remote,
        "/api/pair",
        None,
        &json!({ "code": fresh }).to_string(),
    );
    assert_eq!(reply.status, 200);
}

#[test]
fn wrong_codes_are_limited_then_refused_for_that_address_only() {
    let window = Duration::from_secs(1);
    let timing = Timing {
        wrong_code_window: window,
        ..Timing::default()
    };
    let remote = start_with(config("limits", timing));
    // The five wrong codes and the right one must land inside the window. A
    // starved run can take longer, so it starts over: a new code clears the counts.
    let mut rounds = 0;
    let code = loop {
        rounds += 1;
        let code = remote.server.rotate_code().unwrap();
        let wrong = json!({ "code": wrong(&code) }).to_string();
        let started = Instant::now();
        for _ in 0..5 {
            assert_eq!(post(&remote, "/api/pair", None, &wrong).status, 401);
        }
        let limited = post(
            &remote,
            "/api/pair",
            None,
            &json!({ "code": code }).to_string(),
        );
        if limited.status == 429 {
            assert_eq!(limited.error(), "too_many_tries");
            break code;
        }
        let took = started.elapsed();
        assert!(took >= window, "{} after {took:?}", limited.status);
        assert!(rounds < 10, "every round outlasted the window");
    };
    thread::sleep(window + Duration::from_millis(100));
    let right = json!({ "code": code }).to_string();
    let wrong = json!({ "code": wrong(&code) }).to_string();
    // Past the window five more wrong codes are let in; the tenth blocks this peer.
    for _ in 0..5 {
        assert_eq!(post(&remote, "/api/pair", None, &wrong).status, 401);
    }
    let blocked = post(&remote, "/api/pair", None, &right);
    assert_eq!(blocked.status, 429);
    assert_eq!(blocked.error(), "pairing_locked");
    thread::sleep(window + Duration::from_millis(100));
    let refused = post(&remote, "/api/pair", None, &right);
    assert_eq!(refused.status, 429);
    assert_eq!(refused.error(), "pairing_locked");
    // Only 127.0.0.1 used up its tries; pairing stays open to everyone else.
    assert!(!remote.server.pairing_locked());
    let fresh = remote.server.rotate_code().unwrap();
    let reply = post(
        &remote,
        "/api/pair",
        None,
        &json!({ "code": fresh }).to_string(),
    );
    assert_eq!(reply.status, 200);
}

#[test]
fn every_api_route_needs_a_paired_token() {
    let remote = start("tokens");
    remote.server.publish(sample());
    let paired = pair(&remote);
    let unknown = "0123456789abcdef0123456789abcdef";
    for token in [None, Some("nonsense"), Some(unknown)] {
        for (method, path) in [
            ("GET", "/api/state"),
            ("GET", "/api/events"),
            ("POST", "/api/speakers/kitchen"),
            ("POST", "/api/master"),
            ("POST", "/api/unpair"),
            ("POST", "/api/source"),
        ] {
            let reply = match method {
                "GET" => get(&remote, path, token),
                _ => post(&remote, path, token, "{\"volume\": 5}"),
            };
            assert_eq!(reply.status, 401, "{method} {path} {token:?}");
            assert_eq!(reply.error(), "not_paired");
            assert_eq!(reply.header("cache-control"), Some("no-store"));
            assert_security_headers(&reply);
        }
    }
    assert_eq!(get(&remote, "/api/state", Some(&paired)).status, 200);
    assert!(remote.received.lock().unwrap().is_empty());
}

#[test]
fn each_endpoint_answers_and_hands_over_exactly_what_was_posted() {
    let remote = start("endpoints");
    let token = pair(&remote);
    let token = Some(token.as_str());

    let early = get(&remote, "/api/state", token);
    assert_eq!(early.status, 503);
    assert_eq!(early.error(), "not_ready");

    assert_eq!(remote.server.publish(sample()), 1);
    assert_eq!(
        remote.server.publish(sample()),
        1,
        "the same snapshot keeps its revision"
    );
    let state = get(&remote, "/api/state", token);
    assert_eq!(state.status, 200);
    assert_eq!(state.header("cache-control"), Some("no-store"));
    let value = state.json();
    assert_eq!(value["revision"], 1);
    assert_eq!(value["computer_name"], "Den Mac");
    assert_eq!(value["speakers"][1]["notice"], "Not on the network");

    let reply = post(
        &remote,
        "/api/speakers/kitchen",
        token,
        "{\"volume\": 55, \"on\": false}",
    );
    assert_eq!(reply.status, 202, "{}", reply.body);
    let reply = post(
        &remote,
        "/api/speakers/living%20room",
        token,
        "{\"delay_ms\": 1000}",
    );
    assert_eq!(reply.status, 202, "{}", reply.body);
    let reply = post(&remote, "/api/speakers/kitchen", token, "{\"delay_ms\": 0}");
    assert_eq!(reply.status, 202, "{}", reply.body);
    let reply = post(&remote, "/api/master", token, "{\"volume\": 0}");
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(
        *remote.received.lock().unwrap(),
        [
            Command::Speaker {
                id: "kitchen".to_owned(),
                change: SpeakerChange {
                    on: Some(false),
                    volume: Some(55.0),
                    delay_ms: None,
                },
            },
            Command::Speaker {
                id: "living room".to_owned(),
                change: SpeakerChange {
                    delay_ms: Some(1000),
                    ..SpeakerChange::default()
                },
            },
            Command::Speaker {
                id: "kitchen".to_owned(),
                change: SpeakerChange {
                    delay_ms: Some(0),
                    ..SpeakerChange::default()
                },
            },
            Command::Master { volume: 0.0 },
        ]
    );
    remote.received.lock().unwrap().clear();

    for (path, body, status, error) in [
        ("/api/speakers/kitchen", "{\"volume\": ", 400, "bad_json"),
        ("/api/speakers/kitchen", "[1]", 400, "bad_json"),
        ("/api/speakers/kitchen", "{\"on\": 1}", 400, "bad_json"),
        ("/api/speakers/kitchen", "{}", 400, "nothing_to_change"),
        (
            "/api/speakers/attic",
            "{\"on\": true}",
            404,
            "unknown_speaker",
        ),
        (
            "/api/speakers/bad%zz",
            "{\"on\": true}",
            404,
            "unknown_speaker",
        ),
        (
            "/api/speakers/den",
            "{\"on\": true}",
            409,
            "speaker_unavailable",
        ),
        (
            "/api/speakers/kitchen",
            "{\"volume\": 100.5}",
            422,
            "out_of_range",
        ),
        (
            "/api/speakers/kitchen",
            "{\"volume\": -1}",
            422,
            "out_of_range",
        ),
        (
            "/api/speakers/kitchen",
            "{\"delay_ms\": 3001}",
            422,
            "out_of_range",
        ),
        (
            "/api/speakers/kitchen",
            "{\"delay_ms\": -1}",
            422,
            "out_of_range",
        ),
        (
            "/api/speakers/living%20room",
            "{\"delay_ms\": 1001}",
            422,
            "out_of_range",
        ),
        ("/api/master", "{\"volume\": 101}", 422, "out_of_range"),
        ("/api/master", "{}", 400, "nothing_to_change"),
        ("/api/master", "{\"volume\": \"loud\"}", 400, "bad_json"),
        ("/api/nothing", "{}", 404, "not_found"),
    ] {
        let reply = post(&remote, path, token, body);
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (status, error),
            "{path} {body}"
        );
    }
    let delay = post(
        &remote,
        "/api/speakers/kitchen",
        token,
        "{\"delay_ms\": 3001}",
    );
    assert_eq!(delay.json()["message"], "Delay must be from 0 to 3000 ms.");
    let wrong_method = get(&remote, "/api/master", token);
    assert_eq!(wrong_method.status, 405);
    assert_eq!(wrong_method.header("allow"), Some("POST"));
    assert!(remote.received.lock().unwrap().is_empty());

    let oversized = format!("{{\"on\": true, \"pad\": \"{}\"}}", "x".repeat(5000));
    assert_eq!(
        post(&remote, "/api/speakers/kitchen", token, &oversized).status,
        413
    );

    let unpaired = post(&remote, "/api/unpair", token, "{}");
    assert_eq!(unpaired.status, 200);
    assert!(unpaired.header("set-cookie").unwrap().contains("Max-Age=0"));
    assert_eq!(get(&remote, "/api/state", token).status, 401);
    assert_eq!(remote.server.device_count(), 0);
}

/// An open event stream and what it has delivered so far.
struct Events {
    stream: TcpStream,
    data: String,
}

impl Events {
    fn open(remote: &Remote, token: &str) -> Events {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, remote.server.port())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let raw = format!(
            "GET /api/events HTTP/1.1\r\nHost: {}\r\nCookie: as_token={token}\r\nAccept: text/event-stream\r\n\r\n",
            host(remote)
        );
        stream.write_all(raw.as_bytes()).unwrap();
        Events {
            stream,
            data: String::new(),
        }
    }

    /// Reads for `time`; false when the server closed the stream.
    fn read_for(&mut self, time: Duration) -> bool {
        let until = Instant::now() + time;
        let mut buffer = [0u8; 4096];
        while Instant::now() < until {
            match self.stream.read(&mut buffer) {
                Ok(0) => return false,
                Ok(count) => self
                    .data
                    .push_str(&String::from_utf8_lossy(&buffer[..count])),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return false,
            }
        }
        true
    }

    /// Reads until `done` holds, for 5 s at most, so a slow runner only waits longer.
    fn read_until(&mut self, done: impl Fn(&Events) -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !done(self) && Instant::now() < until {
            assert!(
                self.read_for(Duration::from_millis(50)),
                "the stream closed"
            );
        }
    }

    fn head(&self) -> &str {
        self.data.split("\r\n\r\n").next().unwrap_or_default()
    }

    fn revisions(&self) -> Vec<u64> {
        let Some((_, body)) = self.data.split_once("\r\n\r\n") else {
            return Vec::new();
        };
        body.split("\n\n")
            .filter(|block| block.starts_with("event: state\n"))
            .map(|block| {
                let id: u64 = block
                    .lines()
                    .nth(1)
                    .unwrap()
                    .strip_prefix("id: ")
                    .unwrap()
                    .parse()
                    .unwrap();
                let data = block
                    .lines()
                    .nth(2)
                    .unwrap()
                    .strip_prefix("data: ")
                    .unwrap();
                let value: Value = serde_json::from_str(data).unwrap();
                assert_eq!(value["revision"], id);
                id
            })
            .collect()
    }

    fn heartbeats(&self) -> usize {
        self.data.matches(": heartbeat\n\n").count()
    }
}

#[test]
fn the_event_stream_delivers_each_revision_once_and_heartbeats() {
    let timing = Timing {
        heartbeat: Duration::from_millis(300),
        ..Timing::default()
    };
    let remote = start_with(config("events", timing));
    let token = pair(&remote);
    remote.server.publish(sample());
    let mut events = Events::open(&remote, &token);
    events.read_until(|events| !events.revisions().is_empty());
    assert!(events.head().starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(
        events
            .head()
            .contains("\r\nContent-Type: text/event-stream\r\n")
    );
    assert!(
        events
            .head()
            .contains("\r\nX-Content-Type-Options: nosniff\r\n")
    );
    assert_eq!(events.revisions(), [1]);

    let mut state = sample();
    for volume in [10.0, 20.0, 30.0] {
        state.master_volume = volume;
        remote.server.publish(state.clone());
    }
    remote.server.publish(state.clone());
    events.read_until(|events| events.revisions().len() >= 4);
    assert_eq!(events.revisions(), [1, 2, 3, 4]);

    events.read_until(|events| events.heartbeats() >= 1);
    assert!(events.heartbeats() >= 1, "{:?}", events.data);
    assert!(events.read_for(Duration::from_millis(300)));
    assert_eq!(events.revisions(), [1, 2, 3, 4], "a revision came twice");

    // A second stream starts from the current snapshot.
    let mut late = Events::open(&remote, &token);
    late.read_until(|events| !events.revisions().is_empty());
    assert!(late.read_for(Duration::from_millis(200)));
    assert_eq!(late.revisions(), [4]);

    // Unpairing ends the device's streams.
    assert_eq!(post(&remote, "/api/unpair", Some(&token), "{}").status, 200);
    assert!(
        !events.read_for(Duration::from_secs(2)),
        "the stream stays open"
    );
    assert!(
        !late.read_for(Duration::from_secs(2)),
        "the stream stays open"
    );
    assert!(!events.data.contains("event: replaced"));
    assert!(!late.data.contains("event: replaced"));
}

#[test]
fn a_device_keeps_two_event_streams_and_a_third_ends_the_oldest() {
    let remote = start("streams");
    let token = pair(&remote);
    remote.server.publish(sample());
    // Each waits for its first snapshot, so the server registered it before the next opens.
    let mut streams: Vec<Events> = Vec::new();
    for _ in 0..3 {
        let mut events = Events::open(&remote, &token);
        events.read_until(|events| !events.revisions().is_empty());
        streams.push(events);
    }
    assert!(
        !streams[0].read_for(Duration::from_secs(2)),
        "the oldest stays open"
    );
    // A page that knows it was replaced stops reconnecting, which would end the next stream.
    assert!(
        streams[0].data.ends_with("event: replaced\ndata: {}\n\n"),
        "{:?}",
        streams[0].data
    );
    let mut state = sample();
    state.master_volume = 5.0;
    remote.server.publish(state);
    for events in &mut streams[1..] {
        events.read_until(|events| events.revisions().len() >= 2);
        assert_eq!(events.revisions(), [1, 2]);
    }
}

fn idle_connection(remote: &Remote) -> TcpStream {
    TcpStream::connect((Ipv4Addr::LOCALHOST, remote.server.port())).unwrap()
}

fn read_all(stream: &mut TcpStream) -> String {
    // 5 s ran out once on a busy PC before the server's answer and close arrived.
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut data = Vec::new();
    // A reset after the server closed counts as closed.
    let _ = stream.read_to_end(&mut data);
    String::from_utf8_lossy(&data).into_owned()
}

#[test]
fn one_address_holds_eight_connections_at_most() {
    let remote = start("per-peer");
    let idle: Vec<TcpStream> = (0..8).map(|_| idle_connection(&remote)).collect();
    // Sends nothing: a request left unread could reset the connection before the answer arrives.
    let mut ninth = idle_connection(&remote);
    let answer = read_all(&mut ninth);
    assert!(answer.starts_with("HTTP/1.1 503 "), "{answer:?}");
    let mut turned_away = false;
    while let Some(event) = remote.server.poll_event() {
        turned_away |= matches!(event, ServerEvent::PeerBusy { .. });
    }
    assert!(turned_away);

    drop(idle);
    let until = Instant::now() + Duration::from_secs(5);
    while get(&remote, "/", None).status != 200 {
        assert!(Instant::now() < until, "the closed connections still count");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_head_over_8_kib_gets_431() {
    let remote = start("head-too-large");
    let raw = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nX-Padding: {}\r\n\r\n",
        host(&remote),
        "a".repeat(9 * 1024)
    );
    let reply = exchange(remote.server.port(), raw.as_bytes());
    assert_eq!(reply.status, 431);
    assert_security_headers(&reply);
}

#[test]
fn a_chunked_body_is_refused() {
    let remote = start("chunked");
    let host = host(&remote);
    let raw = format!(
        "POST /api/pair HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
    );
    let reply = exchange(remote.server.port(), raw.as_bytes());
    assert_eq!(reply.status, 400);
    assert_eq!(remote.server.device_count(), 0);
}

#[test]
fn an_unreadable_body_length_gets_a_json_400_and_pairs_nothing() {
    let remote = start("body-length");
    let host = host(&remote);
    let code = remote.server.pairing_code().unwrap();
    let body = json!({ "code": code }).to_string();
    for lengths in [
        format!("Content-Length: {0}\r\nContent-Length: {0}\r\n", body.len()),
        "Content-Length: -1\r\n".to_owned(),
        "Content-Length: twelve\r\n".to_owned(),
    ] {
        let raw = format!(
            "POST /api/pair HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: application/json\r\n{lengths}\r\n{body}"
        );
        let reply = exchange(remote.server.port(), raw.as_bytes());
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (400, "bad_request"),
            "{lengths}"
        );
        assert_security_headers(&reply);
    }
    assert_eq!(remote.server.device_count(), 0);
    assert_eq!(remote.server.pairing_code(), Some(code));

    let oversized = format!("{{\"code\": \"{}\"}}", "1".repeat(5000));
    let reply = post(&remote, "/api/pair", None, &oversized);
    assert_eq!((reply.status, reply.error().as_str()), (413, "too_large"));
    let reply = post(&remote, "/api/pair", None, "{}");
    assert_eq!((reply.status, reply.error().as_str()), (400, "bad_json"));
}

fn with_sources() -> State {
    let sources = r#""sources": [
        {"id": "system", "name": "All System Audio", "kind": "system", "available": true, "notice": null},
        {"id": "app:music", "name": "Music", "kind": "app", "available": true, "notice": null},
        {"id": "input:usb", "name": "USB Mic", "kind": "input", "available": false,
         "notice": "Allow microphone access"}
    ], "source_id": "system", "speakers""#;
    State::from_json(&SAMPLE.replacen("\"speakers\"", sources, 1)).unwrap()
}

#[test]
fn a_source_pick_reaches_the_app_and_a_refused_one_says_why() {
    let remote = start("source");
    let token = pair(&remote);
    let token = Some(token.as_str());
    let early = post(&remote, "/api/source", token, "{\"id\": \"system\"}");
    assert_eq!(
        (early.status, early.error().as_str()),
        (404, "unknown_source")
    );

    remote.server.publish(with_sources());
    let state = get(&remote, "/api/state", token).json();
    assert_eq!(state["source_id"], "system");
    assert_eq!(state["sources"][1]["name"], "Music");
    assert_eq!(state["sources"][2]["kind"], "input");
    assert_eq!(state["sources"][2]["available"], false);

    let reply = post(&remote, "/api/source", token, "{\"id\": \"app:music\"}");
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(
        *remote.received.lock().unwrap(),
        [Command::Source {
            id: "app:music".to_owned()
        }]
    );
    remote.received.lock().unwrap().clear();

    for (body, status, error) in [
        ("{\"id\": \"input:usb\"}", 409, "source_unavailable"),
        ("{\"id\": \"app:gone\"}", 404, "unknown_source"),
        ("{\"id\": \"\"}", 404, "unknown_source"),
        ("{\"id\": 3}", 400, "bad_json"),
        ("{\"id\": ", 400, "bad_json"),
        ("{}", 400, "nothing_to_change"),
    ] {
        let reply = post(&remote, "/api/source", token, body);
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (status, error),
            "{body}"
        );
        assert!(!reply.json()["message"].as_str().unwrap().is_empty());
    }
    assert!(remote.received.lock().unwrap().is_empty());
}

#[test]
fn a_speaker_change_needs_a_published_speaker_and_an_id() {
    let remote = start("no-snapshot");
    let token = pair(&remote);
    let token = Some(token.as_str());
    for path in ["/api/speakers/kitchen", "/api/speakers/"] {
        let reply = post(&remote, path, token, "{\"on\": true}");
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (404, "unknown_speaker"),
            "{path}"
        );
    }
    remote.server.publish(sample());
    let reply = post(&remote, "/api/speakers/", token, "{\"on\": true}");
    assert_eq!(reply.error(), "unknown_speaker");
    let reply = post(
        &remote,
        "/api/speakers/kitchen?x=1",
        token,
        "{\"on\": true}",
    );
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(remote.received.lock().unwrap().len(), 1);
}

#[test]
fn reserved_slots_let_in_only_a_paired_device() {
    let remote = start_with(Config {
        capacity: Capacity {
            connections: 4,
            per_peer: 4,
            reserved: 2,
            reserved_per_peer: 1,
        },
        ..config("reserved", Timing::default())
    });
    remote.server.publish(sample());
    let token = pair(&remote);
    // Accepted in order, so both are counted before the next connection is.
    let _slow: Vec<TcpStream> = (0..2).map(|_| idle_connection(&remote)).collect();
    let unpaired = get(&remote, "/", None);
    assert_eq!(unpaired.status, 503);
    assert_eq!(unpaired.error(), "busy");
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        // The connections answered just before may not have ended yet.
        let reply = get(&remote, "/api/state", Some(&token));
        if reply.status == 200 {
            break;
        }
        assert_eq!(reply.status, 503, "{}", reply.body);
        assert!(Instant::now() < until, "a paired device stayed out");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_full_server_makes_room_by_closing_the_oldest_reserved_waiter() {
    let config = config("reserved-evict", Timing::default());
    // Paired on a first server, so no pairing connection is still held by the second.
    let first = start_with(config.clone());
    let token = pair(&first);
    first.server.stop();
    let capacity = Capacity {
        connections: 2,
        per_peer: 3,
        reserved: 1,
        reserved_per_peer: 2,
    };
    for round in 1.. {
        let remote = start_with(Config {
            capacity,
            ..config.clone()
        });
        remote.server.publish(sample());
        // Accepted in order: the first takes the shared slot, the second the reserved one.
        let _shared = idle_connection(&remote);
        let opened = Instant::now();
        let mut waiter = idle_connection(&remote);
        let reply = get(&remote, "/api/state", Some(&token));
        assert_eq!(reply.status, 200, "{}", reply.body);
        // Closed unanswered; a waiter left in place would get a 503 after 1 s.
        let answer = read_all(&mut waiter);
        if answer.is_empty() {
            break;
        }
        // On a starved run the waiter's 1 s ran out before the paired request came.
        assert!(opened.elapsed() >= Duration::from_secs(1), "{answer:?}");
        assert!(round < 5, "every round outlasted the waiter's time");
    }
}

#[test]
fn a_busy_answer_arrives_after_a_request_the_server_never_read() {
    let timing = Timing {
        // The idle connections must outlast every round on a slow run.
        request: Duration::from_secs(60),
        ..Timing::default()
    };
    let remote = start_with(config("busy-linger", timing));
    let _idle: Vec<TcpStream> = (0..8).map(|_| idle_connection(&remote)).collect();
    let raw = format!(
        "GET /api/state HTTP/1.1\r\nHost: {}\r\nX-Padding: {}\r\n\r\n",
        host(&remote),
        "a".repeat(4096)
    );
    for round in 0..10 {
        let mut ninth = idle_connection(&remote);
        ninth.write_all(raw.as_bytes()).unwrap();
        ninth
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut answer = Vec::new();
        // A reset instead of a close can cost a browser the answer.
        let ended = ninth.read_to_end(&mut answer);
        let answer = String::from_utf8_lossy(&answer);
        assert!(ended.is_ok(), "round {round}: {ended:?} after {answer:?}");
        assert!(
            answer.starts_with("HTTP/1.1 503 "),
            "round {round}: {answer:?}"
        );
    }
}

#[test]
fn a_request_that_takes_too_long_is_closed_unanswered() {
    let timing = Timing {
        request: Duration::from_millis(300),
        ..Timing::default()
    };
    let remote = start_with(config("slow", timing));
    let mut stream = idle_connection(&remote);
    let started = Instant::now();
    stream.write_all(b"GET / HTTP/1.1\r\nHo").unwrap();
    thread::sleep(Duration::from_millis(150));
    // A byte now and then doesn't buy more time.
    let _ = stream.write_all(b"s");
    let answer = read_all(&mut stream);
    assert_eq!(answer, "");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn devices_survive_a_restart_and_can_all_be_forgotten() {
    let config = config("restart", Timing::default());
    let first = start_with(config.clone());
    let tokens: Vec<String> = (0..9).map(|_| pair(&first)).collect();
    assert_eq!(first.server.device_count(), 8);
    first.server.stop();

    let file = fs::read_to_string(config.devices_path.as_ref().unwrap()).unwrap();
    assert!(tokens.iter().all(|token| !file.contains(token.as_str())));
    let second = start_with(config.clone());
    second.server.publish(sample());
    assert_eq!(second.server.device_count(), 8);
    assert_eq!(
        get(&second, "/api/state", Some(&tokens[0])).status,
        401,
        "the oldest was dropped"
    );
    for token in &tokens[1..] {
        assert_eq!(get(&second, "/api/state", Some(token)).status, 200);
    }

    let mut events = Events::open(&second, &tokens[5]);
    events.read_until(|events| !events.revisions().is_empty());
    second.server.forget_all().unwrap();
    assert_eq!(second.server.device_count(), 0);
    assert!(
        !events.read_for(Duration::from_secs(2)),
        "the stream stays open"
    );
    for token in &tokens {
        assert_eq!(get(&second, "/api/state", Some(token)).status, 401);
    }
    second.server.stop();
    let third = start_with(config);
    assert_eq!(third.server.device_count(), 0);
}

#[test]
fn after_stop_the_port_is_free_and_no_command_arrives() {
    let remote = start("stop");
    let token = pair(&remote);
    remote.server.publish(sample());
    let port = remote.server.port();
    let received = Arc::clone(&remote.received);
    remote.server.stop();
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
    let again = start_with(Config {
        port,
        ..config("stop-again", Timing::default())
    });
    assert_eq!(again.server.port(), port);
    assert_eq!(get(&again, "/api/state", Some(&token)).status, 401);
    assert!(received.lock().unwrap().is_empty());
}

#[test]
fn a_busy_port_is_reported_as_such() {
    let first = start("busy");
    let taken = RemoteServer::start(
        Config {
            port: first.server.port(),
            ..config("busy-second", Timing::default())
        },
        |_: &Command| {},
    );
    assert!(taken.unwrap_err().port_in_use());
}

fn pairing_code_lines(remote: &Remote) -> Vec<String> {
    std::iter::from_fn(|| remote.server.poll_event())
        .map(|event| event.to_string())
        .filter(|line| line.contains("pairing code"))
        .collect()
}

#[test]
fn debug_mode_logs_each_new_pairing_code_and_a_normal_run_never_does() {
    let line = |code: &str| format!("remote: pairing code {code} (debug only)");
    let remote = start_with(Config {
        log_pairing_codes: true,
        ..config("debug-codes", Timing::default())
    });
    let first = remote.server.pairing_code().unwrap();
    assert_eq!(pairing_code_lines(&remote), [line(&first)]);
    let rotated = remote.server.rotate_code().unwrap();
    assert_eq!(pairing_code_lines(&remote), [line(&rotated)]);
    pair(&remote);
    let after_use = remote.server.pairing_code().unwrap();
    assert_ne!(after_use, rotated, "single use");
    assert_eq!(pairing_code_lines(&remote), [line(&after_use)]);

    let quiet = start("quiet-codes");
    let codes = [
        quiet.server.pairing_code().unwrap(),
        quiet.server.rotate_code().unwrap(),
    ];
    pair(&quiet);
    let events: Vec<String> = std::iter::from_fn(|| quiet.server.poll_event())
        .map(|event| event.to_string())
        .collect();
    assert!(
        events.iter().all(|line| !line.contains("pairing code")
            && codes.iter().all(|code| !line.contains(code.as_str()))),
        "{events:?}"
    );
}

#[test]
fn the_pairing_code_switch_reads_from_json_and_is_off_by_default() {
    assert!(!Config::default().log_pairing_codes);
    assert!(!Config::from_json("{}").unwrap().log_pairing_codes);
    assert!(
        Config::from_json(r#"{"log_pairing_codes": true}"#)
            .unwrap()
            .log_pairing_codes
    );
    assert!(Config::from_json(r#"{"log_pairing_codes": 1}"#).is_none());
}

/// Any method, with the headers a same-origin JSON request carries.
fn send(remote: &Remote, method: &str, path: &str, token: Option<&str>, body: &str) -> Reply {
    let host = host(remote);
    let cookie = token.map_or(String::new(), |token| {
        format!("Cookie: as_token={token}\r\n")
    });
    let raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: application/json\r\n{cookie}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    exchange(remote.server.port(), raw.as_bytes())
}

// The endpoint table: each route answers one method; any other gets 405 and its Allow.
#[test]
fn every_route_answers_a_wrong_method_with_405_and_its_allow() {
    let remote = start("methods");
    remote.server.publish(sample());
    let token = pair(&remote);
    let token = Some(token.as_str());
    for (method, path, allow) in [
        ("PUT", "/api/state", "GET"),
        ("POST", "/api/state", "GET"),
        ("DELETE", "/api/events", "GET"),
        ("POST", "/api/events", "GET"),
        ("PUT", "/api/pair", "POST"),
        ("GET", "/api/unpair", "POST"),
        ("DELETE", "/api/unpair", "POST"),
        ("GET", "/api/speakers/kitchen", "POST"),
        ("PATCH", "/api/speakers/kitchen", "POST"),
        ("PUT", "/api/master", "POST"),
        ("GET", "/api/source", "POST"),
        ("DELETE", "/", "GET"),
        ("PUT", "/manifest.webmanifest", "GET"),
        ("POST", "/icon-180.png", "GET"),
    ] {
        let reply = send(&remote, method, path, token, "{\"volume\": 5}");
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (405, "method_not_allowed"),
            "{method} {path}"
        );
        assert_eq!(reply.header("allow"), Some(allow), "{method} {path}");
        assert_eq!(reply.header("cache-control"), Some("no-store"));
        assert_security_headers(&reply);
    }
    assert!(remote.received.lock().unwrap().is_empty());
    assert_eq!(remote.server.device_count(), 1, "DELETE unpaired nothing");
    assert_eq!(get(&remote, "/api/state", token).status, 200);
}

// The 4 KiB body limit: a body at it is read, one byte more gets 413 (RFC 9110 §15.5.14).
#[test]
fn a_body_at_the_limit_is_read_and_one_byte_more_gets_413() {
    use audiosync_remote::http::MAX_BODY;

    let remote = start("body-limit");
    let code = remote.server.pairing_code().unwrap();
    let pad = |body: String, length: usize| format!("{body}{}", " ".repeat(length - body.len()));
    let over = pad(json!({ "code": code }).to_string(), MAX_BODY + 1);
    let reply = post(&remote, "/api/pair", None, &over);
    assert_eq!((reply.status, reply.error().as_str()), (413, "too_large"));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert_security_headers(&reply);
    assert_eq!(
        remote.server.pairing_code(),
        Some(code.clone()),
        "not used up"
    );
    assert_eq!(remote.server.device_count(), 0);

    let at = pad(json!({ "code": code }).to_string(), MAX_BODY);
    let reply = post(&remote, "/api/pair", None, &at);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(remote.server.device_count(), 1);
}

#[test]
fn a_declared_length_over_the_limit_is_refused_before_the_body_arrives() {
    let remote = start("declared-length");
    let host = host(&remote);
    let raw = format!(
        "POST /api/pair HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: application/json\r\nContent-Length: 1000000\r\n\r\n{{"
    );
    let asked = Instant::now();
    let reply = exchange(remote.server.port(), raw.as_bytes());
    assert_eq!((reply.status, reply.error().as_str()), (413, "too_large"));
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "answered without waiting for the body: {:?}",
        asked.elapsed()
    );
    assert_eq!(remote.server.device_count(), 0);
}

// Only the listed paths exist; anything else is 404, paired or not, and runs nothing.
#[test]
fn unknown_paths_get_a_json_404_with_or_without_a_token() {
    let remote = start("unknown-paths");
    remote.server.publish(sample());
    let token = pair(&remote);
    for token in [None, Some(token.as_str())] {
        for path in [
            "/api",
            "/api/",
            "/API/state",
            "/api/state/",
            "/api/statex",
            "/api/speakers",
            "/api/pair/again",
            "/index.html",
            "/../api/state",
            "/icon-180.png/",
            "//",
        ] {
            let reply = get(&remote, path, token);
            assert_eq!(
                (reply.status, reply.error().as_str()),
                (404, "not_found"),
                "{path} {token:?}"
            );
            assert_security_headers(&reply);
        }
        let posted = post(&remote, "/api/master/", token, "{\"volume\": 5}");
        assert_eq!(posted.status, 404, "{token:?}");
    }
    assert!(remote.received.lock().unwrap().is_empty());
}

// DNS rebinding: a foreign Host is refused before any route, method or token
// check, even when the Origin matches that Host.
#[test]
fn a_foreign_host_is_refused_first_even_with_a_matching_origin_and_a_token() {
    let remote = start("rebinding");
    remote.server.publish(sample());
    let token = pair(&remote);
    let code = remote.server.pairing_code().unwrap();
    let port = remote.server.port();
    let foreign = format!("evil.example:{port}");
    let ask = |method: &str, path: &str, body: &str| {
        let raw = format!(
            "{method} {path} HTTP/1.1\r\nHost: {foreign}\r\nOrigin: http://{foreign}\r\nContent-Type: application/json\r\nCookie: as_token={token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        exchange(port, raw.as_bytes())
    };
    let pairing = json!({ "code": code }).to_string();
    for (method, path, body) in [
        ("POST", "/api/pair", pairing.as_str()),
        ("POST", "/api/master", "{\"volume\": 5}"),
        ("POST", "/api/speakers/kitchen", "{\"on\": false}"),
        ("POST", "/api/unpair", "{}"),
        ("GET", "/api/state", ""),
        ("GET", "/api/events", ""),
        ("PUT", "/api/state", ""),
        ("GET", "/nothing", ""),
    ] {
        let reply = ask(method, path, body);
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (403, "bad_host"),
            "{method} {path}"
        );
        assert_security_headers(&reply);
    }
    assert!(remote.received.lock().unwrap().is_empty());
    assert_eq!(remote.server.pairing_code(), Some(code));
    assert_eq!(remote.server.device_count(), 1);

    // A Host without a port names port 80 (RFC 9110 §7.2); an empty one names nothing.
    for line in [
        "Host: 127.0.0.1\r\n",
        "Host: \r\n",
        "Host: kitchen.local\r\n",
    ] {
        let raw = format!("GET / HTTP/1.1\r\n{line}\r\n");
        assert_eq!(exchange(port, raw.as_bytes()).status, 403, "{line:?}");
    }
    // Header names are case-insensitive (RFC 9110 §5.1).
    let lower = format!("GET / HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n");
    assert_eq!(exchange(port, lower.as_bytes()).status, 200);
}

fn zoned(calibrating: bool) -> State {
    let json = SAMPLE.replacen(
        "\"speakers\": [",
        &format!(
            r#""calibrating": {calibrating},
    "zones": [
        {{"id": "zone 1", "name": "Downstairs", "volume": 80, "members": ["kitchen", "living room"],
         "counts": {{"on": 2, "total": 2, "waiting": 0}}}},
        {{"id": "attic", "name": "Attic", "volume": 100, "members": ["den"],
         "counts": {{"on": 0, "total": 1, "waiting": 0}}}}
    ],
    "presets": [
        {{"id": "preset:1", "name": "Evening", "current": true}},
        {{"id": "party time", "name": "Party", "current": false}}
    ],
    "speakers": ["#
        ),
        1,
    );
    State::from_json(&json).unwrap()
}

fn zone_command(id: &str, toggle: bool, volume: Option<f64>) -> Command {
    Command::Zone {
        id: id.to_owned(),
        change: ZoneChange { toggle, volume },
    }
}

// covers: S256, S257
#[test]
fn zone_toggle_and_volume_routes_are_accepted() {
    let remote = start("zone-routes");
    let token = pair(&remote);
    let token = Some(token.as_str());
    remote.server.publish(zoned(false));
    let state = get(&remote, "/api/state", token).json();
    assert_eq!(
        state["zones"][0]["members"],
        json!(["kitchen", "living room"])
    );
    assert_eq!(state["zones"][1]["counts"]["total"], 1);
    assert_eq!(state["calibrating"], false);

    for body in [
        "{\"toggle\": true}",
        "{\"volume\": 0}",
        "{\"toggle\": true, \"volume\": 100}",
    ] {
        let reply = post(&remote, "/api/zones/zone%201", token, body);
        assert_eq!(reply.status, 202, "{body}: {}", reply.body);
        assert_eq!(reply.json(), json!({"accepted": true}));
    }
    let reply = post(&remote, "/api/zones/attic", token, "{\"volume\": 40}");
    assert_eq!(
        reply.status, 202,
        "an empty zone still takes a volume: {}",
        reply.body
    );
    assert_eq!(
        *remote.received.lock().unwrap(),
        [
            zone_command("zone 1", true, None),
            zone_command("zone 1", false, Some(0.0)),
            zone_command("zone 1", true, Some(100.0)),
            zone_command("attic", false, Some(40.0)),
        ]
    );
    for method in ["GET", "PUT", "DELETE"] {
        for path in ["/api/zones/attic", "/api/presets/preset:1"] {
            let reply = send(&remote, method, path, token, "{\"toggle\": true}");
            assert_eq!(reply.status, 405, "{method} {path}");
            assert_eq!(reply.header("allow"), Some("POST"));
        }
    }
    assert_eq!(remote.received.lock().unwrap().len(), 4);
}

// covers: S256, S257
#[test]
fn preset_recall_route_is_accepted() {
    let remote = start("preset-routes");
    let token = pair(&remote);
    let token = Some(token.as_str());
    remote.server.publish(zoned(false));
    let state = get(&remote, "/api/state", token).json();
    assert_eq!(
        state["presets"],
        json!([
            {"id": "preset:1", "name": "Evening", "current": true},
            {"id": "party time", "name": "Party", "current": false}
        ])
    );
    let reply = post(
        &remote,
        "/api/presets/party%20time",
        token,
        "{\"recall\": true}",
    );
    assert_eq!(reply.status, 202, "{}", reply.body);
    let reply = post(
        &remote,
        "/api/presets/preset%3A1",
        token,
        "{\"recall\": true}",
    );
    assert_eq!(
        reply.status, 202,
        "the current one recalls too: {}",
        reply.body
    );
    assert_eq!(
        *remote.received.lock().unwrap(),
        [
            Command::Preset {
                id: "party time".to_owned()
            },
            Command::Preset {
                id: "preset:1".to_owned()
            },
        ]
    );
}

// covers: S257
#[test]
fn unknown_zone_and_unknown_preset_answer_404() {
    let remote = start("zone-unknown");
    let token = pair(&remote);
    let token = Some(token.as_str());
    let early = post(&remote, "/api/zones/attic", token, "{\"toggle\": true}");
    assert_eq!(
        (early.status, early.error().as_str()),
        (404, "unknown_zone")
    );
    remote.server.publish(zoned(false));
    for (path, body, error) in [
        ("/api/zones/cellar", "{\"toggle\": true}", "unknown_zone"),
        ("/api/zones/", "{\"toggle\": true}", "unknown_zone"),
        ("/api/zones/zone%2", "{\"volume\": 5}", "unknown_zone"),
        ("/api/zones/kitchen", "{\"toggle\": true}", "unknown_zone"),
        (
            "/api/presets/preset:9",
            "{\"recall\": true}",
            "unknown_preset",
        ),
        ("/api/presets/", "{\"recall\": true}", "unknown_preset"),
        ("/api/presets/%FF", "{\"recall\": true}", "unknown_preset"),
    ] {
        let reply = post(&remote, path, token, body);
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (404, error),
            "{path}"
        );
        assert_eq!(reply.header("cache-control"), Some("no-store"));
    }
    // A zone or preset the app no longer publishes is gone for the page too.
    remote.server.publish(sample());
    let reply = post(
        &remote,
        "/api/presets/preset:1",
        token,
        "{\"recall\": true}",
    );
    assert_eq!(reply.error(), "unknown_preset");
    assert!(remote.received.lock().unwrap().is_empty());
}

// covers: S257
#[test]
fn a_zone_with_no_member_that_can_play_answers_409() {
    let remote = start("zone-unavailable");
    let token = pair(&remote);
    let token = Some(token.as_str());
    remote.server.publish(zoned(false));
    let reply = post(&remote, "/api/zones/attic", token, "{\"toggle\": true}");
    assert_eq!(
        (reply.status, reply.error().as_str()),
        (409, "zone_unavailable")
    );
    let reply = post(
        &remote,
        "/api/zones/attic",
        token,
        "{\"toggle\": true, \"volume\": 10}",
    );
    assert_eq!(reply.status, 409, "the volume goes with the refused toggle");
    assert!(remote.received.lock().unwrap().is_empty());
}

// covers: S257
#[test]
fn bad_bodies_answer_400_and_out_of_range_422() {
    let remote = start("zone-bodies");
    let token = pair(&remote);
    let token = Some(token.as_str());
    remote.server.publish(zoned(false));
    for (path, body, status, error) in [
        ("/api/zones/attic", "{\"toggle\": ", 400, "bad_json"),
        ("/api/zones/attic", "[true]", 400, "bad_json"),
        ("/api/zones/attic", "{\"toggle\": 1}", 400, "bad_json"),
        ("/api/zones/attic", "{\"volume\": \"5\"}", 400, "bad_json"),
        ("/api/zones/attic", "{}", 400, "nothing_to_change"),
        (
            "/api/zones/attic",
            "{\"toggle\": false}",
            400,
            "nothing_to_change",
        ),
        ("/api/zones/cellar", "{}", 400, "nothing_to_change"),
        (
            "/api/zones/attic",
            "{\"volume\": 100.5}",
            422,
            "out_of_range",
        ),
        (
            "/api/zones/zone%201",
            "{\"volume\": -1}",
            422,
            "out_of_range",
        ),
        (
            "/api/presets/preset:1",
            "{\"recall\": \"yes\"}",
            400,
            "bad_json",
        ),
        ("/api/presets/preset:1", "not json", 400, "bad_json"),
        (
            "/api/presets/preset:1",
            "{\"recall\": false}",
            400,
            "nothing_to_change",
        ),
        ("/api/presets/preset:1", "{}", 400, "nothing_to_change"),
    ] {
        let reply = post(&remote, path, token, body);
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (status, error),
            "{path} {body}"
        );
    }
    let range = post(&remote, "/api/zones/attic", token, "{\"volume\": 101}");
    assert_eq!(range.json()["message"], "Volume must be from 0 to 100.");
    assert!(remote.received.lock().unwrap().is_empty());
}

// covers: S257
#[test]
fn a_body_over_4_kib_answers_413() {
    use audiosync_remote::http::MAX_BODY;

    let remote = start("zone-body-limit");
    let token = pair(&remote);
    let token = Some(token.as_str());
    remote.server.publish(zoned(true));
    let pad = |body: &str, length: usize| format!("{body}{}", " ".repeat(length - body.len()));
    for path in ["/api/zones/attic", "/api/presets/preset:1"] {
        let reply = post(&remote, path, token, &pad("{\"volume\": 5}", MAX_BODY + 1));
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (413, "too_large"),
            "the limit comes before the calibrating answer: {path}"
        );
    }
    remote.server.publish(zoned(false));
    let at = pad("{\"volume\": 5}", MAX_BODY);
    let reply = post(&remote, "/api/zones/attic", token, &at);
    assert_eq!(reply.status, 202, "{}", reply.body);
}

// covers: S256, S258
#[test]
fn every_write_answers_409_while_calibrating() {
    let remote = start("calibrating");
    let token = pair(&remote);
    let token = Some(token.as_str());
    remote.server.publish(zoned(true));
    assert_eq!(
        get(&remote, "/api/state", token).json()["calibrating"],
        true
    );
    for (path, body) in [
        ("/api/master", "{\"volume\": 5}"),
        ("/api/speakers/kitchen", "{\"on\": false}"),
        ("/api/speakers/den", "{\"on\": true}"),
        ("/api/speakers/attic", "{}"),
        ("/api/zones/zone%201", "{\"toggle\": true}"),
        ("/api/zones/attic", "{\"toggle\": true}"),
        ("/api/zones/cellar", "not json"),
        ("/api/presets/preset:1", "{\"recall\": true}"),
        ("/api/presets/gone", "{\"recall\": 7}"),
        ("/api/master", "{\"volume\": 500}"),
    ] {
        let reply = post(&remote, path, token, body);
        assert_eq!(
            (reply.status, reply.error().as_str()),
            (409, "calibrating"),
            "{path} {body}"
        );
        assert_eq!(
            reply.json()["message"],
            "AudioSync is calibrating. Try again when it's done."
        );
    }
    let unpaired = post(&remote, "/api/master", None, "{\"volume\": 5}");
    assert_eq!(
        unpaired.status, 401,
        "only a paired phone learns of the run"
    );
    let wrong_method = send(&remote, "GET", "/api/zones/attic", token, "");
    assert_eq!(wrong_method.status, 405);
    assert!(remote.received.lock().unwrap().is_empty());

    remote.server.publish(zoned(false));
    let reply = post(&remote, "/api/master", token, "{\"volume\": 5}");
    assert_eq!(reply.status, 202, "{}", reply.body);
}

// covers: S258
#[test]
fn pairing_and_unpair_stay_open_while_calibrating() {
    let remote = start("calibrating-pairing");
    remote.server.publish(zoned(true));
    let token = pair(&remote);
    let state = get(&remote, "/api/state", Some(&token));
    assert_eq!(state.status, 200, "reading the state stays open");
    let reply = post(&remote, "/api/unpair", Some(&token), "{}");
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(remote.server.device_count(), 0);
}
