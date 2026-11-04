#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};

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
