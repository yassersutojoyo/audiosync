//! A minimal HTTP/1.1 client for the stream server tests.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// `http://host:port/path` as its socket address and path.
pub fn split_url(url: &str) -> Option<(SocketAddr, String)> {
    let rest = url.strip_prefix("http://")?;
    let slash = rest.find('/')?;
    let address = rest[..slash].parse().ok()?;
    Some((address, rest[slash..].to_owned()))
}

pub fn request(address: SocketAddr, request: &str) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.write_all(request.as_bytes())?;
    Ok(stream)
}

/// Where the header section ends (the index of its blank line).
pub fn head_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

/// One read that waits up to the socket's read timeout. The wait is a peek:
/// on Windows a timeout racing arriving bytes can end a read with
/// ERROR_IO_PENDING and lose what it took, while a peek takes nothing.
pub fn read_timed(stream: &mut TcpStream, buffer: &mut [u8]) -> io::Result<usize> {
    const ERROR_IO_PENDING: i32 = 997;
    match stream.peek(buffer) {
        Ok(0) => return Ok(0),
        Ok(_) => {}
        Err(error) if cfg!(windows) && error.raw_os_error() == Some(ERROR_IO_PENDING) => {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        Err(error) => return Err(error),
    }
    stream.read(buffer)
}

/// Reads until the head is complete, then for `body_for` more; returns
/// the head and the raw body bytes read.
pub fn read_response(stream: &mut TcpStream, body_for: Duration) -> (String, Vec<u8>) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    let start = Instant::now();
    let mut head_at: Option<Instant> = None;
    loop {
        if let Some(at) = head_at
            && at.elapsed() >= body_for
        {
            break;
        }
        if head_at.is_none() && start.elapsed() > Duration::from_secs(5) {
            break;
        }
        match read_timed(stream, &mut buffer) {
            Ok(0) => break,
            Ok(count) => data.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        if head_at.is_none() && head_end(&data).is_some() {
            head_at = Some(Instant::now());
        }
    }
    match head_end(&data) {
        Some(end) => (
            String::from_utf8_lossy(&data[..end]).into_owned(),
            data[end + 4..].to_vec(),
        ),
        None => (String::from_utf8_lossy(&data).into_owned(), Vec::new()),
    }
}

/// The status code of a response head.
pub fn status(head: &str) -> u16 {
    head.split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

/// The value of a header, by case-insensitive name.
pub fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The chunks of a chunked body; the last is empty when the terminating
/// chunk arrived. A partial chunk at the end is left out.
pub fn chunks(body: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(line) = body[at..].windows(2).position(|w| w == b"\r\n") {
        let size_text = String::from_utf8_lossy(&body[at..at + line]).into_owned();
        let Ok(size) = usize::from_str_radix(size_text.trim(), 16) else {
            break;
        };
        let start = at + line + 2;
        if body.len() < start + size + 2 {
            break;
        }
        out.push(body[start..start + size].to_vec());
        at = start + size + 2;
        if size == 0 {
            break;
        }
    }
    out
}
