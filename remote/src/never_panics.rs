//! No bytes a peer on the network sends make the remote panic or pass its limits.
//!
//! Every parser a peer reaches gets thousands of deterministic inputs: random
//! bytes, valid requests, hosts and bodies cut short or mutated, and edges
//! picked by hand (empty input, lengths that overflow, bytes that aren't
//! UTF-8, broken escapes, other line ends, repeated headers). Each must
//! return a value or an error, and what it accepts stays within the 8 KiB
//! head and 4 KiB body. The inputs come from a fixed-seed xorshift
//! generator, so a failure shows up the same way on every run.

use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::http::{self, MAX_BODY, MAX_HEAD, ReadError, Request};
use crate::net;
use crate::server::percent_decode;
use crate::state;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.next().is_multiple_of(n)
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }

    fn pick<'a, T: ?Sized>(&mut self, items: &[&'a T]) -> &'a T {
        items[self.below(items.len())]
    }
}

/// Mostly bytes from `alphabet`, now and then any byte.
fn text_like(rng: &mut Rng, alphabet: &[u8], max_len: usize) -> Vec<u8> {
    let len = rng.below(max_len + 1);
    (0..len)
        .map(|_| {
            if rng.one_in(6) {
                rng.byte()
            } else {
                alphabet[rng.below(alphabet.len())]
            }
        })
        .collect()
}

fn mutate(rng: &mut Rng, base: &[u8], alphabet: &[u8]) -> Vec<u8> {
    let mut out = base.to_vec();
    for _ in 0..=rng.below(4) {
        let at = rng.below(out.len() + 1);
        match rng.below(6) {
            0 if at < out.len() => out[at] = rng.byte(),
            1 => out.insert(at, alphabet[rng.below(alphabet.len())]),
            2 if at < out.len() => {
                out.remove(at);
            }
            3 => out.truncate(at),
            4 => {
                let end = (at + rng.below(16)).min(out.len());
                let piece = out[at..end].to_vec();
                out.splice(at..at, piece);
            }
            _ => out.insert(at, rng.byte()),
        }
    }
    out
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

const HTTP_BYTES: &[u8] =
    b"\r\n\r\n\r\n:: /?#%;=,\"\t0123456789GET POST HTTP/1.1 Host Content-Length Cookie";

const LENGTHS: &[&str] = &[
    "0",
    "5",
    "4096",
    "4097",
    "+5",
    "-1",
    " 7 ",
    "5, 5",
    "1e3",
    "",
    "18446744073709551615",
    "18446744073709551616",
    "99999999999999999999999999999",
];

fn random_request(rng: &mut Rng) -> Vec<u8> {
    let method = rng.pick(&["GET", "POST", "PUT", "", "G ET"]);
    let target = rng.pick(&[
        "/",
        "/api/state",
        "/api/speakers/den%20x?a=1#f",
        "/%",
        "*",
        "",
        "/a b",
    ]);
    let version = rng.pick(&["HTTP/1.1", "HTTP/1.0", "HTTP/2", "http/1.1", ""]);
    let line_end = if rng.one_in(8) {
        rng.pick(&["\n", "\r", "\r\r\n"])
    } else {
        "\r\n"
    };
    let mut text = format!("{method} {target} {version}");
    for _ in 0..rng.below(7) {
        let header = match rng.below(9) {
            0 | 1 => format!("Content-Length: {}", rng.pick(LENGTHS)),
            2 => format!("content-length: {}", rng.below(MAX_BODY + 64)),
            3 => "Transfer-Encoding: chunked".to_owned(),
            4 => "Host: den.local:8080".to_owned(),
            5 => "Cookie: a=b; t=\"x\"".to_owned(),
            6 => " bad: x".to_owned(),
            7 => "no colon".to_owned(),
            _ => "X-Empty:".to_owned(),
        };
        text.push_str(line_end);
        text.push_str(&header);
    }
    let mut bytes = text.into_bytes();
    if !rng.one_in(10) {
        bytes.extend_from_slice(b"\r\n\r\n");
    }
    let body_len = rng.below(MAX_BODY + 200);
    bytes.extend((0..body_len).map(|_| rng.byte()));
    if rng.one_in(3) {
        bytes = mutate(rng, &bytes, HTTP_BYTES);
    }
    bytes
}

/// Hands out `data`, never across a cut, then reports the end.
struct Chunked<'a> {
    data: &'a [u8],
    at: usize,
    cuts: Vec<usize>,
}

impl Read for Chunked<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let next = self
            .cuts
            .iter()
            .copied()
            .filter(|&cut| cut > self.at)
            .min()
            .unwrap_or(self.data.len())
            .min(self.data.len());
        let count = buffer.len().min(next - self.at);
        buffer[..count].copy_from_slice(&self.data[self.at..self.at + count]);
        self.at += count;
        Ok(count)
    }
}

fn read_cut(data: &[u8], cuts: Vec<usize>) -> Result<Request, ReadError> {
    let mut reader = Chunked { data, at: 0, cuts };
    let mut reads = 0;
    // Each read takes at least one byte, so this many arms means a loop.
    let limit = MAX_HEAD + MAX_BODY + 64;
    http::read_from(&mut reader, |_| {
        reads += 1;
        assert!(reads < limit, "read_from looped");
        true
    })
}

fn blank_line(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

/// What an accepted request must satisfy, against the bytes it came from.
fn check_accepted(data: &[u8], request: &Request) {
    let end = blank_line(data).expect("an accepted request has a blank line");
    assert!(end <= MAX_HEAD, "head of {end} bytes accepted");
    assert!(
        request.body.len() <= MAX_BODY,
        "body of {} bytes accepted",
        request.body.len()
    );
    assert_eq!(request.body, data[end + 4..end + 4 + request.body.len()]);
    assert!(request.header("transfer-encoding").is_none());
    assert!(request.header_count("content-length") <= 1);
    match request.header("content-length") {
        None => assert!(request.body.is_empty()),
        Some(value) => {
            // RFC 9110 §8.6: Content-Length = 1*DIGIT.
            assert!(
                !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                "Content-Length {value:?} accepted"
            );
            assert_eq!(value.parse::<usize>().ok(), Some(request.body.len()));
        }
    }
    let head = http::parse_head(&data[..end]).expect("an accepted head parses");
    assert_eq!(head.method, request.method);
    assert_eq!(head.path, request.path);
}

fn check_head(head: &[u8]) {
    let Ok(request) = http::parse_head(head) else {
        return;
    };
    assert!(request.body.is_empty());
    assert!(!request.method.is_empty() && !request.method.contains(' '));
    assert!(request.path.starts_with('/') && !request.path.contains(['?', '#', ' ']));
    let _ = request.cookie("t");
    let _ = request.content_type_is("application/json");
    let _ = request.header_count("host");
}

#[test]
fn request_heads_from_any_bytes_parse_or_refuse() {
    let mut rng = Rng::new(0x5eed_0001);
    for _ in 0..6_000 {
        let request = random_request(&mut rng);
        let end = blank_line(&request).unwrap_or(request.len());
        check_head(&request[..end]);
        let cut = rng.below(end + 1);
        check_head(&request[..cut]);
    }
    for _ in 0..4_000 {
        check_head(&text_like(&mut rng, HTTP_BYTES, 300));
    }
    let long_name = format!("GET / HTTP/1.1\r\n{}: x", "a".repeat(MAX_HEAD));
    for edge in [
        &b""[..],
        b"\r\n",
        b" ",
        b"GET",
        b"GET  / HTTP/1.1",
        b"GET / HTTP/1.1 x",
        b"GET / HTTP/1.1\r\n:",
        b"GET / HTTP/1.1\r\nA:\r\nA:\r\n",
        b"GET / HTTP/1.1\n\nHost: x",
        b"GET /\xff HTTP/1.1",
        b"GET / HTTP/1.1\r\nCookie: \xc3",
        long_name.as_bytes(),
    ] {
        check_head(edge);
    }
}

#[test]
fn requests_read_the_same_however_they_arrive_and_stay_in_limits() {
    let mut rng = Rng::new(0x5eed_0002);
    for _ in 0..5_000 {
        let data = random_request(&mut rng);
        let whole = read_cut(&data, Vec::new());
        let cuts = (0..rng.below(6))
            .map(|_| rng.below(data.len() + 1))
            .collect();
        assert_eq!(read_cut(&data, cuts), whole, "{:?}", lossy(&data));
        if data.len() <= 600 {
            assert_eq!(read_cut(&data, (1..data.len()).collect()), whole);
        }
        if let Ok(request) = &whole {
            check_accepted(&data, request);
        }
    }
}

#[test]
fn request_edges_get_the_documented_answers() {
    let post = |length: &str, body: &[u8]| {
        let mut data =
            format!("POST /api/pair HTTP/1.1\r\nContent-Length: {length}\r\n\r\n").into_bytes();
        data.extend_from_slice(body);
        data
    };
    let full = vec![b'x'; MAX_BODY];
    let cases: Vec<(Vec<u8>, Result<usize, ReadError>)> = vec![
        (Vec::new(), Err(ReadError::Incomplete)),
        (b"\r\n\r\n".to_vec(), Err(ReadError::Malformed)),
        (b"GET / HTTP/1.1\n\n".to_vec(), Err(ReadError::Incomplete)),
        (b"GET / HTTP/1.1\r\n\r\n".to_vec(), Ok(0)),
        (
            b"GET /\xff HTTP/1.1\r\n\r\n".to_vec(),
            Err(ReadError::Malformed),
        ),
        (post("4096", &full), Ok(MAX_BODY)),
        (post("4097", &full), Err(ReadError::BodyTooLarge)),
        (
            post("18446744073709551615", b""),
            Err(ReadError::BodyTooLarge),
        ),
        (post("18446744073709551616", b""), Err(ReadError::Malformed)),
        (
            post("99999999999999999999999999", b""),
            Err(ReadError::Malformed),
        ),
        (post("-1", b""), Err(ReadError::Malformed)),
        (post("+5", b"hello"), Err(ReadError::Malformed)),
        (post("5, 5", b"hello"), Err(ReadError::Malformed)),
        (post("", b""), Err(ReadError::Malformed)),
        (post("5", b"hel"), Err(ReadError::Incomplete)),
        (
            b"POST / HTTP/1.1\r\nContent-Length: 0\r\ncontent-length: 0\r\n\r\n".to_vec(),
            Err(ReadError::Malformed),
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
            Err(ReadError::Malformed),
        ),
    ];
    for (data, expected) in cases {
        let got = read_cut(&data, Vec::new()).map(|request| request.body.len());
        assert_eq!(got, expected, "{:?}", lossy(&data));
        assert_eq!(
            read_cut(&data, (1..data.len()).collect()).map(|request| request.body.len()),
            expected
        );
    }
}

#[test]
fn the_head_limit_holds_wherever_the_bytes_split() {
    for len in MAX_HEAD - 2..=MAX_HEAD + 2 {
        let start = "GET / HTTP/1.1\r\nX: ";
        let mut data = start.as_bytes().to_vec();
        data.resize(len, b'a');
        data.extend_from_slice(b"\r\n\r\n");
        let expected = if len <= MAX_HEAD {
            Ok(len)
        } else {
            Err(ReadError::HeadTooLarge)
        };
        for cut in [None, Some(MAX_HEAD - 1), Some(MAX_HEAD), Some(MAX_HEAD + 1)]
            .into_iter()
            .chain((MAX_HEAD + 2..=MAX_HEAD + 4).map(Some))
        {
            let got = read_cut(&data, cut.into_iter().collect())
                .map(|request| request.header("x").map_or(0, str::len) + start.len());
            assert_eq!(got, expected, "head of {len} bytes, cut at {cut:?}");
        }
    }
}

/// `prefix`, then `fill` without end.
struct Endless {
    prefix: Vec<u8>,
    fill: u8,
    served: usize,
}

impl Read for Endless {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        for byte in buffer.iter_mut() {
            *byte = self.prefix.get(self.served).copied().unwrap_or(self.fill);
            self.served += 1;
        }
        Ok(buffer.len())
    }
}

#[test]
fn an_endless_peer_is_cut_off_at_the_limits() {
    let cases = [
        (Vec::new(), b'a', Err(ReadError::HeadTooLarge)),
        (
            b"GET / HTTP/1.1\r\nX: ".to_vec(),
            b'\r',
            Err(ReadError::HeadTooLarge),
        ),
        (
            b"POST / HTTP/1.1\r\nContent-Length: 4096\r\n\r\n".to_vec(),
            b'{',
            Ok(MAX_BODY),
        ),
    ];
    for (prefix, fill, expected) in cases {
        let mut reader = Endless {
            prefix,
            fill,
            served: 0,
        };
        let got = http::read_from(&mut reader, |_| true).map(|request| request.body.len());
        assert_eq!(got, expected);
        assert!(
            reader.served <= MAX_HEAD + MAX_BODY + 2 * 1024,
            "{} bytes read",
            reader.served
        );
    }
}

const ESCAPE_BYTES: &[u8] = b"%%%%0123456789abcdefABCDEFGgxz+-/ ";

fn check_decoded(segment: &str) {
    let Some(decoded) = percent_decode(segment) else {
        return;
    };
    assert!(!decoded.is_empty() && decoded.len() <= segment.len());
    // RFC 3986 §2.1: pct-encoded = "%" HEXDIG HEXDIG.
    let bytes = segment.as_bytes();
    for (index, _) in segment.match_indices('%') {
        let pair = bytes.get(index + 1..index + 3);
        assert!(
            pair.is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit)),
            "{segment:?} decoded to {decoded:?} through a broken escape"
        );
    }
}

// covers: S257 (the IDs are percent-decoded)
#[test]
fn path_segments_decode_or_refuse() {
    let mut rng = Rng::new(0x5eed_0003);
    for _ in 0..12_000 {
        check_decoded(&lossy(&text_like(&mut rng, ESCAPE_BYTES, 40)));
    }
    for _ in 0..2_000 {
        let original = lossy(&text_like(&mut rng, b"abc /%:\xc3\xa9", 24));
        let encoded: String = original.bytes().map(|b| format!("%{b:02X}")).collect();
        let expected = Some(original).filter(|text| !text.is_empty());
        assert_eq!(percent_decode(&encoded), expected);
    }
    for edge in [
        "", "%", "%%", "%2", "%G1", "%1G", "%+1", "%+F", "%-1", "% 1", "%é1", "%1é", "%C3", "%00",
        "%FF", "a%",
    ] {
        check_decoded(edge);
    }
}

const HOST_BYTES: &[u8] = b"[]:%.0123456789abcdefden.local:8080 ";

fn local() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
}

fn check_host(value: &str) {
    if let Some((host, port)) = net::split_host(value) {
        assert!(!host.is_empty() && value.contains(host));
        if value.starts_with('[') {
            assert!(
                host.split('%').next().unwrap().parse::<Ipv6Addr>().is_ok(),
                "{value:?}"
            );
        } else {
            assert!(!host.contains(':'), "{value:?}");
        }
        if let Some(port) = port {
            let digits = value.rsplit(':').next().unwrap();
            assert_eq!(digits.parse::<u16>().ok(), Some(port), "{value:?}");
        }
    }
    let six = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 80);
    for (local, name) in [
        (local(), Some("den")),
        (local(), Some("den.local")),
        (local(), Some("")),
        (local(), Some(".local")),
        (local(), None),
        (six, Some("é")),
    ] {
        if net::host_allowed(value, local, name) {
            assert!(net::split_host(value.trim()).is_some(), "{value:?}");
        }
    }
    for origin in [
        format!("http://{value}"),
        value.to_owned(),
        "http://".to_owned(),
    ] {
        if net::origin_matches(&origin, value) {
            let rest = origin.trim().strip_prefix("http://").unwrap();
            assert_eq!(rest.len(), value.trim().len());
        }
    }
}

#[test]
fn hosts_and_origins_from_any_text_are_judged_without_panic() {
    let mut rng = Rng::new(0x5eed_0004);
    let valid: &[&str] = &[
        "den.local:8080",
        "DEN.LOCAL.:8080",
        "127.0.0.1:8080",
        "[::1]:8080",
        "[::1]",
        "[fe80::1%25en0]:80",
        "den.local",
    ];
    for _ in 0..12_000 {
        let value = if rng.one_in(2) {
            let base = rng.pick(valid).as_bytes();
            lossy(&mutate(&mut rng, base, HOST_BYTES))
        } else {
            lossy(&text_like(&mut rng, HOST_BYTES, 30))
        };
        check_host(&value);
    }
    for edge in [
        "",
        " ",
        ":",
        "[",
        "]",
        "[]",
        "[]:80",
        "[::1",
        "[::1]:",
        "[::1]x",
        "[::1]:+80",
        "[den]:80",
        "::1",
        "den:",
        "den:65536",
        "den:+80",
        "den:99999999999999999999",
        "den::80",
        "[::1%]:8080",
        "[%25]:8080",
        "é:8080",
        "den.local.:8080",
    ] {
        check_host(edge);
    }
}

const JSON_BYTES: &[u8] = b"{}[]\":,0123456789.eE+- truefalsnul\\u\"\"";

const BODIES: &[&str] = &[
    r#"{"on": true, "volume": 40, "delay_ms": 120}"#,
    r#"{"volume": 70.5}"#,
    r#"{"toggle": true, "volume": 3}"#,
    r#"{"recall": true}"#,
    r#"{"id": "airplay:den"}"#,
    r#"{"code": "123456"}"#,
];

fn check_bodies(body: &[u8]) {
    if let Ok(change) = state::speaker_change_from_body(body) {
        assert!(change.on.is_some() || change.volume.is_some() || change.delay_ms.is_some());
    }
    let _ = state::master_volume_from_body(body);
    if let Ok(change) = state::zone_change_from_body(body) {
        assert!(change.toggle || change.volume.is_some());
    }
    let _ = state::preset_recall_from_body(body);
    if let Ok(id) = state::source_id_from_body(body) {
        assert!(id.len() <= body.len());
    }
    if let Ok(code) = state::pairing_code_from_body(body) {
        assert!(code.len() <= body.len());
    }
}

#[test]
fn json_bodies_from_any_bytes_parse_or_refuse() {
    let mut rng = Rng::new(0x5eed_0005);
    for _ in 0..3_000 {
        let base = rng.pick(BODIES).as_bytes();
        let cut = rng.below(base.len() + 1);
        check_bodies(&base[..cut]);
        check_bodies(&mutate(&mut rng, base, JSON_BYTES));
    }
    for _ in 0..2_000 {
        check_bodies(&text_like(&mut rng, JSON_BYTES, 120));
    }
    let deep_array = "[".repeat(MAX_BODY);
    let deep_object = r#"{"a":"#.repeat(MAX_BODY / 5);
    let long_string = format!(r#"{{"id": "{}"}}"#, "x".repeat(MAX_BODY - 12));
    for edge in [
        &b""[..],
        b"null",
        b"[]",
        b"{}",
        b"{\"volume\": 1e400}",
        b"{\"volume\": -0}",
        b"{\"delay_ms\": 99999999999999999999}",
        b"{\"delay_ms\": 1.5}",
        b"{\"id\": \"\\ud800\"}",
        b"{\"code\": \"\xff\"}",
        b"{\"on\": true, \"on\": false}",
        b"\xef\xbb\xbf{\"volume\": 1}",
        deep_array.as_bytes(),
        deep_object.as_bytes(),
        long_string.as_bytes(),
    ] {
        check_bodies(edge);
    }
}
