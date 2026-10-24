//! One AudioSync per user on Linux, with the rules of
//! `crate::single_instance`: the first copy holds an exclusive `flock` on
//! `instance.lock` and listens on the Unix socket `instance.sock` next to
//! it, in `$XDG_RUNTIME_DIR/audiosync` (0700). A second launch connects,
//! writes `show <token>` and waits for `shown` once the main window is up.
//!
//! The kernel drops the lock with the last open descriptor, so a crash
//! frees it; only the lock holder removes and binds the socket, which makes
//! removing a stale one safe. Both files get the sticky bit, which keeps the
//! runtime folder's periodic clean-up from removing them while held.
//!
//! The token is the activation token a desktop hands the launch, so the
//! running copy may take the focus with it; winit has no call for that yet,
//! so on Wayland the window shows, maybe without the focus.

use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::single_instance::{Acquired, HandOver, MutexState, Startup};

const LOCK_FILE: &str = "instance.lock";
const SOCKET_FILE: &str = "instance.sock";

/// Owner read and write, plus the sticky bit that exempts the file from the
/// runtime folder's clean-up.
const HELD_FILE_MODE: u32 = 0o1600;

/// How long the first copy waits for a client's request line, so one that
/// sends nothing can't hold up the next launch.
const REQUEST_WAIT: Duration = Duration::from_secs(1);

/// The longest line either side reads.
const MAX_LINE: u64 = 512;

const ANSWER: &[u8] = b"shown\n";

/// This copy's hold on the session, from [`claim_instance`].
pub type Instance = crate::os::claim::Instance<Lock>;

/// The lock and the socket, claimed before the log and anything else.
pub fn claim_instance() -> Option<Instance> {
    let folder = instance_dir();
    let token = activation_token();
    crate::os::claim::claim(
        || match &folder {
            Ok(folder) => acquire_in(folder),
            Err(error) => Acquired::First(Err(format!("the instance folder: {error}"))),
        },
        || match &folder {
            Ok(folder) => hand_over_in(folder, token.as_deref()),
            Err(_) => HandOver::NotSent,
        },
    )
}

/// `$XDG_RUNTIME_DIR/audiosync`, or `audiosync-<uid>` in the temporary
/// folder when the runtime folder is missing or not private; created 0700
/// and refused when another account owns it.
fn instance_dir() -> io::Result<PathBuf> {
    let user =
        super::user_id().ok_or_else(|| io::Error::other("this process's user is unknown"))?;
    let folder = match super::runtime_base(std::env::var_os("XDG_RUNTIME_DIR"), user) {
        Some(runtime) => runtime.join(super::FOLDER_NAME),
        None => std::env::temp_dir().join(format!("{}-{user}", super::FOLDER_NAME)),
    };
    super::owned_private_dir(&folder, user)?;
    Ok(folder)
}

/// The first copy's lock and socket. Dropping it removes the socket file,
/// then releases the lock.
pub struct Lock {
    socket: PathBuf,
    listener: UnixListener,
    _file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

/// Takes the lock in `folder` and, when this is the first copy, binds the
/// socket. Mapped onto the Windows mutex's states, so the rules are the same.
pub fn acquire_in(folder: &Path) -> Acquired<Lock> {
    let locked = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(folder.join(LOCK_FILE))
        .and_then(|file| match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        });
    let state = match &locked {
        Ok(Some(_)) => MutexState::Created,
        Ok(None) => MutexState::Existed,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => MutexState::AccessDenied,
        Err(_) => MutexState::Failed,
    };
    match (Startup::decide(state), locked) {
        (Startup::HandOver, _) | (_, Ok(None)) => Acquired::Second,
        (Startup::Run, Ok(Some(file))) => Acquired::First(bind(folder, file)),
        (Startup::Run, Err(error)) => Acquired::First(Err(format!("the instance lock: {error}"))),
    }
}

fn bind(folder: &Path, file: File) -> Result<Lock, String> {
    let _ = fs::set_permissions(
        folder.join(LOCK_FILE),
        Permissions::from_mode(HELD_FILE_MODE),
    );
    let socket = folder.join(SOCKET_FILE);
    let listener = match fs::remove_file(&socket) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => UnixListener::bind(&socket),
    };
    match listener {
        Ok(listener) => {
            let _ = fs::set_permissions(&socket, Permissions::from_mode(HELD_FILE_MODE));
            Ok(Lock {
                socket,
                listener,
                _file: file,
            })
        }
        Err(error) => {
            // Held until the process ends, as the Windows mutex is: later
            // launches still find this copy, they just can't reach it.
            std::mem::forget(file);
            Err(format!("the instance socket {}: {error}", socket.display()))
        }
    }
}

impl Lock {
    /// Starts the listener thread: `on_show` gets the stream of each client
    /// that asked to show the main window, to answer once it is shown.
    pub fn listen_with(&self, on_show: impl Fn(UnixStream) + Send + 'static) {
        let Ok(listener) = self.listener.try_clone() else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("audiosync-single-instance".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            if read_request(&stream) {
                                on_show(stream);
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                            ) => {}
                        Err(_) => break,
                    }
                }
            });
    }
}

impl crate::os::claim::Listen for Lock {
    fn listen(&self, show: fn()) {
        self.listen_with(move |stream| {
            let _ = slint::invoke_from_event_loop(move || {
                show();
                let _ = (&stream).write_all(ANSWER);
            });
        });
    }
}

/// Whether the client asked to show the main window, within
/// [`REQUEST_WAIT`].
fn read_request(stream: &UnixStream) -> bool {
    if stream.set_read_timeout(Some(REQUEST_WAIT)).is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(Read::take(stream, MAX_LINE))
        .read_line(&mut line)
        .is_ok()
        && is_show_request(&line)
}

/// `show`, alone or followed by a space and the activation token.
fn is_show_request(line: &str) -> bool {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line == "show" || line.starts_with("show ")
}

/// The request line, with `token` when it is one word.
fn request_line(token: Option<&str>) -> String {
    let token = token.filter(|token| {
        !token.is_empty() && !token.chars().any(|c| c.is_whitespace() || c.is_control())
    });
    match token {
        Some(token) => format!("show {token}\n"),
        None => "show\n".to_owned(),
    }
}

/// The token the desktop handed this launch for taking the focus: Wayland's
/// activation token, else X11's startup notification id.
fn activation_token() -> Option<String> {
    ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|token| !token.is_empty()))
}

/// The second copy: asks the copy that listens in `folder` to show its
/// main window and waits at most `ANSWER_WAIT` for it.
pub fn hand_over_in(folder: &Path, token: Option<&str>) -> HandOver {
    let Ok(stream) = UnixStream::connect(folder.join(SOCKET_FILE)) else {
        return HandOver::NotSent;
    };
    HandOver::run(
        || (&stream).write_all(request_line(token).as_bytes()).is_ok(),
        |wait| {
            let mut line = String::new();
            stream.set_read_timeout(Some(wait)).is_ok()
                && BufReader::new(Read::take(&stream, MAX_LINE))
                    .read_line(&mut line)
                    .is_ok()
                && line.as_bytes() == ANSWER
        },
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::sync::mpsc;

    use super::*;

    /// A short folder for the socket, whose path must fit in 108 bytes;
    /// the build's target folder can be deeper than that.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> TestDir {
            let dir = std::env::temp_dir().join(format!("audiosync-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn first(acquired: Acquired<Lock>) -> Lock {
        match acquired {
            Acquired::First(lock) => lock.expect("the socket is bound"),
            Acquired::Second => panic!("another copy holds the lock"),
        }
    }

    #[test]
    fn a_held_lock_makes_every_other_launch_the_second_copy() {
        let dir = TestDir::new("instance-held");
        let lock = first(acquire_in(&dir.0));
        let folder = dir.0.clone();
        let in_thread = std::thread::spawn(move || matches!(acquire_in(&folder), Acquired::Second));
        assert!(in_thread.join().unwrap());
        assert!(matches!(acquire_in(&dir.0), Acquired::Second));

        let mode = |name: &str| fs::metadata(dir.0.join(name)).unwrap().mode() & 0o7777;
        assert_eq!(mode(LOCK_FILE), HELD_FILE_MODE);
        assert_eq!(mode(SOCKET_FILE), HELD_FILE_MODE);

        drop(lock);
        assert!(
            !dir.0.join(SOCKET_FILE).exists(),
            "a clean quit removes the socket"
        );
        drop(first(acquire_in(&dir.0)));
    }

    #[test]
    fn a_socket_left_by_a_crash_is_replaced_by_the_next_first_copy() {
        let dir = TestDir::new("instance-stale");
        drop(UnixListener::bind(dir.0.join(SOCKET_FILE)).unwrap());
        assert!(dir.0.join(SOCKET_FILE).exists());
        let _lock = first(acquire_in(&dir.0));
        assert_eq!(hand_over_in(&dir.0, None), HandOver::NoAnswer);
    }

    #[test]
    fn a_second_launch_gets_the_window_shown_and_passes_its_token() {
        let dir = TestDir::new("instance-shown");
        let lock = first(acquire_in(&dir.0));
        let (requests, received) = mpsc::channel();
        lock.listen_with(move |stream| {
            let _ = requests.send(());
            let _ = (&stream).write_all(ANSWER);
        });
        assert_eq!(hand_over_in(&dir.0, Some("token-1")), HandOver::Shown);
        assert_eq!(hand_over_in(&dir.0, None), HandOver::Shown);
        assert_eq!(received.try_iter().count(), 2);
    }

    #[test]
    fn a_copy_that_never_answers_or_isnt_listening_ends_the_hand_over() {
        let dir = TestDir::new("instance-silent");
        assert_eq!(hand_over_in(&dir.0, None), HandOver::NotSent);
        let lock = first(acquire_in(&dir.0));
        lock.listen_with(drop::<UnixStream>);
        assert_eq!(hand_over_in(&dir.0, None), HandOver::NoAnswer);
    }

    #[test]
    fn a_socket_path_too_long_to_bind_still_runs_the_app() {
        let dir = TestDir::new("instance-long");
        let deep = dir.0.join("d".repeat(120));
        fs::create_dir(&deep).unwrap();
        assert!(matches!(acquire_in(&deep), Acquired::First(Err(_))));
    }

    #[test]
    fn only_show_is_a_request_and_a_token_rides_along_as_one_word() {
        assert!(is_show_request("show\n"));
        assert!(is_show_request("show abc\n"));
        assert!(!is_show_request("shown\n"));
        assert!(!is_show_request("quit\n"));
        assert_eq!(request_line(None), "show\n");
        assert_eq!(request_line(Some("")), "show\n");
        assert_eq!(request_line(Some("a b")), "show\n");
        assert_eq!(request_line(Some("x11_1:2")), "show x11_1:2\n");
    }
}
