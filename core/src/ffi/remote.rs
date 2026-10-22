//! AudioSync's web remote ([`audiosync_remote`]) through the C interface,
//! for the macOS app, which reaches it through the core's static library the
//! way it reaches the senders.
//!
//! The host starts the server with its configuration as JSON and one
//! command callback, publishes a full snapshot as JSON whenever something
//! the page shows changes, and shows the pairing code the setting needs.
//! The callback runs on a server thread with the change as JSON; the host
//! applies it on its own main thread like a click in its window and
//! publishes the result. The server never calls the host for a read.
//!
//! Threads: every `asc_remote_*` call on a handle runs on one control thread
//! at a time, never an audio thread. [`asc_remote_stop`] waits for a
//! callback in progress and for the connection threads (about 0.5 s at
//! most), so the callback must not wait for the thread that stops the
//! server.
//!
//! Panics: a panic inside a call returns that call's failure value; one on
//! a connection thread ends only that connection.

use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{self, AssertUnwindSafe};
use std::ptr;

use audiosync_remote::{Command, Config, DEFAULT_PORT, RemoteError, RemoteServer};

/// The port the remote listens on unless the host's configuration names another.
pub const REMOTE_DEFAULT_PORT: u32 = DEFAULT_PORT as u32;

/// Why [`asc_remote_start`] returned null: it didn't.
pub const REMOTE_ERROR_NONE: i32 = 0;
/// The configuration doesn't read, or the callback is null.
pub const REMOTE_ERROR_CONFIG: i32 = 1;
/// Another program listens on the port; the user may pick another.
pub const REMOTE_ERROR_PORT_IN_USE: i32 = 2;
/// No address could be listened on for another reason.
pub const REMOTE_ERROR_LISTEN: i32 = 3;
/// No randomness for the pairing code.
pub const REMOTE_ERROR_RANDOM: i32 = 4;
/// A panic while starting.
pub const REMOTE_ERROR_FAILED: i32 = 5;

/// The host's command callback: `command_json` is a NUL-terminated UTF-8
/// string valid during the call only.
pub type AscRemoteCommand =
    Option<unsafe extern "C" fn(context: *mut c_void, command_json: *const c_char)>;

/// A running remote.
pub struct AscRemote {
    server: RemoteServer,
    code: CString,
    log: CString,
}

struct HostCommand {
    call: unsafe extern "C" fn(*mut c_void, *const c_char),
    context: *mut c_void,
}

// SAFETY: by the contract of asc_remote_start the host's callback and its
// context may be used from any thread until asc_remote_stop returns, and the
// server makes one call at a time and none after its stop.
unsafe impl Send for HostCommand {}
// SAFETY: as for Send: the server calls it one call at a time.
unsafe impl Sync for HostCommand {}

impl HostCommand {
    fn deliver(&self, command: &Command) {
        // serde_json escapes every control character, so the JSON holds no NUL.
        let Ok(json) = CString::new(command.to_json()) else {
            return;
        };
        // SAFETY: the host's contract (see the Send impl); the string lives
        // through the call.
        unsafe { (self.call)(self.context, json.as_ptr()) };
    }
}

/// # Safety
///
/// `text` is null or points to a NUL-terminated string that stays valid and
/// unchanged during the call.
unsafe fn host_text<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    // SAFETY: non-null, and by the caller's contract NUL-terminated.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// Runs `body` with the handle behind `remote`, stopping a panic at this
/// boundary: `failed` for a null handle and after a panic.
///
/// # Safety
///
/// `remote` is null or a live handle, called under the threading contract.
unsafe fn with_remote<T>(
    remote: *mut AscRemote,
    failed: T,
    body: impl FnOnce(&mut AscRemote) -> T,
) -> T {
    if remote.is_null() {
        return failed;
    }
    // SAFETY: non-null, and by the caller's contract a live handle that no
    // other call uses now.
    let remote = unsafe { &mut *remote };
    panic::catch_unwind(AssertUnwindSafe(|| body(remote))).unwrap_or(failed)
}

/// Keeps `text` in `slot` and returns it as a C string the handle owns.
fn keep(slot: &mut CString, text: String) -> *const c_char {
    match CString::new(text) {
        Ok(kept) => {
            *slot = kept;
            slot.as_ptr()
        }
        Err(_) => ptr::null(),
    }
}

/// Starts the remote: `config_json` is `{"port"?, "local_name"?,
/// "devices_path"?, "listen"?}` (the defaults: [`REMOTE_DEFAULT_PORT`], no
/// `.local` name, devices kept in memory only, every IPv6 and IPv4
/// address). `command` gets each accepted change with `context`. Null on
/// failure, with the reason in `*error` (one of the `REMOTE_ERROR_` codes)
/// when `error` isn't null.
///
/// # Safety
///
/// `config_json` is null or a NUL-terminated string, unchanged during the
/// call; `error` is null or points to a writable `i32`. `command` and
/// `context` may be used from any thread until [`asc_remote_stop`] returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_start(
    config_json: *const c_char,
    command: AscRemoteCommand,
    context: *mut c_void,
    error: *mut i32,
) -> *mut AscRemote {
    // SAFETY: as the caller's.
    let text = unsafe { host_text(config_json) };
    let started = panic::catch_unwind(AssertUnwindSafe(|| {
        let config = Config::from_json(text.unwrap_or("{}")).ok_or(REMOTE_ERROR_CONFIG)?;
        let call = command.ok_or(REMOTE_ERROR_CONFIG)?;
        let host = HostCommand { call, context };
        let server = RemoteServer::start(config, move |command: &Command| host.deliver(command))
            .map_err(|problem| match problem {
                RemoteError::Bind(_) if problem.port_in_use() => REMOTE_ERROR_PORT_IN_USE,
                RemoteError::Bind(_) => REMOTE_ERROR_LISTEN,
                RemoteError::Random => REMOTE_ERROR_RANDOM,
                RemoteError::Page(_) => REMOTE_ERROR_FAILED,
            })?;
        Ok(Box::into_raw(Box::new(AscRemote {
            server,
            code: CString::default(),
            log: CString::default(),
        })))
    }));
    let (handle, code) = match started {
        Ok(Ok(handle)) => (handle, REMOTE_ERROR_NONE),
        Ok(Err(code)) => (ptr::null_mut(), code),
        Err(_) => (ptr::null_mut(), REMOTE_ERROR_FAILED),
    };
    if !error.is_null() {
        // SAFETY: by the caller's contract a writable i32.
        unsafe { error.write(code) };
    }
    handle
}

/// The port the remote listens on; 0 for a null handle.
///
/// # Safety
///
/// `remote` is null or a live handle from [`asc_remote_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_port(remote: *mut AscRemote) -> u32 {
    // SAFETY: as the caller's.
    unsafe { with_remote(remote, 0, |remote| u32::from(remote.server.port())) }
}

/// Publishes the snapshot in `state_json` (the API's state without
/// `revision`) and streams it to the open pages. False for a null handle or
/// a snapshot that doesn't read; the previous one then stays.
///
/// # Safety
///
/// `remote` is null or a live handle; `state_json` is null or a
/// NUL-terminated string, unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_publish(
    remote: *mut AscRemote,
    state_json: *const c_char,
) -> bool {
    // SAFETY: as the caller's.
    let Some(text) = (unsafe { host_text(state_json) }) else {
        return false;
    };
    // SAFETY: as the caller's.
    unsafe {
        with_remote(remote, false, |remote| {
            remote.server.publish_json(text).is_ok()
        })
    }
}

/// The pairing code that works now, 6 digits; null when none does (it
/// expired: call [`asc_remote_rotate_code`]) or for a null handle. The
/// string is the handle's, valid until the next code call on it or its stop.
///
/// # Safety
///
/// `remote` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_pairing_code(remote: *mut AscRemote) -> *const c_char {
    // SAFETY: as the caller's.
    unsafe {
        with_remote(remote, ptr::null(), |remote| {
            match remote.server.pairing_code() {
                Some(code) => keep(&mut remote.code, code),
                None => ptr::null(),
            }
        })
    }
}

/// Makes a new pairing code, for when the user opens the setting: the old
/// one stops working and a lockout after too many wrong codes ends. Returns
/// it as [`asc_remote_pairing_code`] does; null when no code could be made.
///
/// # Safety
///
/// `remote` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_rotate_code(remote: *mut AscRemote) -> *const c_char {
    // SAFETY: as the caller's.
    unsafe {
        with_remote(remote, ptr::null(), |remote| {
            match remote.server.rotate_code() {
                Ok(code) => keep(&mut remote.code, code),
                Err(_) => ptr::null(),
            }
        })
    }
}

/// Whether too many wrong codes locked pairing until the next
/// [`asc_remote_rotate_code`].
///
/// # Safety
///
/// `remote` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_pairing_locked(remote: *mut AscRemote) -> bool {
    // SAFETY: as the caller's.
    unsafe { with_remote(remote, false, |remote| remote.server.pairing_locked()) }
}

/// How many devices are paired (8 at most).
///
/// # Safety
///
/// `remote` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_device_count(remote: *mut AscRemote) -> u32 {
    // SAFETY: as the caller's.
    unsafe {
        with_remote(remote, 0, |remote| {
            u32::try_from(remote.server.device_count()).unwrap_or(u32::MAX)
        })
    }
}

/// Forgets every paired device: their next request is refused and their
/// open pages lose the stream. False when the device file couldn't be
/// written (they are forgotten until the app quits) or for a null handle.
///
/// # Safety
///
/// `remote` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_forget_all(remote: *mut AscRemote) -> bool {
    // SAFETY: as the caller's.
    unsafe { with_remote(remote, false, |remote| remote.server.forget_all().is_ok()) }
}

/// The oldest debug-log line not taken yet (pairing, refusals, file
/// problems; never a code or a token), or null when there is none. The
/// string is the handle's, valid until the next call of this on it or its
/// stop.
///
/// # Safety
///
/// `remote` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_next_log(remote: *mut AscRemote) -> *const c_char {
    // SAFETY: as the caller's.
    unsafe {
        with_remote(remote, ptr::null(), |remote| {
            match remote.server.poll_event() {
                Some(event) => keep(&mut remote.log, event.to_string()),
                None => ptr::null(),
            }
        })
    }
}

/// Stops the remote and frees it: closes the port and every connection,
/// and waits for a command callback in progress; once it returns the
/// callback is never called again. Takes the address of the host's pointer
/// and sets that pointer to null; a null address or pointer does nothing.
///
/// # Safety
///
/// `remote` is null or points to a pointer that is null or was returned by
/// [`asc_remote_start`] and not stopped yet; no other call on that handle
/// runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_remote_stop(remote: *mut *mut AscRemote) {
    if remote.is_null() {
        return;
    }
    // SAFETY: by the caller's contract `remote` points to a readable and
    // writable handle pointer.
    let handle = unsafe { remote.replace(ptr::null_mut()) };
    if handle.is_null() {
        return;
    }
    // SAFETY: from Box::into_raw in asc_remote_start, stopped once (the
    // host's pointer is now null), with no call in progress.
    let owned = unsafe { Box::from_raw(handle) };
    let stopped = panic::catch_unwind(AssertUnwindSafe(move || owned.server.stop()));
    // What a panic left unfreed stays behind; the host's pointer is null.
    drop(stopped);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpStream};
    use std::sync::Mutex;
    use std::time::Duration;

    const STATE: &CStr = c"{\"master\": {\"volume\": 50}, \"now_playing\": null, \"speakers\": [{\"id\": \"den\", \"name\": \"Den\", \"kind\": \"local\", \"on\": true, \"volume\": 40, \"delay_ms\": 0, \"delay_min_ms\": 0, \"delay_max_ms\": 500, \"available\": true, \"notice\": null}]}";

    unsafe extern "C" fn record(context: *mut c_void, command_json: *const c_char) {
        // SAFETY: the tests pass a live Mutex<Vec<String>> as the context and
        // the server a NUL-terminated string.
        let (received, json) = unsafe {
            (
                &*(context as *const Mutex<Vec<String>>),
                CStr::from_ptr(command_json),
            )
        };
        received
            .lock()
            .unwrap()
            .push(json.to_str().unwrap().to_owned());
    }

    fn exchange(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    fn post(port: u16, path: &str, cookie: &str, body: &str) -> String {
        exchange(
            port,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nCookie: {cookie}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    #[test]
    fn a_change_posted_on_the_page_reaches_the_host_callback() {
        let received = Mutex::new(Vec::<String>::new());
        let context = &received as *const Mutex<Vec<String>> as *mut c_void;
        let mut error = -1;
        // SAFETY: a C string literal, a live context that outlives the stop,
        // a writable error.
        let mut remote = unsafe {
            asc_remote_start(
                c"{\"port\": 0, \"listen\": [\"127.0.0.1\"], \"local_name\": \"kitchen\"}".as_ptr(),
                Some(record),
                context,
                &mut error,
            )
        };
        assert!(!remote.is_null());
        assert_eq!(error, REMOTE_ERROR_NONE);
        // SAFETY: a live handle and a C string literal.
        unsafe {
            assert!(asc_remote_publish(remote, STATE.as_ptr()));
            assert!(!asc_remote_publish(remote, c"{}".as_ptr()));
            assert!(!asc_remote_publish(remote, ptr::null()));
        }
        // SAFETY: a live handle.
        let port = u16::try_from(unsafe { asc_remote_port(remote) }).unwrap();
        assert_ne!(port, 0);
        // SAFETY: a live handle; the string is copied before the next call.
        let code = unsafe { CStr::from_ptr(asc_remote_pairing_code(remote)) }
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(code.len(), 6);

        let paired = post(port, "/api/pair", "", &format!("{{\"code\": \"{code}\"}}"));
        assert!(paired.starts_with("HTTP/1.1 200 "), "{paired}");
        let cookie = paired
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))
            .and_then(|value| value.split(';').next())
            .unwrap()
            .to_owned();
        // SAFETY: a live handle.
        assert_eq!(unsafe { asc_remote_device_count(remote) }, 1);

        let changed = post(port, "/api/speakers/den", &cookie, "{\"delay_ms\": 250}");
        assert!(changed.starts_with("HTTP/1.1 202 "), "{changed}");
        {
            let received = received.lock().unwrap();
            assert_eq!(received.len(), 1);
            let command = &received[0];
            for part in ["\"type\":\"speaker\"", "\"id\":\"den\"", "\"delay_ms\":250"] {
                assert!(command.contains(part), "{command}");
            }
            assert!(!command.contains("volume") && !command.contains("\"on\""));
        }

        let mut lines = Vec::new();
        loop {
            // SAFETY: a live handle.
            let line = unsafe { asc_remote_next_log(remote) };
            if line.is_null() {
                break;
            }
            // SAFETY: the handle's string, copied before the next call.
            lines.push(
                unsafe { CStr::from_ptr(line) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert!(
            lines.iter().any(|line| line.contains("paired")),
            "{lines:?}"
        );
        assert!(lines.iter().all(|line| !line.contains(&code)));

        // SAFETY: a live handle.
        unsafe {
            let fresh = CStr::from_ptr(asc_remote_rotate_code(remote));
            assert_ne!(fresh.to_str().unwrap(), code);
            assert!(!asc_remote_pairing_locked(remote));
            assert!(asc_remote_forget_all(remote));
            assert_eq!(asc_remote_device_count(remote), 0);
        }
        let refused = post(port, "/api/speakers/den", &cookie, "{\"on\": false}");
        assert!(refused.starts_with("HTTP/1.1 401 "), "{refused}");

        // SAFETY: the host's pointer to a live handle; it becomes null.
        unsafe { asc_remote_stop(&mut remote) };
        assert!(remote.is_null());
        // SAFETY: a null pointer and a null address do nothing.
        unsafe {
            asc_remote_stop(&mut remote);
            asc_remote_stop(ptr::null_mut());
        }
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
        assert_eq!(received.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_bad_start_says_why() {
        let start = |config: &CStr, command: AscRemoteCommand| {
            let mut error = -1;
            // SAFETY: a C string literal and a writable error.
            let remote =
                unsafe { asc_remote_start(config.as_ptr(), command, ptr::null_mut(), &mut error) };
            assert!(remote.is_null());
            error
        };
        assert_eq!(
            start(c"{\"port\": \"x\"}", Some(record)),
            REMOTE_ERROR_CONFIG
        );
        assert_eq!(start(c"{\"port\": 0}", None), REMOTE_ERROR_CONFIG);
        assert_eq!(
            start(c"{\"port\": 0, \"listen\": []}", Some(record)),
            REMOTE_ERROR_LISTEN
        );
        // SAFETY: null handles do nothing and give the failure values.
        unsafe {
            assert!(asc_remote_pairing_code(ptr::null_mut()).is_null());
            assert!(!asc_remote_publish(ptr::null_mut(), STATE.as_ptr()));
            assert_eq!(asc_remote_port(ptr::null_mut()), 0);
            assert!(asc_remote_next_log(ptr::null_mut()).is_null());
        }
    }

    #[test]
    fn a_taken_port_is_reported_as_in_use() {
        let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let config =
            CString::new(format!("{{\"port\": {port}, \"listen\": [\"127.0.0.1\"]}}")).unwrap();
        let mut error = -1;
        // SAFETY: a live C string and a writable error.
        let remote =
            unsafe { asc_remote_start(config.as_ptr(), Some(record), ptr::null_mut(), &mut error) };
        assert!(remote.is_null());
        assert_eq!(error, REMOTE_ERROR_PORT_IN_USE);
    }
}
