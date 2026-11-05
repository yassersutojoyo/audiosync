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
//! Avahi or the Windows DNS client). The browser only queries;
//! [`MdnsAdvertiser`] announces the one record the sender has, its
//! remote-control service (P188), on a daemon of its own.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mdns_sd::{
    DaemonEvent, RRType, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo,
};

use super::{BrowseEvent, Browser, ScopedAddr, ServiceKind, ServiceRecord};
use crate::dacp::{AdvertiseError, DacpAdvertiser, DacpRecord, NameConflict, SERVICE_TYPE};

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

/// How many times [`MdnsBrowser::new`] tried to start a live daemon in this
/// process.
static DAEMONS_STARTED: AtomicUsize = AtomicUsize::new(0);

/// How many times this process tried to start a live `mdns-sd` daemon
/// ([`MdnsBrowser::new`]), whether it started or not. Tests of the crates
/// built on the sender check it stays 0: a test never browses the real
/// network, whose multicast sockets on every interface make the system
/// firewall ask the user to allow the test program.
pub fn daemons_started() -> usize {
    DAEMONS_STARTED.load(Ordering::SeqCst)
}

/// Browses both service types (P7) and hands over what it finds.
pub struct MdnsBrowser {
    sources: Vec<Source>,
    daemon: Option<ServiceDaemon>,
    /// A source's sending side is gone.
    dead: bool,
    nets: Nets,
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
        DAEMONS_STARTED.fetch_add(1, Ordering::SeqCst);
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
            nets: Nets::Live(None),
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
            nets: Nets::Live(None),
        }
    }

    /// Checks addresses against `nets` instead of this computer's
    /// interfaces (P234), for tests.
    pub fn with_local_nets(mut self, nets: Vec<LocalNet>) -> MdnsBrowser {
        self.nets = Nets::Fixed(nets);
        self
    }
}

impl Browser for MdnsBrowser {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        for source in &self.sources {
            loop {
                match source.try_next() {
                    Next::Event(event) => {
                        let event = browse_event(event)
                            .and_then(|event| keep_on_link(event, &mut self.nets));
                        if let Some(event) = event {
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

/// One address of this computer's interfaces, with its on-link prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNet {
    pub address: IpAddr,
    pub prefix: u8,
}

/// This computer's interface addresses now; none when the OS won't list them.
pub fn local_nets() -> Vec<LocalNet> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    interfaces
        .iter()
        .map(|interface| LocalNet {
            address: interface.ip(),
            prefix: match &interface.addr {
                if_addrs::IfAddr::V4(v4) => v4.prefixlen,
                if_addrs::IfAddr::V6(v6) => v6.prefixlen,
            },
        })
        .collect()
}

/// How long the browser reuses its list of this computer's interfaces.
const NETS_REFRESH: Duration = Duration::from_secs(1);

/// The interface addresses of P234: read from the OS at most once per
/// [`NETS_REFRESH`], since listing them is slow on Windows and interfaces
/// come and go, or fixed by a test.
enum Nets {
    Live(Option<(Instant, Vec<LocalNet>)>),
    Fixed(Vec<LocalNet>),
}

impl Nets {
    fn current(&mut self) -> &[LocalNet] {
        match self {
            Nets::Fixed(nets) => nets,
            Nets::Live(cache) => {
                if cache
                    .as_ref()
                    .is_some_and(|(read, _)| read.elapsed() >= NETS_REFRESH)
                {
                    *cache = None;
                }
                &cache
                    .get_or_insert_with(|| (Instant::now(), local_nets()))
                    .1
            }
        }
    }
}

/// A resolved service keeps only the addresses P234 admits and is dropped
/// when none is left.
fn keep_on_link(event: BrowseEvent, nets: &mut Nets) -> Option<BrowseEvent> {
    let BrowseEvent::Resolved(mut record) = event else {
        return Some(event);
    };
    let nets = nets.current();
    record
        .addresses
        .retain(|address| on_local_link(address.ip, nets));
    (!record.addresses.is_empty()).then_some(BrowseEvent::Resolved(record))
}

/// Whether discovery may use an address (P234). Never loopback,
/// unspecified, multicast, broadcast or one of `nets` itself; a link-local
/// address only when `nets` holds one of its kind; otherwise an address
/// inside the subnet of one of `nets`, or a private or unique local one
/// that an mDNS reflector passed on from a routed network.
pub fn on_local_link(address: IpAddr, nets: &[LocalNet]) -> bool {
    let address = address.to_canonical();
    if address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address == IpAddr::V4(Ipv4Addr::BROADCAST)
        || nets.iter().any(|net| net.address.to_canonical() == address)
    {
        return false;
    }
    if link_local(address) {
        return nets.iter().any(|net| {
            link_local(net.address.to_canonical()) && net.address.is_ipv4() == address.is_ipv4()
        });
    }
    let in_ours = |broadcast| {
        nets.iter().any(|net| {
            in_subnet(address, net.address.to_canonical(), net.prefix) == Some(broadcast)
        })
    };
    if in_ours(true) {
        return false;
    }
    in_ours(false)
        || match address {
            IpAddr::V4(v4) => v4.is_private(),
            IpAddr::V6(v6) => v6.segments()[0] & 0xfe00 == 0xfc00,
        }
}

fn link_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    }
}

/// Whether `address` is inside the subnet of `own`, and if so whether it is
/// that IPv4 subnet's broadcast address. Teredo (2001::/32) and 6to4
/// (2002::/16) prefixes are built from public IPv4 addresses, so they are
/// no link; a prefix of 0 is an address the OS listed without a netmask,
/// not a link to the whole internet.
fn in_subnet(address: IpAddr, own: IpAddr, prefix: u8) -> Option<bool> {
    let (address, own, width) = match (address, own) {
        (IpAddr::V4(address), IpAddr::V4(own)) => {
            (u128::from(address.to_bits()), u128::from(own.to_bits()), 32)
        }
        (IpAddr::V6(address), IpAddr::V6(own))
            if own.segments()[..2] != [0x2001, 0] && own.segments()[0] != 0x2002 =>
        {
            (address.to_bits(), own.to_bits(), 128)
        }
        _ => return None,
    };
    if prefix == 0 || u32::from(prefix) > width {
        return None;
    }
    let host_bits = width - u32::from(prefix);
    let host_mask = (1u128 << host_bits) - 1;
    ((address ^ own) & !host_mask == 0)
        .then_some(width == 32 && host_bits >= 2 && address & host_mask == host_mask)
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

/// Registers the remote-control service `_dacp._tcp` on the network
/// (P188), on an `mdns-sd` daemon it starts at the first registration. The
/// record carries the addresses of every interface, which the daemon keeps
/// current; a receiver takes only its port and sends to the address its
/// RTSP connection came from.
#[derive(Default)]
pub struct MdnsAdvertiser {
    daemon: Option<ServiceDaemon>,
    /// The daemon's own events, read for its renames after a conflict.
    monitor: Option<mdns_sd::Receiver<DaemonEvent>>,
    /// The registered record's full name.
    registered: Option<String>,
}

/// Monitor events read per call. The daemon's monitor channel already holds
/// at most 100 and drops the rest; the cap is defensive.
const MONITOR_READS: usize = 100;

impl fmt::Debug for MdnsAdvertiser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MdnsAdvertiser")
            .field("live", &self.daemon.is_some())
            .field("registered", &self.registered.is_some())
            .finish()
    }
}

impl MdnsAdvertiser {
    pub fn new() -> MdnsAdvertiser {
        MdnsAdvertiser::default()
    }
}

impl DacpAdvertiser for MdnsAdvertiser {
    fn register(&mut self, record: &DacpRecord) -> Result<(), AdvertiseError> {
        let failed = |error: mdns_sd::Error| AdvertiseError(error.to_string());
        if self.daemon.is_none() {
            DAEMONS_STARTED.fetch_add(1, Ordering::SeqCst);
            let daemon = ServiceDaemon::new().map_err(failed)?;
            self.monitor = daemon.monitor().ok();
            self.daemon = Some(daemon);
        }
        let Some(daemon) = self.daemon.as_ref() else {
            return Err(AdvertiseError("no daemon".to_owned()));
        };
        // A host name of its own, made from the random DACP ID only (P5).
        let id = record
            .instance
            .rsplit('_')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let host = format!("audiosync-{id}.local.");
        let txt: Vec<(&str, &str)> = record
            .txt
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &record.instance,
            &host,
            (),
            record.port,
            &txt[..],
        )
        .map_err(failed)?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        daemon.register(info).map_err(failed)?;
        self.registered = Some(fullname);
        Ok(())
    }

    fn withdraw(&mut self) {
        if let (Some(daemon), Some(name)) = (self.daemon.as_ref(), self.registered.take()) {
            let _ = daemon.unregister(&name);
        }
    }

    fn conflicts(&mut self) -> Vec<NameConflict> {
        match self.monitor.as_ref() {
            Some(monitor) => name_conflicts(monitor.try_iter()),
            None => Vec::new(),
        }
    }
}

/// The renames among the first [`MONITOR_READS`] daemon events, once per
/// name and interface: the daemon reports each renamed record (SRV and
/// TXT, or A and AAAA), and an address record's rename is the host name's.
fn name_conflicts(events: impl Iterator<Item = DaemonEvent>) -> Vec<NameConflict> {
    let mut conflicts: Vec<NameConflict> = Vec::new();
    for event in events.take(MONITOR_READS) {
        let DaemonEvent::NameChange(change) = event else {
            continue;
        };
        let conflict = NameConflict {
            host: matches!(change.rr_type, RRType::A | RRType::AAAA),
            interface: change.intf_name,
        };
        if !conflicts.contains(&conflict) {
            conflicts.push(conflict);
        }
    }
    conflicts
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        self.withdraw();
        if let Some(daemon) = self.daemon.take() {
            let _ = daemon.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use mdns_sd::DnsNameChange;

    use super::*;

    fn rename(rr_type: RRType, interface: &str) -> DaemonEvent {
        DaemonEvent::NameChange(DnsNameChange {
            original: "iTunes_Ctrl_0123456789ABCDEF._dacp._tcp.local.".to_owned(),
            new_name: "iTunes_Ctrl_0123456789ABCDEF (2)._dacp._tcp.local.".to_owned(),
            rr_type,
            intf_name: interface.to_owned(),
        })
    }

    fn conflict(host: bool, interface: &str) -> NameConflict {
        NameConflict {
            host,
            interface: interface.to_owned(),
        }
    }

    #[test]
    fn address_renames_are_the_host_name_and_other_events_are_skipped() {
        let events = [
            DaemonEvent::Respond("eth0".to_owned()),
            rename(RRType::SRV, "eth0"),
            rename(RRType::A, "wlan0"),
            rename(RRType::SRV, "wlan0"),
        ];
        assert_eq!(
            name_conflicts(events.into_iter()),
            [
                conflict(false, "eth0"),
                conflict(true, "wlan0"),
                conflict(false, "wlan0"),
            ]
        );
    }

    #[test]
    fn a_conflict_renaming_several_records_is_one_conflict() {
        let events = [
            rename(RRType::SRV, "eth0"),
            rename(RRType::TXT, "eth0"),
            rename(RRType::A, "eth0"),
            rename(RRType::AAAA, "eth0"),
        ];
        assert_eq!(
            name_conflicts(events.into_iter()),
            [conflict(false, "eth0"), conflict(true, "eth0")]
        );
    }

    #[test]
    fn at_most_the_cap_is_read_per_call() {
        let mut events = (0..MONITOR_READS + 5).map(|n| rename(RRType::SRV, &format!("if{n}")));
        assert_eq!(name_conflicts(events.by_ref()).len(), MONITOR_READS);
        assert_eq!(events.count(), 5);
    }
}
