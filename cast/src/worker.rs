//! Threads whose work may panic (a bug) without leaving a device waiting.

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::thread::{self, JoinHandle};

/// Starts the thread `name` running `body`. A panic in `body` ends only this
/// thread, which then runs `on_panic` to report what a failure of its work
/// would.
pub(crate) fn spawn(
    name: &str,
    body: impl FnOnce() + Send + 'static,
    on_panic: impl FnOnce() + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        if panic::catch_unwind(AssertUnwindSafe(body)).is_err() {
            on_panic();
        }
    })
}
