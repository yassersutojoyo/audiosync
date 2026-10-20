//! RTSP/1.0 messages (RFC 2326) as AirPlay uses them (P20, P21, P24, P25,
//! P27, P31, P32).
//!
//! [`Request::encode`] writes a request; [`Parser`] reads messages from a
//! byte stream in either direction, since the event channel of the AirPlay 2
//! path carries requests from the receiver (P70) while on AirPlay 1 any
//! request from the receiver is a protocol error (P40). Header helpers read
//! the values the session needs.

use std::fmt;

/// The request methods the sender uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Options,
    Announce,
    Setup,
    Record,
    SetParameter,
    GetParameter,
    Flush,
    Teardown,
    /// `POST` to a path such as `/auth-setup` or `/pair-verify` (P47, P60).
    Post,
    /// `GET /info` on the AirPlay 2 path (P41).
    Get,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Options => "OPTIONS",
            Method::Announce => "ANNOUNCE",
            Method::Setup => "SETUP",
            Method::Record => "RECORD",
            Method::SetParameter => "SET_PARAMETER",
            Method::GetParameter => "GET_PARAMETER",
            Method::Flush => "FLUSH",
            Method::Teardown => "TEARDOWN",
            Method::Post => "POST",
            Method::Get => "GET",
        }
    }

    /// Method names are case-sensitive (RFC 2326 §6.1).
    pub fn parse(name: &str) -> Option<Method> {
        Some(match name {
            "OPTIONS" => Method::Options,
            "ANNOUNCE" => Method::Announce,
            "SETUP" => Method::Setup,
            "RECORD" => Method::Record,
            "SET_PARAMETER" => Method::SetParameter,
            "GET_PARAMETER" => Method::GetParameter,
            "FLUSH" => Method::Flush,
            "TEARDOWN" => Method::Teardown,
            "POST" => Method::Post,
            "GET" => Method::Get,
            _ => return None,
        })
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Header lines in the order they were added or received. Names compare
/// case-insensitively (P24).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn new() -> Headers {
        Headers(Vec::new())
    }

    /// Adds a header line. CR and LF in either part are replaced by spaces,
    /// so no value can end a line early.
    pub fn push(&mut self, name: &str, value: impl Into<String>) {
        let clean = |text: String| text.replace(['\r', '\n'], " ");
        self.0.push((clean(name.to_owned()), clean(value.into())));
    }

    /// The first value of `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Every value of `name`, in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A request: the request line, its headers and a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// `*`, an `rtsp://` URI or a path such as `/auth-setup`.
    pub uri: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: Method, uri: impl Into<String>) -> Request {
        Request {
            method,
            uri: uri.into(),
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    /// RTSP/1.0 text with CRLF line ends (P20). `Content-Length` is written
    /// from the body when the body isn't empty, after the other headers; a
    /// `Content-Length` among the headers is skipped.
    pub fn encode(&self) -> Vec<u8> {
        let mut text = format!("{} {} RTSP/1.0\r\n", self.method.as_str(), self.uri);
        write_head(&mut text, &self.headers, self.body.len());
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// A response: the status line, its headers and a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub code: u16,
    pub reason: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(code: u16, reason: impl Into<String>) -> Response {
        Response {
            code,
            reason: reason.into(),
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.code)
    }

    /// RTSP/1.0 text with CRLF line ends, as a receiver writes it.
    pub fn encode(&self) -> Vec<u8> {
        let mut text = format!("RTSP/1.0 {} {}\r\n", self.code, self.reason);
        write_head(&mut text, &self.headers, self.body.len());
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }

    /// The `CSeq` value, if present and a number (P21).
    pub fn cseq(&self) -> Option<u32> {
        self.headers.get("CSeq")?.trim().parse().ok()
    }
}

fn write_head(text: &mut String, headers: &Headers, body_len: usize) {
    for (name, value) in headers.iter() {
        if name.eq_ignore_ascii_case("Content-Length") {
            continue;
        }
        text.push_str(name);
        text.push_str(": ");
        text.push_str(value);
        text.push_str("\r\n");
    }
    if body_len > 0 {
        text.push_str(&format!("Content-Length: {body_len}\r\n"));
    }
    text.push_str("\r\n");
}

/// One message read from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Request(Request),
    Response(Response),
}

/// Why a message couldn't be read; the session treats each as a protocol
/// error (P140).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The first line is neither `RTSP/1.0 <code> <reason>` nor a request
    /// line.
    StartLine,
    /// A request line names a method the sender doesn't know.
    UnknownMethod,
    /// A header line without a colon, or a continuation line with nothing to
    /// continue.
    HeaderLine,
    /// `Content-Length` isn't a number.
    ContentLength,
    /// The head or the body is larger than the limits below.
    TooLarge,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl ParseError {
    pub fn detail(self) -> &'static str {
        match self {
            ParseError::StartLine => "malformed start line",
            ParseError::UnknownMethod => "unknown request method",
            ParseError::HeaderLine => "malformed header line",
            ParseError::ContentLength => "malformed Content-Length",
            ParseError::TooLarge => "message too large",
        }
    }
}

impl std::error::Error for ParseError {}

/// Largest message head the parser accepts.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Largest body the parser accepts.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Reads messages from a byte stream. Lines end in CRLF; a bare LF is
/// accepted too, as RFC 2326 §4 asks of receivers of RTSP text. Empty lines
/// before a start line are skipped, and a head that isn't UTF-8 is read with
/// each invalid sequence replaced by U+FFFD, so a stray byte in a header
/// value doesn't end the session (P140 then judges what is left). A
/// control character other than tab inside a header value does: that line
/// is malformed, since values go back out in the sender's own headers.
#[derive(Debug, Default)]
pub struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    pub fn new() -> Parser {
        Parser { buffer: Vec::new() }
    }

    /// Appends bytes read from the connection.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Bytes received but not yet part of a complete message.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// The next complete message, `Ok(None)` while one is still incomplete.
    /// After an error the stream can't be trusted and should be closed.
    pub fn next_message(&mut self) -> Result<Option<Message>, ParseError> {
        let stray = self
            .buffer
            .iter()
            .take_while(|&&byte| byte == b'\r' || byte == b'\n')
            .count();
        if stray > 0 {
            self.buffer.drain(..stray);
        }
        let Some((head_len, body_start)) = find_head_end(&self.buffer) else {
            if self.buffer.len() > MAX_HEAD_BYTES {
                return Err(ParseError::TooLarge);
            }
            return Ok(None);
        };
        if head_len > MAX_HEAD_BYTES {
            return Err(ParseError::TooLarge);
        }
        let head = String::from_utf8_lossy(&self.buffer[..head_len]).into_owned();
        let mut lines = head
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line));
        let start = lines.next().unwrap_or("");
        let headers = parse_headers(lines)?;
        let body_len = match headers.get("Content-Length") {
            None => 0,
            Some(value) => value
                .trim()
                .parse::<usize>()
                .map_err(|_| ParseError::ContentLength)?,
        };
        if body_len > MAX_BODY_BYTES {
            return Err(ParseError::TooLarge);
        }
        if self.buffer.len() < body_start + body_len {
            return Ok(None);
        }
        let message = parse_start(start, headers)?;
        let body = self.buffer[body_start..body_start + body_len].to_vec();
        self.buffer.drain(..body_start + body_len);
        Ok(Some(match message {
            Message::Request(mut request) => {
                request.body = body;
                Message::Request(request)
            }
            Message::Response(mut response) => {
                response.body = body;
                Message::Response(response)
            }
        }))
    }
}

/// The length of the head (without its blank line) and where the body starts.
fn find_head_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while let Some(offset) = buffer[index..].iter().position(|&byte| byte == b'\n') {
        let newline = index + offset;
        let rest = &buffer[newline + 1..];
        if rest.starts_with(b"\r\n") {
            return Some((newline + 1, newline + 3));
        }
        if rest.starts_with(b"\n") {
            return Some((newline + 1, newline + 2));
        }
        index = newline + 1;
    }
    None
}

fn parse_headers<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Headers, ParseError> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            // A continuation line extends the previous value (RFC 2326 §4.2).
            let (_, value) = entries.last_mut().ok_or(ParseError::HeaderLine)?;
            value.push(' ');
            value.push_str(header_value(line)?);
            continue;
        }
        let (name, value) = line.split_once(':').ok_or(ParseError::HeaderLine)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(ParseError::HeaderLine);
        }
        entries.push((name.to_owned(), header_value(value)?.to_owned()));
    }
    Ok(Headers(entries))
}

/// A header value as read: trimmed, and with no control character but tab
/// left in it. Values such as a nonce or a session ID go back out in the
/// sender's own headers, where a CR, an LF or a NUL could end the header
/// or start another, so a value holding one makes its line malformed.
fn header_value(text: &str) -> Result<&str, ParseError> {
    let value = text.trim();
    if value.bytes().any(|byte| byte < b' ' && byte != b'\t') {
        return Err(ParseError::HeaderLine);
    }
    Ok(value)
}

fn parse_start(line: &str, headers: Headers) -> Result<Message, ParseError> {
    if let Some(rest) = line.strip_prefix("RTSP/1.0 ") {
        let (code, reason) = rest.split_once(' ').unwrap_or((rest, ""));
        if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ParseError::StartLine);
        }
        let code: u16 = code.parse().map_err(|_| ParseError::StartLine)?;
        if !(100..600).contains(&code) {
            return Err(ParseError::StartLine);
        }
        return Ok(Message::Response(Response {
            code,
            reason: reason.trim().to_owned(),
            headers,
            body: Vec::new(),
        }));
    }
    let mut parts = line.split(' ');
    let (Some(method), Some(uri), Some("RTSP/1.0"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ParseError::StartLine);
    };
    if uri.is_empty() {
        return Err(ParseError::StartLine);
    }
    let method = Method::parse(method).ok_or(ParseError::UnknownMethod)?;
    Ok(Message::Request(Request {
        method,
        uri: uri.to_owned(),
        headers,
        body: Vec::new(),
    }))
}

/// The ports of a SETUP response's `Transport` header (P31).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransportPorts {
    /// Where audio goes.
    pub server_port: Option<u16>,
    /// Where sync packets and retransmission replies go.
    pub control_port: Option<u16>,
    /// The receiver's timing port; 0 is allowed.
    pub timing_port: Option<u16>,
}

impl TransportPorts {
    /// Reads the `key=value` parameters of a `Transport` value; parameters
    /// that aren't ports are skipped, and a port that isn't a number counts
    /// as missing.
    pub fn parse(value: &str) -> TransportPorts {
        let mut ports = TransportPorts::default();
        for parameter in value.split(';') {
            let Some((key, port)) = parameter.split_once('=') else {
                continue;
            };
            let port = port.trim().parse::<u16>().ok();
            match key.trim().to_ascii_lowercase().as_str() {
                "server_port" => ports.server_port = port,
                "control_port" => ports.control_port = port,
                "timing_port" => ports.timing_port = port,
                _ => {}
            }
        }
        ports
    }
}

/// The method list of an OPTIONS answer's `Public` header (P27).
pub fn public_methods(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|method| !method.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The session ID of a `Session` header, without the `;timeout=` parameter
/// RFC 2326 §12.37 allows after it.
pub fn session_id(value: &str) -> Option<&str> {
    let id = value.split(';').next()?.trim();
    (!id.is_empty()).then_some(id)
}

/// `Audio-Latency` of a RECORD answer, in frames (P32, P110).
pub fn audio_latency(value: &str) -> Option<u32> {
    value.trim().parse().ok()
}

/// What `Audio-Jack-Status` says (P24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JackStatus {
    pub connected: bool,
    pub kind: Option<JackKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JackKind {
    Analog,
    Digital,
}

impl JackStatus {
    /// `connected` or `disconnected`, optionally followed by `; type=analog`
    /// or `; type=digital`. Anything else is `None`.
    pub fn parse(value: &str) -> Option<JackStatus> {
        let mut parts = value.split(';').map(str::trim);
        let connected = match parts.next()?.to_ascii_lowercase().as_str() {
            "connected" => true,
            "disconnected" => false,
            _ => return None,
        };
        let mut kind = None;
        for part in parts {
            if let Some((key, value)) = part.split_once('=')
                && key.trim().eq_ignore_ascii_case("type")
            {
                kind = match value.trim().to_ascii_lowercase().as_str() {
                    "analog" => Some(JackKind::Analog),
                    "digital" => Some(JackKind::Digital),
                    _ => None,
                };
            }
        }
        Some(JackStatus { connected, kind })
    }
}
