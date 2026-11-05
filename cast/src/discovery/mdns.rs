//! Live browsing of `_googlecast._tcp` through the `mdns-sd` crate (C10).
//!
//! The daemon follows PTR records to SRV, TXT and address records itself
//! and reports an instance once it is resolved; [`browse_event`] turns that
//! report into a [`BrowseEvent`]. The daemon runs on its own thread, and
//! [`MdnsBrowser::poll_event`] only drains its channel, so it never blocks.
//! TXT values come from the daemon as raw bytes, so no DNS presentation
//! escaping reaches the sender (C12). A removal waits for a fresh query
//! to go unanswered before it is reported (C134), and a resolved instance
//! keeps only the addresses C135 admits.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
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
    nets: Nets,
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
                nets: Nets::Live(None),
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
            nets: Nets::Live(None),
        }
    }

    /// Checks addresses against `nets` instead of this computer's
    /// interfaces (C135), for tests.
    pub fn with_local_nets(mut self, nets: Vec<LocalNet>) -> MdnsBrowser {
        self.nets = Nets::Fixed(nets);
        self
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
            match browse_event(event).and_then(|event| keep_on_link(event, &mut self.nets)) {
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

/// The interface addresses of C135: read from the OS at most once per
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

/// A resolved instance keeps only the addresses C135 admits and is dropped
/// when none is left.
fn keep_on_link(event: BrowseEvent, nets: &mut Nets) -> Option<BrowseEvent> {
    let BrowseEvent::Resolved(mut record) = event else {
        return Some(event);
    };
    let nets = nets.current();
    record
        .addresses
        .retain(|&address| on_local_link(address, nets));
    (!record.addresses.is_empty()).then_some(BrowseEvent::Resolved(record))
}

/// Whether discovery may use an address (C135). Never loopback,
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
