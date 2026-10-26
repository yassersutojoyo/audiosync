//! HTTPS fetches through WinHTTP (spec S294), behind a trait so tests feed
//! bytes without a socket.

use std::time::{Duration, Instant};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The whole appcast, and for a download the longest wait for a byte.
pub const APPCAST_TIMEOUT: Duration = Duration::from_secs(30);
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_APPCAST: usize = 1024 * 1024;
pub const MAX_REDIRECTS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Appcast,
    Download,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// No answer, a TLS failure, a timeout or a redirect it won't follow.
    Failed(String),
    Status(u16),
    /// The sink asked to stop.
    Stopped,
}

/// Bytes as they arrive; returns `false` to stop the fetch.
pub type Sink<'a> = dyn FnMut(&[u8]) -> bool + 'a;

pub trait Fetch: Send + Sync {
    fn get(&self, url: &str, kind: Kind, sink: &mut Sink<'_>) -> Result<(), FetchError>;
}

/// An `https:` URL's host, port and path with query.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Splits an `https:` URL; any other scheme is `None`.
pub fn https_target(url: &str) -> Option<Target> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains(['@', '[']) {
        return None;
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 443),
    };
    if host.is_empty() {
        return None;
    }
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_owned()
    };
    Some(Target {
        host: host.to_owned(),
        port,
        path,
    })
}

/// Where a redirect from `from` to `location` leads, if it stays on HTTPS.
pub fn redirect_target(from: &Target, location: &str) -> Option<String> {
    if location.starts_with('/') && !location.starts_with("//") {
        let port = if from.port == 443 {
            String::new()
        } else {
            format!(":{}", from.port)
        };
        return Some(format!("https://{}{port}{location}", from.host));
    }
    https_target(location).map(|_| location.to_owned())
}

/// The user agent, such as `AudioSync/0.8.0 (Windows; x64)`.
pub fn user_agent(version: &str, arch: &str) -> String {
    format!("AudioSync/{version} (Windows; {arch})")
}

/// Collects an appcast through `fetch`, refusing more than [`MAX_APPCAST`]
/// bytes and more than [`APPCAST_TIMEOUT`] in total.
pub fn appcast(fetch: &dyn Fetch, url: &str) -> Result<Vec<u8>, FetchError> {
    let started = Instant::now();
    let mut body = Vec::new();
    let mut over = None;
    let result = fetch.get(url, Kind::Appcast, &mut |bytes| {
        if body.len() + bytes.len() > MAX_APPCAST {
            over = Some("over 1 MiB");
            return false;
        }
        if started.elapsed() > APPCAST_TIMEOUT {
            over = Some("over 30 s");
            return false;
        }
        body.extend_from_slice(bytes);
        true
    });
    match (result, over) {
        (Err(FetchError::Stopped), Some(why)) => {
            Err(FetchError::Failed(format!("the appcast is {why}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), _) => Ok(body),
    }
}

pub struct WinHttp {
    pub user_agent: String,
}

impl Fetch for WinHttp {
    fn get(&self, url: &str, kind: Kind, sink: &mut Sink<'_>) -> Result<(), FetchError> {
        winhttp::get(&self.user_agent, url, kind, sink)
    }
}

mod winhttp {
    use std::ffi::c_void;

    use windows::Win32::Networking::WinHttp::*;
    use windows::core::{HSTRING, PCWSTR, w};

    use super::*;

    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a handle WinHTTP returned, closed once.
                let _ = unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    fn handle(raw: *mut c_void, what: &str) -> Result<Handle, FetchError> {
        if raw.is_null() {
            Err(FetchError::Failed(format!(
                "{what}: {}",
                windows::core::Error::from_thread()
            )))
        } else {
            Ok(Handle(raw))
        }
    }

    fn failed(what: &str) -> impl Fn(windows::core::Error) -> FetchError + '_ {
        move |error| FetchError::Failed(format!("{what}: {error}"))
    }

    fn millis(duration: Duration) -> i32 {
        i32::try_from(duration.as_millis()).unwrap_or(i32::MAX)
    }

    fn set_u32(handle: &Handle, option: u32, value: u32) -> Result<(), FetchError> {
        // SAFETY: the handle is open and the buffer is the option's u32.
        unsafe { WinHttpSetOption(Some(handle.0), option, Some(&value.to_ne_bytes())) }
            .map_err(failed("an option"))
    }

    fn query_u32(request: &Handle, level: u32) -> Option<u32> {
        let mut value = 0u32;
        let mut size = 4u32;
        // SAFETY: the buffer is a u32 and `size` says so.
        unsafe {
            WinHttpQueryHeaders(
                request.0,
                level | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&raw mut value).cast()),
                &mut size,
                std::ptr::null_mut(),
            )
        }
        .ok()
        .map(|()| value)
    }

    fn query_text(request: &Handle, level: u32) -> Option<String> {
        let mut buffer = vec![0u16; 4096];
        let mut size = (buffer.len() * 2) as u32;
        // SAFETY: `size` is the buffer's length in bytes.
        unsafe {
            WinHttpQueryHeaders(
                request.0,
                level,
                PCWSTR::null(),
                Some(buffer.as_mut_ptr().cast()),
                &mut size,
                std::ptr::null_mut(),
            )
        }
        .ok()?;
        buffer.truncate(size as usize / 2);
        Some(String::from_utf16_lossy(&buffer))
    }

    pub fn get(agent: &str, url: &str, kind: Kind, sink: &mut Sink<'_>) -> Result<(), FetchError> {
        // SAFETY: the strings live through the call.
        let session = handle(
            unsafe {
                WinHttpOpen(
                    &HSTRING::from(agent),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    0,
                )
            },
            "open",
        )?;
        let receive = match kind {
            Kind::Appcast => APPCAST_TIMEOUT,
            Kind::Download => STALL_TIMEOUT,
        };
        // SAFETY: the session handle is open.
        unsafe {
            WinHttpSetTimeouts(
                session.0,
                millis(CONNECT_TIMEOUT),
                millis(CONNECT_TIMEOUT),
                millis(receive),
                millis(receive),
            )
        }
        .map_err(failed("timeouts"))?;
        let mut url = url.to_owned();
        for _ in 0..=MAX_REDIRECTS {
            let target = https_target(&url)
                .ok_or_else(|| FetchError::Failed("not an https: URL".to_owned()))?;
            // SAFETY: the session is open; the host string lives through it.
            let connection = handle(
                unsafe {
                    WinHttpConnect(
                        session.0,
                        &HSTRING::from(target.host.as_str()),
                        target.port,
                        0,
                    )
                },
                "connect",
            )?;
            // SAFETY: the connection is open; the strings live through it.
            let request = handle(
                unsafe {
                    WinHttpOpenRequest(
                        connection.0,
                        w!("GET"),
                        &HSTRING::from(target.path.as_str()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        std::ptr::null(),
                        WINHTTP_FLAG_SECURE,
                    )
                },
                "request",
            )?;
            set_u32(
                &request,
                WINHTTP_OPTION_REDIRECT_POLICY,
                WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
            )?;
            set_u32(
                &request,
                WINHTTP_OPTION_DISABLE_FEATURE,
                WINHTTP_DISABLE_COOKIES,
            )?;
            set_u32(
                &request,
                WINHTTP_OPTION_AUTOLOGON_POLICY,
                WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH,
            )?;
            // SAFETY: the request is open and sends no body.
            unsafe { WinHttpSendRequest(request.0, None, None, 0, 0, 0) }
                .map_err(failed("send"))?;
            // SAFETY: the request was sent.
            unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }
                .map_err(failed("response"))?;
            let status = query_u32(&request, WINHTTP_QUERY_STATUS_CODE)
                .ok_or_else(|| FetchError::Failed("no status".to_owned()))?;
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let location = query_text(&request, WINHTTP_QUERY_LOCATION).ok_or_else(|| {
                    FetchError::Failed("a redirect without a location".to_owned())
                })?;
                url = redirect_target(&target, &location)
                    .ok_or_else(|| FetchError::Failed("a redirect off https".to_owned()))?;
                continue;
            }
            if status != 200 {
                return Err(FetchError::Status(u16::try_from(status).unwrap_or(0)));
            }
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let mut read = 0u32;
                // SAFETY: the buffer holds `len` bytes.
                unsafe {
                    WinHttpReadData(
                        request.0,
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut read,
                    )
                }
                .map_err(failed("read"))?;
                if read == 0 {
                    return Ok(());
                }
                if !sink(&buffer[..read as usize]) {
                    return Err(FetchError::Stopped);
                }
            }
        }
        Err(FetchError::Failed("more than 5 redirects".to_owned()))
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// Serves fixed bytes by URL and records each URL asked for.
    #[derive(Default)]
    pub struct MemoryFetch {
        pub files: HashMap<String, Vec<u8>>,
        pub asked: Mutex<Vec<String>>,
    }

    impl Fetch for MemoryFetch {
        fn get(&self, url: &str, _kind: Kind, sink: &mut Sink<'_>) -> Result<(), FetchError> {
            self.asked.lock().unwrap().push(url.to_owned());
            let body = self
                .files
                .get(url)
                .ok_or_else(|| FetchError::Failed("offline".to_owned()))?;
            for chunk in body.chunks(1000) {
                if !sink(chunk) {
                    return Err(FetchError::Stopped);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn the_feed_is_https_only_with_its_size_and_time_limits() {
        assert_eq!(
            https_target("https://github.com/o/r/releases/latest/download/appcast.xml"),
            Some(Target {
                host: "github.com".to_owned(),
                port: 443,
                path: "/o/r/releases/latest/download/appcast.xml".to_owned(),
            })
        );
        assert_eq!(
            https_target("https://h.invalid:8443?x=1").map(|t| (t.port, t.path)),
            Some((8443, "/?x=1".to_owned()))
        );
        for refused in [
            "http://github.com/a",
            "ftp://h/a",
            "file:///c:/a",
            "https://",
            "https://user@h/a",
        ] {
            assert_eq!(https_target(refused), None, "{refused}");
        }
        let from = https_target("https://github.com/a").unwrap();
        assert_eq!(redirect_target(&from, "http://objects.invalid/x"), None);
        assert_eq!(
            redirect_target(&from, "https://objects.invalid/x?sig=1").as_deref(),
            Some("https://objects.invalid/x?sig=1")
        );
        assert_eq!(
            redirect_target(&from, "/b").as_deref(),
            Some("https://github.com/b")
        );
        assert_eq!(MAX_REDIRECTS, 5);
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(15));
        assert_eq!(STALL_TIMEOUT, Duration::from_secs(30));
        assert_eq!(
            user_agent("0.8.0", "arm64"),
            "AudioSync/0.8.0 (Windows; arm64)"
        );

        let mut fetch = MemoryFetch::default();
        fetch.files.insert(
            "https://f.invalid/big".to_owned(),
            vec![b' '; MAX_APPCAST + 1],
        );
        fetch
            .files
            .insert("https://f.invalid/ok".to_owned(), vec![b' '; MAX_APPCAST]);
        assert!(matches!(
            appcast(&fetch, "https://f.invalid/big"),
            Err(FetchError::Failed(_))
        ));
        assert_eq!(
            appcast(&fetch, "https://f.invalid/ok").map(|b| b.len()),
            Ok(MAX_APPCAST)
        );
    }
}
