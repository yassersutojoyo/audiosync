//! Cast devices on the local network (section 2): the DNS-SD records of
//! `_googlecast._tcp`, their TXT keys, and one list of devices keyed by
//! their `id`.
//!
//! Everything here is plain data in and out, so it is tested without a
//! network; [`mdns`] feeds it from a live browser.

pub mod mdns;
pub mod reach;

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use crate::group::GroupMember;

/// The service the sender browses for (C10).
pub const SERVICE_TYPE: &str = "_googlecast._tcp.local.";
/// The port single devices announce; any other marks a group (C14, C15).
pub const DEVICE_PORT: u16 = 8009;

/// One resolved DNS-SD instance: its full name, the SRV port, the raw TXT
/// strings and the addresses of its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceRecord {
    pub fullname: String,
    pub port: u16,
    pub txt: Vec<Vec<u8>>,
    pub addresses: Vec<IpAddr>,
}

/// What a browser reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowseEvent {
    Resolved(ServiceRecord),
    /// The instance's records expired or a goodbye arrived (C17).
    Removed {
        fullname: String,
    },
}

/// The TXT strings of an instance as `key=value` pairs (C12). Keys compare
/// without case, as RFC 6763 §6.4 asks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Txt {
    entries: Vec<(String, Vec<u8>)>,
}

impl Txt {
    /// Parses raw TXT strings. A string without `=` is skipped, and so is
    /// one whose key isn't text; for a repeated key the first one counts.
    pub fn parse(strings: &[Vec<u8>]) -> Txt {
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for string in strings {
            let Some(split) = string.iter().position(|&byte| byte == b'=') else {
                continue;
            };
            let Ok(key) = std::str::from_utf8(&string[..split]) else {
                continue;
            };
            if key.is_empty() || entries.iter().any(|(k, _)| k.eq_ignore_ascii_case(key)) {
                continue;
            }
            entries.push((key.to_ascii_lowercase(), string[split + 1..].to_vec()));
        }
        Txt { entries }
    }

    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_slice())
    }

    /// A value as text; bytes that aren't UTF-8 become U+FFFD.
    pub fn text(&self, key: &str) -> Option<String> {
        self.get(key)
            .map(|value| String::from_utf8_lossy(value).into_owned())
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }
}

/// A device's `id` TXT value: a UUID (C13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(pub [u8; 16]);

impl DeviceId {
    /// 32 hex digits, with or without the dashes of the 8-4-4-4-12 form.
    pub fn parse(text: &str) -> Option<DeviceId> {
        let dashed = text.len() == 36;
        if dashed {
            let groups: Vec<usize> = text.split('-').map(str::len).collect();
            if groups != [8, 4, 4, 4, 12] {
                return None;
            }
        }
        let digits: Vec<u8> = text.bytes().filter(|&byte| byte != b'-').collect();
        if digits.len() != 32 || (!dashed && text.len() != 32) {
            return None;
        }
        let mut id = [0u8; 16];
        for (slot, pair) in id.iter_mut().zip(digits.as_chunks::<2>().0) {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            *slot = (high * 16 + low) as u8;
        }
        Some(DeviceId(id))
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A single device, or a Cast group, which plays as one target (C3, C14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Single,
    Group,
}

/// One Cast device as the host lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct CastDevice {
    pub id: DeviceId,
    /// The DNS-SD instance it was found under, for removals.
    pub instance: String,
    /// `fn`, the name the user set.
    pub name: Option<String>,
    /// `md`; `None` shows as an unknown model, never as an error.
    pub model: Option<String>,
    /// `ve`, for diagnostics only.
    pub version: Option<String>,
    /// `ic`, a path on the device; not fetched by this version.
    pub icon_path: Option<String>,
    /// `ca`, for the log only: nothing depends on it (C13, C89).
    pub capabilities: Option<String>,
    pub kind: DeviceKind,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
    /// The keys this version does not read (`ca` among them), for the log.
    pub other_keys: Vec<String>,
}

const READ_KEYS: [&str; 5] = ["id", "fn", "md", "ve", "ic"];

/// Why a resolved instance is not a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// Not under `_googlecast._tcp.local.`.
    OtherService,
    /// An instance under a subtype (C11).
    Subtype,
    /// `id` missing or not a UUID (C13).
    BadId,
    /// No address to connect to.
    NoAddress,
}

impl CastDevice {
    /// The device a resolved instance describes (C11 to C14).
    pub fn from_record(record: &ServiceRecord) -> Result<CastDevice, Skipped> {
        let name = record.fullname.to_ascii_lowercase();
        let name = name.strip_suffix('.').unwrap_or(&name);
        let service = SERVICE_TYPE.trim_end_matches('.');
        let Some(instance) = name.strip_suffix(service) else {
            return Err(Skipped::OtherService);
        };
        if instance.is_empty() {
            return Err(Skipped::OtherService);
        }
        if instance.ends_with("._sub.") || instance == "_sub." {
            return Err(Skipped::Subtype);
        }
        let txt = Txt::parse(&record.txt);
        let id = txt
            .text("id")
            .and_then(|id| DeviceId::parse(&id))
            .ok_or(Skipped::BadId)?;
        if record.addresses.is_empty() {
            return Err(Skipped::NoAddress);
        }
        let kind = if record.port == DEVICE_PORT {
            DeviceKind::Single
        } else {
            DeviceKind::Group
        };
        Ok(CastDevice {
            id,
            instance: record.fullname.clone(),
            name: txt.text("fn"),
            model: txt.text("md"),
            version: txt.text("ve"),
            icon_path: txt.text("ic"),
            capabilities: txt.text("ca"),
            kind,
            port: record.port,
            addresses: ordered(&record.addresses),
            other_keys: txt
                .keys()
                .filter(|key| !READ_KEYS.contains(key))
                .map(str::to_owned)
                .collect(),
        })
    }

    /// What a group announces, for the debug log (C89); `None` for a single
    /// device.
    pub fn group_log_line(&self) -> Option<String> {
        (self.kind == DeviceKind::Group).then(|| {
            format!(
                "Cast group {} on port {}: ca={} md={}",
                self.id,
                self.port,
                self.capabilities.as_deref().unwrap_or("-"),
                self.model.as_deref().unwrap_or("-"),
            )
        })
    }

    /// Where to connect, in the order to try (C15, C16): IPv4 first, then
    /// IPv6, each on the announced port.
    pub fn socket_addresses(&self) -> Vec<SocketAddr> {
        self.addresses
            .iter()
            .map(|&ip| SocketAddr::new(ip, self.port))
            .collect()
    }
}

/// IPv4 before IPv6, each in a fixed order, without repeats (C16).
/// Link-local IPv6 addresses are left out: without their interface scope,
/// which this list doesn't carry, no connection to them can be made.
fn ordered(addresses: &[IpAddr]) -> Vec<IpAddr> {
    let mut list: Vec<IpAddr> = addresses
        .iter()
        .copied()
        .filter(|ip| !matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80))
        .collect();
    list.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    list.dedup();
    list
}

/// The most devices the list holds (C108).
pub const MAX_DEVICES: usize = 128;

/// How the host's list names a device (C16, C18).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKey {
    Id(DeviceId),
    /// Added by address while multicast DNS is blocked, until a browse finds
    /// its `id`.
    Manual(SocketAddr),
}

/// One entry of the list.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: DeviceKey,
    /// `None` for a manual entry no browse has found yet.
    pub device: Option<CastDevice>,
    /// Where to connect, the address that last worked first.
    pub addresses: Vec<SocketAddr>,
    pub online: bool,
    pub in_session: bool,
}

impl Entry {
    pub fn is_group(&self) -> bool {
        self.device
            .as_ref()
            .is_some_and(|device| device.kind == DeviceKind::Group)
    }
}

/// A change of the list.
#[derive(Clone, Debug, PartialEq)]
pub enum ListEvent {
    Added(DeviceKey),
    Updated(DeviceKey),
    /// A manual entry turned out to be this device (C18).
    Identified {
        from: SocketAddr,
        id: DeviceId,
    },
    /// Its records went away during a session; it stays listed (C17).
    Offline(DeviceKey),
    Removed(DeviceKey),
    Skipped(Skipped),
}

/// The host's device list: one entry per `id`, however many interfaces and
/// addresses it is seen on (C16).
#[derive(Debug, Default)]
pub struct DeviceList {
    entries: Vec<Entry>,
    /// How many times a session opened or closed on each device added by
    /// address.
    session_changes: Vec<(SocketAddr, u64)>,
    /// The device the user selected, which the cap never drops (C108).
    kept: Option<DeviceKey>,
}

impl DeviceList {
    pub fn new() -> DeviceList {
        DeviceList::default()
    }

    /// Names the device the user selected, `None` when none is.
    pub fn set_kept(&mut self, key: Option<DeviceKey>) {
        self.kept = key;
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, key: DeviceKey) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    /// The single device a group member names (C95): its `deviceId` read
    /// as a UUID equals the device's `id`.
    pub fn member(&self, member: &GroupMember) -> Option<&Entry> {
        let id = member.id?;
        self.get(DeviceKey::Id(id))
            .filter(|entry| entry.device.is_some() && !entry.is_group())
    }

    fn get_mut(&mut self, key: DeviceKey) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.key == key)
    }

    /// Adds a device by address, keyed by that address (C18), offline until
    /// [`DeviceList::set_reachable`] says a check reached it: no record
    /// vouches for it. An address and port a listed device already has
    /// names that device instead: `Updated` with its key, and nothing is
    /// added (C16).
    pub fn add_manual(&mut self, address: SocketAddr) -> ListEvent {
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.addresses.contains(&address))
        {
            return ListEvent::Updated(entry.key);
        }
        let key = DeviceKey::Manual(address);
        self.entries.push(Entry {
            key,
            device: None,
            addresses: vec![address],
            online: false,
            in_session: false,
        });
        ListEvent::Added(key)
    }

    /// What a check of a device added by address found: online when it
    /// reached the device. `Updated` when that changes the entry; nothing
    /// for another key, or while a session holds it, whose own health
    /// decides (C17).
    pub fn set_reachable(&mut self, address: SocketAddr, reached: bool) -> Option<ListEvent> {
        let key = DeviceKey::Manual(address);
        let entry = self.get_mut(key)?;
        if entry.in_session || entry.online == reached {
            return None;
        }
        entry.online = reached;
        Some(ListEvent::Updated(key))
    }

    /// Takes back a device added by address that no browse has found yet;
    /// one a browse found stays, keyed by its `id` (C18).
    pub fn remove_manual(&mut self, address: SocketAddr) -> Option<ListEvent> {
        let key = DeviceKey::Manual(address);
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        self.entries.remove(index);
        Some(ListEvent::Removed(key))
    }

    /// Marks whether a session with the device is open (C17). A device
    /// added by address stays listed when it ends: only the user takes it
    /// back.
    pub fn set_in_session(&mut self, key: DeviceKey, open: bool) {
        let mut gone = false;
        let mut changed = false;
        if let Some(entry) = self.get_mut(key) {
            changed = entry.in_session != open;
            entry.in_session = open;
            gone = !open && !entry.online && matches!(key, DeviceKey::Id(_));
        }
        if changed && let DeviceKey::Manual(address) = key {
            match self
                .session_changes
                .iter_mut()
                .find(|(known, _)| *known == address)
            {
                Some((_, count)) => *count += 1,
                None => self.session_changes.push((address, 1)),
            }
        }
        if gone {
            self.entries.retain(|entry| entry.key != key);
        }
    }

    /// How many times a session opened or closed on the device added by
    /// `address`: a check that started under another count is stale.
    pub fn session_changes(&self, address: SocketAddr) -> u64 {
        self.session_changes
            .iter()
            .find(|(known, _)| *known == address)
            .map_or(0, |&(_, count)| count)
    }

    /// Moves the address that worked to the front (C16). A group keeps the
    /// order of its records, which follow its leader (C90).
    pub fn remember_address(&mut self, key: DeviceKey, address: SocketAddr) {
        if let Some(entry) = self.get_mut(key) {
            if entry.is_group() {
                return;
            }
            entry.addresses.retain(|&known| known != address);
            entry.addresses.insert(0, address);
        }
    }

    pub fn apply(&mut self, event: BrowseEvent) -> ListEvent {
        match event {
            BrowseEvent::Resolved(record) => match CastDevice::from_record(&record) {
                Ok(device) => self.resolved(device),
                Err(skipped) => ListEvent::Skipped(skipped),
            },
            BrowseEvent::Removed { fullname } => self.removed(&fullname),
        }
    }

    fn resolved(&mut self, device: CastDevice) -> ListEvent {
        let id = device.id;
        let key = DeviceKey::Id(id);
        let fresh = device.socket_addresses();
        if let Some(entry) = self.get_mut(key) {
            if device.kind == DeviceKind::Group {
                entry.addresses = fresh;
                entry.device = Some(device);
                entry.online = true;
                return ListEvent::Updated(key);
            }
            let mut addresses = entry.addresses.clone();
            // The address that worked stays first; the rest follow the
            // records, which are the current truth.
            let remembered = addresses.first().copied().filter(|a| fresh.contains(a));
            addresses = fresh;
            if let Some(first) = remembered {
                addresses.retain(|&a| a != first);
                addresses.insert(0, first);
            }
            entry.addresses = addresses;
            entry.device = Some(device);
            entry.online = true;
            return ListEvent::Updated(key);
        }
        let manual = self.entries.iter().position(|entry| match entry.key {
            DeviceKey::Manual(address) => fresh.contains(&address),
            DeviceKey::Id(_) => false,
        });
        if let Some(index) = manual {
            let entry = &mut self.entries[index];
            let DeviceKey::Manual(from) = entry.key else {
                unreachable!("position matched a manual key");
            };
            entry.key = key;
            entry.addresses = fresh;
            entry.addresses.retain(|&a| a != from);
            entry.addresses.insert(0, from);
            entry.device = Some(device);
            entry.online = true;
            return ListEvent::Identified { from, id };
        }
        self.entries.push(Entry {
            key,
            device: Some(device),
            addresses: fresh,
            online: true,
            in_session: false,
        });
        ListEvent::Added(key)
    }

    fn removed(&mut self, fullname: &str) -> ListEvent {
        let Some(index) = self.entries.iter().position(|entry| {
            entry
                .device
                .as_ref()
                .is_some_and(|device| device.instance.eq_ignore_ascii_case(fullname))
        }) else {
            return ListEvent::Skipped(Skipped::OtherService);
        };
        let key = self.entries[index].key;
        if self.entries[index].in_session {
            self.entries[index].online = false;
            ListEvent::Offline(key)
        } else {
            self.entries.remove(index);
            ListEvent::Removed(key)
        }
    }
}
