//! Live browsing of `_googlecast._tcp` through the `mdns-sd` crate (C10).
//!
//! The daemon follows PTR records to SRV, TXT and address records itself
//! and reports an instance once it is resolved; [`browse_event`] turns that
//! report into a [`BrowseEvent`]. The daemon runs on its own thread, and
//! [`MdnsBrowser::poll_event`] only drains its channel, so it never blocks.
//! TXT values come from the daemon as raw bytes, so no DNS presentation
//! escaping reaches the sender (C12).

use std::fmt;
use std::net::IpAddr;
use std::sync::mpsc;

use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};

use super::{BrowseEvent, SERVICE_TYPE, ServiceRecord};

/// The live browser could not start.
#[derive(Debug)]
pub struct DiscoveryError(mdns_sd::Error);

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mDNS browsing could not start: {}", self.0)
    }
}

impl std::error::Error for DiscoveryError {}

enum Source {
    Daemon(mdns_sd::Receiver<ServiceEvent>),
    Channel(mpsc::Receiver<ServiceEvent>),
}

/// Browses `_googlecast._tcp.local.` and hands over what it finds.
pub struct MdnsBrowser {
    source: Source,
    daemon: Option<ServiceDaemon>,
    dead: bool,
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
                source: Source::Daemon(receiver),
                daemon: Some(daemon),
                dead: false,
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
        }
    }

    /// The next browse event, or `None` when nothing is waiting.
    pub fn poll_event(&mut self) -> Option<BrowseEvent> {
        loop {
            let event = match &self.source {
                Source::Daemon(receiver) => match receiver.try_recv() {
                    Ok(event) => event,
                    Err(mdns_sd::TryRecvError::Empty) => return None,
                    Err(mdns_sd::TryRecvError::Disconnected) => {
                        self.dead = true;
                        return None;
                    }
                },
                Source::Channel(receiver) => match receiver.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => return None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.dead = true;
                        return None;
                    }
                },
            };
            if let Some(event) = browse_event(event) {
                return Some(event);
            }
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
