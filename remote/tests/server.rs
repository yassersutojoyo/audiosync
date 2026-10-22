//! The remote over real sockets on 127.0.0.1, which counts as a private peer.

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_remote::page::{self, hash_source};
use audiosync_remote::{Command, Config, RemoteServer, SpeakerChange, State, Timing};
use serde_json::{Value, json};

const SAMPLE: &str = r#"{
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
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(raw).unwrap();
    let mut data = Vec::new();
    stream.read_to_end(&mut data).unwrap();
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
fn wrong_codes_are_limited_then_locked() {
    let timing = Timing {
        wrong_code_window: Duration::from_secs(1),
        ..Timing::default()
    };
    let remote = start_with(config("limits", timing));
    let code = remote.server.pairing_code().unwrap();
    let right = json!({ "code": code }).to_string();
    let wrong = json!({ "code": wrong(&code) }).to_string();
    for round in 0..4 {
        for _ in 0..5 {
            assert_eq!(
                post(&remote, "/api/pair", None, &wrong).status,
                401,
                "round {round}"
            );
        }
        let limited = post(&remote, "/api/pair", None, &right);
        assert_eq!(limited.status, 429);
        let expected = if round == 3 {
            "pairing_locked"
        } else {
            "too_many_tries"
        };
        assert_eq!(limited.error(), expected);
        thread::sleep(Duration::from_millis(1100));
    }
    assert!(remote.server.pairing_locked());
    let locked = post(&remote, "/api/pair", None, &right);
    assert_eq!(locked.status, 429);
    assert_eq!(locked.error(), "pairing_locked");
    let fresh = remote.server.rotate_code().unwrap();
    assert!(!remote.server.pairing_locked());
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
