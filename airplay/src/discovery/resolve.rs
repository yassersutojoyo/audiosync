//! Resolving services from individual mDNS records (P7, P18).
//!
//! A browser that hands over raw records (PTR, SRV, TXT, A and AAAA) feeds
//! them to a [`Resolver`], which follows PTR to SRV, TXT and the addresses of
//! the SRV target and reports a service once all of them are known. A browser
//! that resolves services itself skips this and sends [`BrowseEvent`]s to the
//! [`Directory`](super::Directory) directly.
//!
//! Address records are keyed by host name, not by interface, so a device
//! answering on several interfaces or address families resolves to one
//! service with all its addresses (P10). A record with TTL 0 (a goodbye) is
//! dropped at once and an expired record when [`Resolver::expire`] runs;
//! either makes the service unavailable for new sessions, and neither
//! touches a running session (P18).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::{BrowseEvent, ScopedAddr, ServiceKind, ServiceRecord};

/// One mDNS record of interest, with names already split by the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsRecord {
    /// `<service>.local.` PTR `<instance>.<service>.local.`
    Ptr {
        kind: ServiceKind,
        instance: String,
        ttl: u32,
    },
    /// The instance's SRV record: target host and port.
    Srv {
        kind: ServiceKind,
        instance: String,
        target: String,
        port: u16,
        ttl: u32,
    },
    /// The instance's TXT strings.
    Txt {
        kind: ServiceKind,
        instance: String,
        strings: Vec<Vec<u8>>,
        ttl: u32,
    },
    /// An A or AAAA record of a host.
    Address {
        host: String,
        address: ScopedAddr,
        ttl: u32,
    },
}

type ServiceId = (ServiceKind, String);

#[derive(Debug, Default)]
struct Service {
    ptr: Option<Instant>,
    srv: Option<(String, u16, Instant)>,
    txt: Option<(Vec<Vec<u8>>, Instant)>,
    reported: Option<ServiceRecord>,
}

/// Assembles resolved services from mDNS records.
#[derive(Debug, Default)]
pub struct Resolver {
    services: BTreeMap<ServiceId, Service>,
    /// Lowercased host name without a trailing dot, to its addresses and their
    /// expiry times.
    hosts: BTreeMap<String, Vec<(ScopedAddr, Instant)>>,
}

impl Resolver {
    pub fn new() -> Resolver {
        Resolver::default()
    }

    /// Takes one record received at `now` and reports every service it
    /// resolved, changed or made unavailable.
    pub fn record(&mut self, record: DnsRecord, now: Instant) -> Vec<BrowseEvent> {
        let expiry = |ttl: u32| (ttl > 0).then(|| now + Duration::from_secs(u64::from(ttl)));
        match record {
            DnsRecord::Ptr {
                kind,
                instance,
                ttl,
            } => {
                let id = (kind, instance);
                self.services.entry(id.clone()).or_default().ptr = expiry(ttl);
                self.evaluate(&[id])
            }
            DnsRecord::Srv {
                kind,
                instance,
                target,
                port,
                ttl,
            } => {
                let id = (kind, instance);
                self.services.entry(id.clone()).or_default().srv =
                    expiry(ttl).map(|until| (host_key(&target), port, until));
                self.evaluate(&[id])
            }
            DnsRecord::Txt {
                kind,
                instance,
                strings,
                ttl,
            } => {
                let id = (kind, instance);
                self.services.entry(id.clone()).or_default().txt =
                    expiry(ttl).map(|until| (strings, until));
                self.evaluate(&[id])
            }
            DnsRecord::Address { host, address, ttl } => {
                let host = host_key(&host);
                let addresses = self.hosts.entry(host.clone()).or_default();
                addresses.retain(|(known, _)| *known != address);
                if let Some(until) = expiry(ttl) {
                    addresses.push((address, until));
                }
                if addresses.is_empty() {
                    self.hosts.remove(&host);
                }
                let ids: Vec<ServiceId> = self
                    .services
                    .iter()
                    .filter(|(_, service)| {
                        service
                            .srv
                            .as_ref()
                            .is_some_and(|(target, ..)| *target == host)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                self.evaluate(&ids)
            }
        }
    }

    /// Drops records whose TTL has run out by `now` and reports the services
    /// that became unavailable or changed.
    pub fn expire(&mut self, now: Instant) -> Vec<BrowseEvent> {
        for service in self.services.values_mut() {
            if service.ptr.is_some_and(|until| until <= now) {
                service.ptr = None;
            }
            if service
                .srv
                .as_ref()
                .is_some_and(|(_, _, until)| *until <= now)
            {
                service.srv = None;
            }
            if service.txt.as_ref().is_some_and(|(_, until)| *until <= now) {
                service.txt = None;
            }
        }
        for addresses in self.hosts.values_mut() {
            addresses.retain(|(_, until)| *until > now);
        }
        self.hosts.retain(|_, addresses| !addresses.is_empty());
        let ids: Vec<ServiceId> = self.services.keys().cloned().collect();
        self.evaluate(&ids)
    }

    /// The earliest time a record expires, for the driver's timer.
    pub fn next_expiry(&self) -> Option<Instant> {
        let services = self.services.values().flat_map(|service| {
            [
                service.ptr,
                service.srv.as_ref().map(|(_, _, until)| *until),
                service.txt.as_ref().map(|(_, until)| *until),
            ]
        });
        let hosts = self
            .hosts
            .values()
            .flat_map(|addresses| addresses.iter().map(|(_, until)| Some(*until)));
        services.chain(hosts).flatten().min()
    }

    fn evaluate(&mut self, ids: &[ServiceId]) -> Vec<BrowseEvent> {
        let mut events = Vec::new();
        for id in ids {
            let current = match self.services.get(id) {
                Some(service) => self.assemble(id, service),
                None => continue,
            };
            let Some(service) = self.services.get_mut(id) else {
                continue;
            };
            if current != service.reported {
                events.push(match &current {
                    Some(record) => BrowseEvent::Resolved(record.clone()),
                    None => BrowseEvent::Removed {
                        kind: id.0,
                        instance: id.1.clone(),
                    },
                });
                service.reported = current;
            }
            if service.ptr.is_none() && service.srv.is_none() && service.txt.is_none() {
                self.services.remove(id);
            }
        }
        events
    }

    fn assemble(&self, id: &ServiceId, service: &Service) -> Option<ServiceRecord> {
        service.ptr?;
        let (target, port, _) = service.srv.as_ref()?;
        let (strings, _) = service.txt.as_ref()?;
        let addresses: Vec<ScopedAddr> = self
            .hosts
            .get(target)?
            .iter()
            .map(|(address, _)| *address)
            .collect();
        Some(ServiceRecord {
            kind: id.0,
            instance: id.1.clone(),
            host: target.clone(),
            port: *port,
            txt: strings.clone(),
            addresses,
        })
    }
}

/// Host names compare case-insensitively and without the root dot.
fn host_key(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}
