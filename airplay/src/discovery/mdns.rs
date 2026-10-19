//! Live mDNS browsing through the `mdns-sd` crate (P7).
//!
//! [`MdnsBrowser`] browses `_raop._tcp` and `_airplay._tcp` in `local.` and
//! implements [`Browser`]. The `mdns-sd` daemon follows PTR records to SRV,
//! TXT and address records itself and reports a service once it is
//! resolved, so its events become [`BrowseEvent`]s directly, without the
//! raw-record [`Resolver`](super::Resolver). The daemon runs on its own
//! thread; [`Browser::poll_event`] only drains its channels and never
//! blocks. A channel whose sending side is gone (the daemon's thread ended,
//! which only a bug in it does) makes the browser dead
//! ([`Browser::is_dead`]): it would never report anything again.
//!
//! The daemon binds UDP port 5353 with address (and, on Unix, port) reuse,
//! so it shares the port with the system's own mDNS responder (Bonjour,
//! Avahi or the Windows DNS client). It only queries; it announces nothing.

use std::fmt;
use std::sync::mpsc;

use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};

use super::{BrowseEvent, Browser, ScopedAddr, ServiceKind, ServiceRecord};

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

/// What one look at a source found.
enum Next {
    Event(ServiceEvent),
    Empty,
    /// The sending side is gone and everything it sent was read.
    Gone,
}

impl Source {
    fn try_next(&self) -> Next {
        match self {
            Source::Daemon(receiver) => match receiver.try_recv() {
                Ok(event) => Next::Event(event),
                Err(mdns_sd::TryRecvError::Empty) => Next::Empty,
                Err(mdns_sd::TryRecvError::Disconnected) => Next::Gone,
            },
            Source::Channel(receiver) => match receiver.try_recv() {
                Ok(event) => Next::Event(event),
                Err(mpsc::TryRecvError::Empty) => Next::Empty,
                Err(mpsc::TryRecvError::Disconnected) => Next::Gone,
            },
        }
    }
}

/// Browses both service types (P7) and hands over what it finds.
pub struct MdnsBrowser {
    sources: Vec<Source>,
    daemon: Option<ServiceDaemon>,
    /// A source's sending side is gone.
    dead: bool,
}

impl fmt::Debug for MdnsBrowser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MdnsBrowser")
            .field("sources", &self.sources.len())
            .field("live", &self.daemon.is_some())
            .field("dead", &self.dead)
            .finish()
    }
}

impl MdnsBrowser {
    /// Starts the `mdns-sd` daemon and browses `_raop._tcp.local.` and
    /// `_airplay._tcp.local.`.
    pub fn new() -> Result<MdnsBrowser, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(DiscoveryError)?;
        let mut sources = Vec::new();
        for kind in [ServiceKind::Raop, ServiceKind::AirPlay] {
            let service_type = format!("{}.{}", kind.service_type(), ServiceKind::DOMAIN);
            match daemon.browse(&service_type) {
                Ok(receiver) => sources.push(Source::Daemon(receiver)),
                Err(error) => {
                    let _ = daemon.shutdown();
                    return Err(DiscoveryError(error));
                }
            }
        }
        Ok(MdnsBrowser {
            sources,
            daemon: Some(daemon),
            dead: false,
        })
    }

    /// A browser that reads `mdns-sd` events from a channel instead of the
    /// network, for tests and for hosts that run their own daemon. It is
    /// dead once the channel's sender is gone and everything sent was read.
    pub fn from_events(events: mpsc::Receiver<ServiceEvent>) -> MdnsBrowser {
        MdnsBrowser {
            sources: vec![Source::Channel(events)],
            daemon: None,
            dead: false,
        }
    }
}

impl Browser for MdnsBrowser {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        for source in &self.sources {
            loop {
                match source.try_next() {
                    Next::Event(event) => {
                        if let Some(event) = browse_event(event) {
                            return Some(event);
                        }
                    }
                    Next::Empty => break,
                    Next::Gone => {
                        self.dead = true;
                        break;
                    }
                }
            }
        }
        None
    }

    fn is_dead(&self) -> bool {
        self.dead
    }
}

impl Drop for MdnsBrowser {
    fn drop(&mut self) {
        // The daemon blocks while a full channel waits to be read: drop the
        // receivers first, then ask it to stop.
        self.sources.clear();
        if let Some(daemon) = self.daemon.take() {
            let _ = daemon.shutdown();
        }
    }
}

/// Turns one `mdns-sd` event into a browse event: a resolved service of
/// either type with at least one address, or a removed one (P7, P18).
/// Search notices, other service types and services without an address
/// give `None`.
pub fn browse_event(event: ServiceEvent) -> Option<BrowseEvent> {
    match event {
        ServiceEvent::ServiceResolved(service) => resolved(&service).map(BrowseEvent::Resolved),
        ServiceEvent::ServiceRemoved(service_type, fullname) => {
            let kind = ServiceKind::from_service_type(&service_type)?;
            let instance = instance_name(&fullname, kind)?;
            Some(BrowseEvent::Removed { kind, instance })
        }
        _ => None,
    }
}

fn resolved(service: &ResolvedService) -> Option<ServiceRecord> {
    let kind = ServiceKind::from_service_type(&service.ty_domain)?;
    let instance = instance_name(&service.fullname, kind)?;
    let mut addresses: Vec<ScopedAddr> = service.addresses.iter().filter_map(scoped).collect();
    if addresses.is_empty() {
        return None;
    }
    // The daemon keeps addresses in a set; a fixed order keeps an unchanged
    // service from looking changed (P19).
    addresses.sort();
    addresses.dedup();
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
    Some(ServiceRecord {
        kind,
        instance,
        host: service.host.clone(),
        port: service.port,
        txt,
        addresses,
    })
}

/// An address with the interface scope an IPv6 address carries (P17).
fn scoped(address: &ScopedIp) -> Option<ScopedAddr> {
    match address {
        ScopedIp::V4(v4) => Some(ScopedAddr::new((*v4.addr()).into())),
        ScopedIp::V6(v6) => {
            let ip = *v6.addr();
            let link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
            let scope = if link_local { v6.scope_id().index } else { 0 };
            Some(ScopedAddr::with_scope(ip.into(), scope))
        }
        _ => None,
    }
}

/// The instance part of `<instance>.<service>.local.`, with the escapes of
/// RFC 6763 §4.3 undone (`\.` for a dot inside the instance, `\\` for a
/// backslash, `\DDD` for a byte in decimal).
pub fn instance_name(fullname: &str, kind: ServiceKind) -> Option<String> {
    let name = fullname.strip_suffix('.').unwrap_or(fullname);
    let suffix = format!(".{}.local", kind.service_type());
    let split = name.len().checked_sub(suffix.len())?;
    if !name.is_char_boundary(split) || !name[split..].eq_ignore_ascii_case(&suffix) {
        return None;
    }
    let escaped = &name[..split];
    if escaped.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(escaped.len());
    let raw = escaped.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        let byte = raw[index];
        if byte != b'\\' || index + 1 == raw.len() {
            bytes.push(byte);
            index += 1;
            continue;
        }
        let digits = raw.get(index + 1..index + 4);
        let decimal = digits
            .filter(|digits| digits.iter().all(u8::is_ascii_digit))
            .and_then(|digits| std::str::from_utf8(digits).ok()?.parse::<u8>().ok());
        match decimal {
            Some(value) => {
                bytes.push(value);
                index += 4;
            }
            None => {
                bytes.push(raw[index + 1]);
                index += 2;
            }
        }
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}
