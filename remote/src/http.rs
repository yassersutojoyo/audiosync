//! Reading one HTTP/1.1 request and writing one response (RFC 9112). Every
//! connection carries one request; the answer closes it.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::text;

/// The request line and headers together.
pub const MAX_HEAD: usize = 8 * 1024;
/// The largest body: every request body here is a few small JSON fields.
pub const MAX_BODY: usize = 4 * 1024;
/// The whole request must arrive within this, however it trickles.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// One request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// The target's path, without the query.
    pub path: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Why no request was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The peer closed or the time ran out before a whole request came.
    Incomplete,
    /// Not HTTP/1.x, or a body framing this server doesn't take (400).
    Malformed,
    /// 431.
    HeadTooLarge,
    /// 413.
    BodyTooLarge,
}

impl ReadError {
    /// The answer, when one is worth sending.
    pub fn status(self) -> Option<u16> {
        match self {
            ReadError::Incomplete => None,
            ReadError::Malformed => Some(400),
            ReadError::HeadTooLarge => Some(431),
            ReadError::BodyTooLarge => Some(413),
        }
    }
}

impl Request {
    /// The first header named `name`, any case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn header_count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .count()
    }

    /// The value of cookie `name` (RFC 6265 §5.4), across every `Cookie` header.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("cookie"))
            .flat_map(|(_, value)| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.trim_matches('"'))
    }

    /// Whether `Content-Type` is `type` (RFC 9110 §8.3.1: parameters and case aside).
    pub fn content_type_is(&self, media_type: &str) -> bool {
        self.header("content-type").is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|base| base.trim().eq_ignore_ascii_case(media_type))
        })
    }
}

/// Parses the request line and the headers, up to but without the blank line.
pub fn parse_head(head: &[u8]) -> Result<Request, ReadError> {
    let text = std::str::from_utf8(head).map_err(|_| ReadError::Malformed)?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().ok_or(ReadError::Malformed)?.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ReadError::Malformed);
    };
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") || !target.starts_with('/') || method.is_empty()
    {
        return Err(ReadError::Malformed);
    }
    let path = target.split(['?', '#']).next().unwrap_or(target).to_owned();
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(ReadError::Malformed)?;
        // RFC 9112 §5.1: no whitespace between the name and the colon.
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(ReadError::Malformed);
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    Ok(Request {
        method: method.to_owned(),
        path,
        headers,
        body: Vec::new(),
    })
}

fn find_blank_line(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Reads one request, its body included, within `limit` however it
/// trickles ([`REQUEST_TIMEOUT`] in the product).
pub fn read_request(stream: &mut TcpStream, limit: Duration) -> Result<Request, ReadError> {
    let deadline = Instant::now() + limit;
    read_from(stream, |stream| {
        let left = deadline.saturating_duration_since(Instant::now());
        !left.is_zero() && stream.set_read_timeout(Some(left)).is_ok()
    })
}

/// [`read_request`] over any reader: `arm` runs before each read, and a
/// `false` from it ends the request as [`ReadError::Incomplete`].
pub(crate) fn read_from<R: Read>(
    reader: &mut R,
    mut arm: impl FnMut(&mut R) -> bool,
) -> Result<Request, ReadError> {
    let mut data = Vec::new();
    let mut buffer = [0u8; 1024];
    let mut fill = |data: &mut Vec<u8>, reader: &mut R| -> Result<(), ReadError> {
        if !arm(reader) {
            return Err(ReadError::Incomplete);
        }
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Err(ReadError::Incomplete),
                Ok(count) => {
                    data.extend_from_slice(&buffer[..count]);
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(ReadError::Incomplete),
            }
        }
    };
    let end = loop {
        if let Some(end) = find_blank_line(&data) {
            break end;
        }
        // Only once a blank line can no longer end within MAX_HEAD, so the
        // answer doesn't depend on how the bytes were split.
        if data.len() > MAX_HEAD + 3 {
            return Err(ReadError::HeadTooLarge);
        }
        fill(&mut data, reader)?;
    };
    if end > MAX_HEAD {
        return Err(ReadError::HeadTooLarge);
    }
    let mut request = parse_head(&data[..end])?;
    if request.header("transfer-encoding").is_some() || request.header_count("content-length") > 1 {
        return Err(ReadError::Malformed);
    }
    let length = match request.header("content-length") {
        None => 0,
        // RFC 9110 §8.6 allows digits only; parse alone also takes a leading '+'.
        Some(value) if value.bytes().all(|b| b.is_ascii_digit()) => {
            value.parse::<usize>().map_err(|_| ReadError::Malformed)?
        }
        Some(_) => return Err(ReadError::Malformed),
    };
    if length > MAX_BODY {
        return Err(ReadError::BodyTooLarge);
    }
    let start = end + 4;
    while data.len() < start + length {
        fill(&mut data, reader)?;
    }
    request.body = data[start..start + length].to_vec();
    Ok(request)
}

/// One response; the headers every answer carries are added when it is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            headers: vec![("Content-Type", content_type.to_owned())],
            body: body.into(),
        }
    }

    /// An API answer, never cached (the snapshot changes under the same URL).
    pub fn json(status: u16, body: String) -> Response {
        Response::new(status, "application/json; charset=utf-8", body)
            .with("Cache-Control", "no-store")
    }

    pub fn error(error: &text::ApiError) -> Response {
        Response::json(error.status, error.body())
    }

    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Response {
        self.headers.push((name, value.into()));
        self
    }
}

/// The headers on every response, the event stream's included.
pub fn security_headers(content_security_policy: &str) -> String {
    format!(
        "Content-Security-Policy: {content_security_policy}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n"
    )
}

/// The status line and the headers, without the body's length.
pub fn head(status: u16, headers: &[(&str, String)], content_security_policy: &str) -> String {
    let mut text = format!("HTTP/1.1 {status} {}\r\n", text::reason(status));
    for (name, value) in headers {
        text.push_str(name);
        text.push_str(": ");
        text.push_str(value);
        text.push_str("\r\n");
    }
    text.push_str(&security_headers(content_security_policy));
    text.push_str("Connection: close\r\n");
    text
}

pub fn write_response(
    stream: &mut impl Write,
    response: &Response,
    content_security_policy: &str,
) -> io::Result<()> {
    let mut bytes = head(response.status, &response.headers, content_security_policy);
    bytes.push_str(&format!("Content-Length: {}\r\n\r\n", response.body.len()));
    let mut bytes = bytes.into_bytes();
    bytes.extend_from_slice(&response.body);
    stream.write_all(&bytes)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_parses_into_method_path_and_headers() {
        let request = parse_head(
            b"POST /api/pair?x=1 HTTP/1.1\r\nHost: kitchen.local:7655\r\nContent-Type: Application/JSON; charset=utf-8\r\nCookie: a=b; as_token=\"abc\"\r\nCookie: c=d",
        )
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/pair");
        assert_eq!(request.header("HOST"), Some("kitchen.local:7655"));
        assert!(request.content_type_is("application/json"));
        assert!(!request.content_type_is("text/plain"));
        assert_eq!(request.cookie("as_token"), Some("abc"));
        assert_eq!(request.cookie("c"), Some("d"));
        assert_eq!(request.cookie("missing"), None);
    }

    #[test]
    fn the_token_cookie_is_found_only_by_its_exact_name() {
        let request = parse_head(
            b"GET / HTTP/1.1\r\nCookie: xas_token=a; as_token_old=b; AS_TOKEN=c; as_token\r\nCookie: other=d",
        )
        .unwrap();
        assert_eq!(request.cookie("as_token"), None);
        let request = parse_head(b"GET / HTTP/1.1\r\nCookie: x=1;as_token=abc ;y=2").unwrap();
        assert_eq!(request.cookie("as_token"), Some("abc"));
        let request = parse_head(b"GET / HTTP/1.1\r\nCookie: as_token=").unwrap();
        assert_eq!(request.cookie("as_token"), Some(""));
    }

    #[test]
    fn a_bad_head_is_refused() {
        for bad in [
            &b"GET / HTTP/2\r\nHost: x"[..],
            b"GET http://x/ HTTP/1.1",
            b"GET / HTTP/1.1 extra",
            b"GET / HTTP/1.1\r\nHost : x",
            b"GET / HTTP/1.1\r\nno colon",
            b"\xff / HTTP/1.1",
        ] {
            assert_eq!(parse_head(bad), Err(ReadError::Malformed), "{bad:?}");
        }
    }

    #[test]
    fn every_response_carries_the_security_headers() {
        let mut out = Vec::new();
        let response = Response::json(200, "{}".to_owned());
        write_response(&mut out, &response, "default-src 'self'").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("\r\nContent-Security-Policy: default-src 'self'\r\n"));
        assert!(text.contains("\r\nX-Content-Type-Options: nosniff\r\n"));
        assert!(text.contains("\r\nReferrer-Policy: no-referrer\r\n"));
        assert!(text.contains("\r\nCache-Control: no-store\r\n"));
        assert!(text.ends_with("Content-Length: 2\r\n\r\n{}"));
    }
}
