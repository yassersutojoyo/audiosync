//! Whether a device added by address answers (C18). No record vouches for
//! such a device, so a TCP connect to its address decides whether the list
//! shows it online. Each check runs on a thread of its own and
//! [`Reachability::poll`] only drains their results, so it never blocks.

use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{DeviceKey, DeviceList, ListEvent};

/// How long a finished check waits for the next one.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// How long a check waits for the device to accept the connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Tries to reach `address`; it may block up to its own timeout.
pub type Probe = fn(SocketAddr) -> bool;

/// The live check: the device accepts a TCP connection, which is closed
/// again at once without a byte sent (C9).
pub fn connects(address: SocketAddr) -> bool {
    TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_ok()
}

/// A finished check: the address, whether it was reached, and the
/// device's session changes when it started.
type Found = (SocketAddr, bool, u64);

struct Check {
    address: SocketAddr,
    running: bool,
    due: Instant,
    /// The device's session changes as this check last saw them.
    session_changes: u64,
}

/// Checks every device added by address that no session holds: at once,
/// then every [`CHECK_INTERVAL`], and at once again after a session on it.
/// A check that started before the latest session opened or closed is
/// dropped, since it says nothing about the device now.
pub struct Reachability {
    probe: Probe,
    checks: Vec<Check>,
    results: (mpsc::Sender<Found>, mpsc::Receiver<Found>),
}

impl Default for Reachability {
    fn default() -> Reachability {
        Reachability::new(connects)
    }
}

impl Reachability {
    pub fn new(probe: Probe) -> Reachability {
        Reachability {
            probe,
            checks: Vec::new(),
            results: mpsc::channel(),
        }
    }

    /// Takes in finished checks, then starts the ones due. Returns what
    /// changed in `list`.
    pub fn poll(&mut self, list: &mut DeviceList, now: Instant) -> Vec<ListEvent> {
        let mut events = Vec::new();
        while let Ok((address, reached, started_under)) = self.results.1.try_recv() {
            let stale = started_under != list.session_changes(address);
            if let Some(check) = self.checks.iter_mut().find(|c| c.address == address) {
                check.running = false;
                check.due = if stale { now } else { now + CHECK_INTERVAL };
            }
            if !stale {
                events.extend(list.set_reachable(address, reached));
            }
        }
        let manual: Vec<SocketAddr> = list
            .entries()
            .iter()
            .filter_map(|entry| match entry.key {
                DeviceKey::Manual(address) if !entry.in_session => Some(address),
                _ => None,
            })
            .collect();
        // A running check stays until its result is in, so one address
        // never has two at once.
        self.checks
            .retain(|check| check.running || manual.contains(&check.address));
        for address in manual {
            let session_changes = list.session_changes(address);
            let index = match self.checks.iter().position(|c| c.address == address) {
                Some(index) => index,
                None => {
                    self.checks.push(Check {
                        address,
                        running: false,
                        due: now,
                        session_changes,
                    });
                    self.checks.len() - 1
                }
            };
            let check = &mut self.checks[index];
            if check.session_changes != session_changes {
                check.session_changes = session_changes;
                check.due = now;
            }
            if check.running || check.due > now {
                continue;
            }
            let sender = self.results.0.clone();
            let probe = self.probe;
            let spawned = thread::Builder::new()
                .name("cast-reach".to_owned())
                .spawn(move || {
                    let _ = sender.send((address, probe(address), session_changes));
                });
            match spawned {
                Ok(_) => check.running = true,
                Err(_) => check.due = now + CHECK_INTERVAL,
            }
        }
        events
    }
}
