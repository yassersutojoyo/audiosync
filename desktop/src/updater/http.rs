//! One HTTP/1.1 GET over a stream the caller opened: the Linux updater's
//! fetch, since Linux has no system HTTP stack such as WinHTTP (spec S424).
#![cfg_attr(windows, allow(dead_code))]

use std::io::{self, Read, Write};
use std::time::Instant;

use super::fetch::{FetchError, Sink, Target, redirect_target};

/// The most bytes a header block or a chunk-size line may take.
const MAX_HEAD: usize = 64 * 1024;

/// One request for `target` on `stream`: the next URL for a redirect,
/// `None` once the body went to `sink`. Past `deadline` the read fails.
pub fn get_on<S: Read + Write>(
    stream: &mut S,
    target: &Target,
    user_agent: &str,
    deadline: Option<Instant>,
    sink: &mut Sink<'_>,
) -> Result<Option<String>, FetchError> {
    let host = if target.port == 443 {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {user_agent}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        target.path
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(failed)?;
    let mut reader = Reader {
        stream,
        buffer: Vec::new(),
        deadline,
    };
    let head = reader.until(b"\r\n\r\n", "the answer ended in its headers")?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| {
            let mut parts = line.splitn(3, ' ');
            if !parts.next()?.starts_with("HTTP/1.") {
                return None;
            }
            parts.next()?.parse::<u16>().ok()
        })
        .ok_or_else(|| fail("not an HTTP answer"))?;
    let (mut length, mut chunked, mut location) = (None, false, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => {
                length = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| fail("a Content-Length that isn't a number"))?,
                );
            }
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "location" => location = Some(value.to_owned()),
            _ => {}
        }
    }
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        let location = location.ok_or_else(|| fail("a redirect without a Location"))?;
        return redirect_target(target, &location)
            .map(Some)
            .ok_or_else(|| fail(&format!("a redirect to {location}, not HTTPS")));
    }
    if status != 200 {
        return Err(FetchError::Status(status));
    }
    if chunked {
        reader.chunked(sink)?;
    } else if let Some(length) = length {
        reader.sized(length, sink)?;
    } else {
        reader.rest(sink)?;
    }
    Ok(None)
}

fn fail(why: &str) -> FetchError {
    FetchError::Failed(why.to_owned())
}

fn failed(error: io::Error) -> FetchError {
    FetchError::Failed(error.to_string())
}

struct Reader<'a, S> {
    stream: &'a mut S,
    buffer: Vec<u8>,
    deadline: Option<Instant>,
}

impl<S: Read> Reader<'_, S> {
    /// Reads more into the buffer; false at the end of the stream.
    fn fill(&mut self) -> Result<bool, FetchError> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(fail("the appcast is over 30 s"));
        }
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(read) => {
                    self.buffer.extend_from_slice(&chunk[..read]);
                    return Ok(read > 0);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(failed(error)),
            }
        }
    }

    /// The text up to `end`, which is taken off the buffer with it.
    fn until(&mut self, end: &[u8], short: &str) -> Result<String, FetchError> {
        let mut searched = 0;
        loop {
            if let Some(at) = self.buffer[searched..]
                .windows(end.len())
                .position(|window| window == end)
            {
                let at = searched + at;
                let text = String::from_utf8_lossy(&self.buffer[..at]).into_owned();
                self.buffer.drain(..at + end.len());
                return Ok(text);
            }
            if self.buffer.len() > MAX_HEAD {
                return Err(fail("an answer line over 64 KiB"));
            }
            searched = self.buffer.len().saturating_sub(end.len() - 1);
            if !self.fill()? {
                return Err(fail(short));
            }
        }
    }

    /// Hands the buffered bytes to `sink`, at most `limit` of them.
    fn pass(&mut self, limit: u64, sink: &mut Sink<'_>) -> Result<u64, FetchError> {
        let count = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(self.buffer.len());
        if count > 0 && !sink(&self.buffer[..count]) {
            return Err(FetchError::Stopped);
        }
        self.buffer.drain(..count);
        Ok(count as u64)
    }

    fn sized(&mut self, mut left: u64, sink: &mut Sink<'_>) -> Result<(), FetchError> {
        loop {
            left -= self.pass(left, sink)?;
            if left == 0 {
                return Ok(());
            }
            if !self.fill()? {
                return Err(fail("the body ended short of its length"));
            }
        }
    }

    fn rest(&mut self, sink: &mut Sink<'_>) -> Result<(), FetchError> {
        loop {
            self.pass(u64::MAX, sink)?;
            if !self.fill()? {
                return Ok(());
            }
        }
    }

    fn chunked(&mut self, sink: &mut Sink<'_>) -> Result<(), FetchError> {
        let short = "the chunked body ended early";
        loop {
            let line = self.until(b"\r\n", short)?;
            let size = line.split(';').next().unwrap_or("").trim();
            let size = u64::from_str_radix(size, 16).map_err(|_| fail("a bad chunk size"))?;
            if size == 0 {
                return Ok(());
            }
            self.sized(size, sink)?;
            if !self.until(b"\r\n", short)?.is_empty() {
                return Err(fail("a chunk longer than its size"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers with `answer` a few bytes per read and keeps what was written.
    struct Duplex {
        answer: Vec<u8>,
        at: usize,
        written: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let n = buffer.len().min(7).min(self.answer.len() - self.at);
            buffer[..n].copy_from_slice(&self.answer[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn target() -> Target {
        Target {
            host: "github.com".to_owned(),
            port: 443,
            path: "/o/r/appcast.xml".to_owned(),
        }
    }

    /// The hop's result and the body the sink got.
    fn get(answer: &str) -> (Result<Option<String>, FetchError>, Vec<u8>, String) {
        let mut stream = Duplex {
            answer: answer.as_bytes().to_vec(),
            at: 0,
            written: Vec::new(),
        };
        let mut body = Vec::new();
        let result = get_on(
            &mut stream,
            &target(),
            "AudioSync/0.8.0 (Linux; x64)",
            None,
            &mut |bytes| {
                body.extend_from_slice(bytes);
                true
            },
        );
        (result, body, String::from_utf8(stream.written).unwrap())
    }

    // covers: S424
    #[test]
    fn a_get_reads_a_sized_or_chunked_body_and_sends_no_cookie() {
        let (result, body, request) =
            get("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSet-Cookie: a=b\r\n\r\nhello and more");
        assert_eq!(result, Ok(None));
        assert_eq!(body, b"hello");
        assert!(
            request.starts_with(
                "GET /o/r/appcast.xml HTTP/1.1\r\nHost: github.com\r\nUser-Agent: AudioSync/0.8.0 (Linux; x64)\r\n"
            ),
            "{request}"
        );
        assert!(request.ends_with("\r\n\r\n"));
        assert!(!request.to_ascii_lowercase().contains("cookie"));

        let (result, body, _) = get(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n6;x=y\r\n world\r\n0\r\n\r\n",
        );
        assert_eq!(result, Ok(None));
        assert_eq!(body, b"hello world");

        let (result, body, _) = get("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nto the end");
        assert_eq!(result, Ok(None));
        assert_eq!(body, b"to the end");
    }

    // covers: S424
    #[test]
    fn a_redirect_stays_on_https_and_any_other_status_or_a_short_body_fails() {
        let (result, body, _) = get(
            "HTTP/1.1 302 Found\r\nLocation: https://objects.example.invalid/f?x=1\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(
            result,
            Ok(Some("https://objects.example.invalid/f?x=1".to_owned()))
        );
        assert!(body.is_empty());
        let (result, _, _) = get("HTTP/1.1 301 Moved\r\nlocation: /elsewhere\r\n\r\n");
        assert_eq!(result, Ok(Some("https://github.com/elsewhere".to_owned())));
        let (result, _, _) = get("HTTP/1.1 302 Found\r\nLocation: http://plain.invalid/\r\n\r\n");
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");
        let (result, _, _) = get("HTTP/1.1 302 Found\r\n\r\n");
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");

        assert_eq!(
            get("HTTP/1.1 404 Not Found\r\n\r\n").0,
            Err(FetchError::Status(404))
        );
        assert_eq!(
            get("HTTP/1.1 201 Created\r\n\r\n").0,
            Err(FetchError::Status(201))
        );
        let (result, _, _) = get("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort");
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");
        let (result, _, _) = get("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel");
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");
        let (result, _, _) = get("not http at all\r\n\r\n");
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");
        let endless = format!("HTTP/1.1 200 OK\r\nX: {}", "a".repeat(70_000));
        assert!(matches!(get(&endless).0, Err(FetchError::Failed(_))));

        let mut stream = Duplex {
            answer: b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec(),
            at: 0,
            written: Vec::new(),
        };
        let stopped = get_on(&mut stream, &target(), "UA", None, &mut |_| false);
        assert_eq!(stopped, Err(FetchError::Stopped));
    }
}
