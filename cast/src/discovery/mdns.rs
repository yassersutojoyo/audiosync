//! Live browsing of `_googlecast._tcp` through the `mdns-sd` crate (C10).
//!
//! The daemon follows PTR records to SRV, TXT and address records itself
//! and reports an instance once it is resolved; [`browse_event`] turns that
//! report into a [`BrowseEvent`]. The daemon runs on its own thread, and
//! [`MdnsBrowser::poll_event`] only drains its channel, so it never blocks.
//! TXT values come from the daemon as raw bytes, so no DNS presentation
//! escaping reaches the sender (C12). A removal waits for a fresh query
//! to go unanswered before it is reported (C134).

use std::fmt;
use std::net::IpAddr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};

use super::{BrowseEvent, MAX_DEVICES, SERVICE_TYPE, ServiceRecord};

/// The live browser could not start.
#[derive(Debug)]
pub struct DiscoveryError(mdns_sd::Error);

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mDNS browsing could not start: {}", self.0)
    }
}

impl std::error::Error for DiscoveryError {}

/// How long a removed instance has to answer a fresh query before its
/// removal is reported (C134): RFC 6762 §10.4 flushes a record that ten
/// seconds of queries leave unanswered.
pub const CONFIRM_WAIT: Duration = Duration::from_secs(10);

/// The shortest time between two fresh queries. The daemon repeats each
/// query after one and three seconds, inside every wait it covers.
const QUERY_GAP: Duration = Duration::from_secs(1);

enum Source {
    /// The current browse, and the one a fresh query replaced, drained until
    /// the next query.
    Daemon {
        current: mdns_sd::Receiver<ServiceEvent>,
        previous: Option<mdns_sd::Receiver<ServiceEvent>>,
    },
    Channel(mpsc::Receiver<ServiceEvent>),
}

/// Browses `_googlecast._tcp.local.` and hands over what it finds.
pub struct MdnsBrowser {
    source: Source,
    daemon: Option<ServiceDaemon>,
    dead: bool,
    /// Removed instances and when their removal is due, at most
    /// [`MAX_DEVICES`].
    leaving: Vec<(String, Instant)>,
    queried: Option<Instant>,
}

impl fmt::Debug for MdnsBrowser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MdnsBrowser")
            .field("live", &self.daemon.is_some())
            .field("dead", &self.dead)
            .finish()
    }
}

impl MdnsBrowser {
    /// Starts the daemon and browses the network.
    pub fn new() -> Result<MdnsBrowser, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(DiscoveryError)?;
        match daemon.browse(SERVICE_TYPE) {
            Ok(receiver) => Ok(MdnsBrowser {
                source: Source::Daemon {
                    current: receiver,
                    previous: None,
                },
                daemon: Some(daemon),
                dead: false,
                leaving: Vec::new(),
                queried: None,
            }),
            Err(error) => {
                let _ = daemon.shutdown();
                Err(DiscoveryError(error))
            }
        }
    }

    /// A browser that reads `mdns-sd` events from a channel instead of the
    /// network, for tests and for hosts that run their own daemon.
    pub fn from_events(events: mpsc::Receiver<ServiceEvent>) -> MdnsBrowser {
        MdnsBrowser {
            source: Source::Channel(events),
            daemon: None,
            dead: false,
            leaving: Vec::new(),
            queried: None,
        }
    }

    /// The next browse event, or `None` when nothing is waiting.
    pub fn poll_event(&mut self) -> Option<BrowseEvent> {
        self.poll_event_at(Instant::now())
    }

    /// [`MdnsBrowser::poll_event`] at `now`. A removal starts a fresh query
    /// and is reported once [`CONFIRM_WAIT`] passed without the instance
    /// resolving again (C134).
    pub fn poll_event_at(&mut self, now: Instant) -> Option<BrowseEvent> {
        while let Some(event) = self.next_event() {
            match browse_event(event) {
                Some(BrowseEvent::Removed { fullname }) => self.leave(fullname, now),
                Some(BrowseEvent::Resolved(record)) => {
                    self.leaving
                        .retain(|(name, _)| !name.eq_ignore_ascii_case(&record.fullname));
                    return Some(BrowseEvent::Resolved(record));
                }
                None => {}
            }
        }
        let due = self.leaving.iter().position(|&(_, at)| at <= now)?;
        let (fullname, _) = self.leaving.remove(due);
        Some(BrowseEvent::Removed { fullname })
    }

    fn next_event(&mut self) -> Option<ServiceEvent> {
        match &mut self.source {
            Source::Daemon { current, previous } => {
                if let Some(receiver) = previous {
                    match receiver.try_recv() {
                        Ok(event) => return Some(event),
                        Err(mdns_sd::TryRecvError::Empty) => {}
                        Err(mdns_sd::TryRecvError::Disconnected) => *previous = None,
                    }
                }
                match current.try_recv() {
                    Ok(event) => Some(event),
                    Err(mdns_sd::TryRecvError::Empty) => None,
                    Err(mdns_sd::TryRecvError::Disconnected) => {
                        self.dead = true;
                        None
                    }
                }
            }
            Source::Channel(receiver) => match receiver.try_recv() {
                Ok(event) => Some(event),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.dead = true;
                    None
                }
            },
        }
    }

    /// Holds a removal back and asks the network for the instance again.
    /// One already held, or one past [`MAX_DEVICES`] held, is dropped.
    fn leave(&mut self, fullname: String, now: Instant) {
        if self.leaving.len() >= MAX_DEVICES
            || (self.leaving.iter()).any(|(name, _)| name.eq_ignore_ascii_case(&fullname))
        {
            return;
        }
        self.leaving.push((fullname, now + CONFIRM_WAIT));
        if self.queried.is_some_and(|at| now < at + QUERY_GAP) {
            return;
        }
        self.queried = Some(now);
        // Browsing again sends a new query; the daemon hands the browse's
        // events to the new receiver from then on.
        if let (Some(daemon), Source::Daemon { current, previous }) =
            (&self.daemon, &mut self.source)
            && let Ok(receiver) = daemon.browse(SERVICE_TYPE)
        {
            *previous = Some(std::mem::replace(current, receiver));
        }
    }

    /// The event source is gone; nothing will ever arrive again.
    pub fn is_dead(&self) -> bool {
        self.dead
    }
}

impl Drop for MdnsBrowser {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            let _ = daemon.shutdown();
        }
    }
}

/// One `mdns-sd` event as a browse event: a resolved `_googlecast._tcp`
/// instance, or a removed one. Other events give `None`.
pub fn browse_event(event: ServiceEvent) -> Option<BrowseEvent> {
    match event {
        ServiceEvent::ServiceResolved(service) => {
            is_cast(&service.ty_domain).then(|| BrowseEvent::Resolved(record(&service)))
        }
        ServiceEvent::ServiceRemoved(service_type, fullname) => {
            is_cast(&service_type).then_some(BrowseEvent::Removed { fullname })
        }
        _ => None,
    }
}

fn is_cast(service_type: &str) -> bool {
    service_type.eq_ignore_ascii_case(SERVICE_TYPE)
}

fn record(service: &ResolvedService) -> ServiceRecord {
    let addresses = service
        .addresses
        .iter()
        .filter_map(|address| match address {
            ScopedIp::V4(v4) => Some(IpAddr::V4(*v4.addr())),
            ScopedIp::V6(v6) => Some(IpAddr::V6(*v6.addr())),
            _ => None,
        })
        .collect();
    let txt = service
        .txt_properties
        .iter()
        .map(|property| {
            let mut string = property.key().as_bytes().to_vec();
            if let Some(value) = property.val() {
                string.push(b'=');
                string.extend_from_slice(value);
            }
            string
        })
        .collect();
    ServiceRecord {
        fullname: service.fullname.clone(),
        port: service.port,
        txt,
        addresses,
    }
}
