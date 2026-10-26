//! The host's hook on the driver thread. A normal-priority driver thread on
//! Windows woke more than 5 ms late on about 0.1% of packets under CPU load
//! (P84); the host raises the thread's priority through this hook.

use std::any::Any;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::sender::{Sender, SenderConfig};

static RAN_ON: Mutex<Option<String>> = Mutex::new(None);
static RELEASED: AtomicBool = AtomicBool::new(false);

struct Held;

impl Drop for Held {
    fn drop(&mut self) {
        RELEASED.store(true, Ordering::SeqCst);
    }
}

fn raise() -> Box<dyn Any> {
    *RAN_ON.lock().expect("lock") = thread::current().name().map(str::to_owned);
    Box::new(Held)
}

// covers: P84
#[test]
fn the_hook_runs_on_the_driver_thread_and_holds_its_value_until_the_driver_ends() {
    let (sender, _writer) = Sender::start(SenderConfig {
        on_driver_thread: Some(raise),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let deadline = Instant::now() + Duration::from_secs(5);
    while RAN_ON.lock().expect("lock").is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        RAN_ON.lock().expect("lock").as_deref(),
        Some("airplay-driver")
    );
    assert!(!RELEASED.load(Ordering::SeqCst), "held while the driver runs");
    drop(sender);
    assert!(RELEASED.load(Ordering::SeqCst), "released when it ends");
}
