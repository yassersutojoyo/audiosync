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

/// Fills the buffer within `wait` when given; returns the count, 0 at the end.
type Read<'a> = dyn FnMut(&mut [u8], Option<Duration>) -> Result<usize, FetchError> + 'a;

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
/// bytes; the fetch itself keeps [`APPCAST_TIMEOUT`].
pub fn appcast(fetch: &dyn Fetch, url: &str) -> Result<Vec<u8>, FetchError> {
    let mut body = Vec::new();
    let mut over = None;
    let result = fetch.get(url, Kind::Appcast, &mut |bytes| {
        if body.len() + bytes.len() > MAX_APPCAST {
            over = Some("over 1 MiB");
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

/// Reads a body to its end, by `deadline` when given: a per-read timeout
/// alone lets a trickling server stretch the whole body.
fn read_body(
    deadline: Option<Instant>,
    now: &dyn Fn() -> Instant,
    read: &mut Read<'_>,
    sink: &mut Sink<'_>,
) -> Result<(), FetchError> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = read(&mut buffer, time_left(deadline, now)?)?;
        if read == 0 {
            return Ok(());
        }
        if !sink(&buffer[..read]) {
            return Err(FetchError::Stopped);
        }
    }
}

/// One request: the next URL for a redirect, `None` once its body is read.
type Hop<'a> = dyn FnMut(&str, Option<Duration>) -> Result<Option<String>, FetchError> + 'a;

/// The time left before `deadline`, failing once it has passed.
fn time_left(
    deadline: Option<Instant>,
    now: &dyn Fn() -> Instant,
) -> Result<Option<Duration>, FetchError> {
    let left = deadline.map(|deadline| deadline.saturating_duration_since(now()));
    if left.is_some_and(|left| left.is_zero()) {
        return Err(FetchError::Failed("the appcast is over 30 s".to_owned()));
    }
    Ok(left)
}

/// Follows redirects from `url` up to [`MAX_REDIRECTS`] hops, each hop
/// bounded by what is left of `deadline`.
fn follow(
    url: &str,
    deadline: Option<Instant>,
    now: &dyn Fn() -> Instant,
    hop: &mut Hop<'_>,
) -> Result<(), FetchError> {
    let mut url = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        match hop(&url, time_left(deadline, now)?)? {
            Some(next) => url = next,
            None => return Ok(()),
        }
    }
    Err(FetchError::Failed("more than 5 redirects".to_owned()))
}

pub struct WinHttp {
    pub user_agent: String,
    /// Also plain `http://127.0.0.1:<port>` URLs, for the loopback test's
    /// server only; the app never sets it.
    pub plain_loopback: bool,
}

impl Fetch for WinHttp {
    fn get(&self, url: &str, kind: Kind, sink: &mut Sink<'_>) -> Result<(), FetchError> {
        winhttp::get(&self.user_agent, self.plain_loopback, url, kind, sink)
    }
}

/// A plain `http://127.0.0.1:<port>/path` URL's target.
fn loopback_target(url: &str) -> Option<Target> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let (port, path) = rest.split_at(rest.find('/')?);
    Some(Target {
        host: "127.0.0.1".to_owned(),
        port: port.parse().ok()?,
        path: path.to_owned(),
    })
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

    pub fn get(
        agent: &str,
        plain_loopback: bool,
        url: &str,
        kind: Kind,
        sink: &mut Sink<'_>,
    ) -> Result<(), FetchError> {
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
        let deadline = (kind == Kind::Appcast).then(|| Instant::now() + APPCAST_TIMEOUT);
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
        let mut hop = |url: &str, wait: Option<Duration>| -> Result<Option<String>, FetchError> {
            let (target, secure) = match https_target(url) {
                Some(target) => (target, true),
                None => match loopback_target(url).filter(|_| plain_loopback) {
                    Some(target) => (target, false),
                    None => return Err(FetchError::Failed("not an https: URL".to_owned())),
                },
            };
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
                        if secure {
                            WINHTTP_FLAG_SECURE
                        } else {
                            WINHTTP_OPEN_REQUEST_FLAGS(0)
                        },
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
            if let Some(left) = wait {
                let left = millis(left).max(1);
                let connect = left.min(millis(CONNECT_TIMEOUT));
                // SAFETY: the request is open.
                unsafe { WinHttpSetTimeouts(request.0, connect, connect, left, left) }
                    .map_err(failed("timeouts"))?;
                // The receive timeout covers one socket read; this one all the headers.
                set_u32(
                    &request,
                    WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT,
                    left as u32,
                )?;
            }
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
                return Ok(Some(if secure {
                    redirect_target(&target, &location)
                        .ok_or_else(|| FetchError::Failed("a redirect off https".to_owned()))?
                } else {
                    format!("http://127.0.0.1:{}{location}", target.port)
                }));
            }
            if status != 200 {
                return Err(FetchError::Status(u16::try_from(status).unwrap_or(0)));
            }
            let mut read = |buffer: &mut [u8], wait: Option<Duration>| {
                if let Some(wait) = wait {
                    set_u32(
                        &request,
                        WINHTTP_OPTION_RECEIVE_TIMEOUT,
                        millis(wait).max(1) as u32,
                    )?;
                }
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
                Ok(read as usize)
            };
            read_body(deadline, &Instant::now, &mut read, sink)?;
            Ok(None)
        };
        follow(url, deadline, &Instant::now, &mut hop)
    }
}

#[cfg(test)]
pub mod tests {
    use std::cell::Cell;
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

    // covers: S294
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

    // covers: S294
    #[test]
    fn a_trickled_appcast_fails_at_its_deadline() {
        let start = Instant::now();
        let deadline = start + APPCAST_TIMEOUT;
        let clock = Cell::new(start);
        let gap = Duration::from_secs(20);
        let mut sent = 0;
        // A server that sends one byte every 20 s for three minutes.
        let mut read = |buffer: &mut [u8], wait: Option<Duration>| {
            if let Some(wait) = wait.filter(|wait| *wait < gap) {
                clock.set(clock.get() + wait);
                return Err(FetchError::Failed("timed out".to_owned()));
            }
            clock.set(clock.get() + gap);
            sent += 1;
            if sent > 9 {
                return Ok(0);
            }
            buffer[0] = b'<';
            Ok(1)
        };
        let result = read_body(Some(deadline), &|| clock.get(), &mut read, &mut |_| true);
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");
        assert!(clock.get() <= deadline);
    }

    // covers: S294
    #[test]
    fn slow_headers_across_redirects_fail_at_the_deadline() {
        let start = Instant::now();
        let deadline = start + APPCAST_TIMEOUT;
        let clock = Cell::new(start);
        let gap = Duration::from_secs(20);
        let mut hops = 0;
        // Each hop's headers take 20 s: two redirects, then the appcast.
        let mut hop = |_url: &str, wait: Option<Duration>| {
            if let Some(wait) = wait.filter(|wait| *wait < gap) {
                clock.set(clock.get() + wait);
                return Err(FetchError::Failed("timed out".to_owned()));
            }
            clock.set(clock.get() + gap);
            hops += 1;
            Ok((hops < 3).then(|| format!("https://f.invalid/{hops}")))
        };
        let result = follow(
            "https://f.invalid/0",
            Some(deadline),
            &|| clock.get(),
            &mut hop,
        );
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");
        assert!(clock.get() <= deadline);
    }
}
