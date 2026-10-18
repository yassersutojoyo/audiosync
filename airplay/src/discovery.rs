//! Discovery: the DNS-SD records of `_raop._tcp` and `_airplay._tcp` and one
//! merged description per device (P7 to P19), with the decisions that rest
//! on it: protocol path (P2), security modes (P45, P46), codec (P74) and
//! connect order (P17).
//!
//! No mDNS traffic happens here. A browser (a later stage, behind
//! [`Browser`]) produces [`BrowseEvent`]s, either itself or by feeding raw
//! records to a [`Resolver`]; the [`Directory`] merges them into [`Device`]s
//! and reports every change.

pub mod resolve;
pub mod txt;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;

pub use resolve::{DnsRecord, Resolver};
pub use txt::{AirPlayTxt, RaopTxt, Txt};

use crate::error::{ErrorKind, UnsupportedReason};
use crate::metadata::MetadataSupport;
use crate::sdp::Codec;

/// The two service types the sender browses for in `local.` (P7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServiceKind {
    /// `_raop._tcp`: AirPlay 1 audio.
    Raop,
    /// `_airplay._tcp`.
    AirPlay,
}

impl ServiceKind {
    /// The browse domain (P7).
    pub const DOMAIN: &'static str = "local.";

    pub fn service_type(self) -> &'static str {
        match self {
            ServiceKind::Raop => "_raop._tcp",
            ServiceKind::AirPlay => "_airplay._tcp",
        }
    }

    /// Reads `_raop._tcp`, `_raop._tcp.local` or `_raop._tcp.local.` (and the
    /// same for `_airplay._tcp`), in any case.
    pub fn from_service_type(name: &str) -> Option<ServiceKind> {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let name = name.strip_suffix(".local").unwrap_or(&name);
        match name {
            "_raop._tcp" => Some(ServiceKind::Raop),
            "_airplay._tcp" => Some(ServiceKind::AirPlay),
            _ => None,
        }
    }
}

/// A device identifier: six bytes, written as 12 uppercase hex digits (P10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub [u8; 6]);

impl DeviceId {
    /// 12 hex digits without separators, as before the `@` of a RAOP name
    /// (P8).
    pub fn from_hex12(text: &str) -> Option<DeviceId> {
        if text.len() != 12 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0u8; 6];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(DeviceId(bytes))
    }

    /// Six colon-separated hex bytes, as in AirPlay `deviceid` (P9).
    pub fn from_colon_form(text: &str) -> Option<DeviceId> {
        let parts: Vec<&str> = text.trim().split(':').collect();
        if parts.len() != 6 || parts.iter().any(|part| part.len() != 2) {
            return None;
        }
        DeviceId::from_hex12(&parts.concat())
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// Splits a RAOP instance name `<id>@<name>` at its first `@` (P8). `None`
/// unless the part before it is 12 hex digits.
pub fn parse_raop_instance(instance: &str) -> Option<(DeviceId, &str)> {
    let (id, name) = instance.split_once('@')?;
    Some((DeviceId::from_hex12(id)?, name))
}

/// The features bitmask of AirPlay `features` and RAOP `ft` (P14).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Features(pub u64);

impl Features {
    pub const AUDIO: u32 = 9;
    pub const AUDIO_REDUNDANCY: u32 = 11;
    pub const FAIRPLAY: u32 = 14;
    pub const METADATA_ARTWORK: u32 = 15;
    pub const METADATA_PROGRESS: u32 = 16;
    pub const METADATA_TEXT: u32 = 17;
    /// One of the audio-format bits 18 to 21; [oas-feat] says 19 and 20 must
    /// be set for an AirPlay 2 connection.
    pub const AUDIO_FORMAT_19: u32 = 19;
    pub const AUDIO_FORMAT_20: u32 = 20;
    pub const RSA_AUTH: u32 = 23;
    /// MFi authentication in one source, "has unified advertiser info" in the
    /// other (P14, G12): only the harmless `/auth-setup` rests on it (P60).
    pub const BIT_26: u32 = 26;
    pub const LEGACY_PAIRING: u32 = 27;
    /// Read differently by the two sources (P14, G12); no decision uses it.
    pub const BIT_30: u32 = 30;
    pub const COREUTILS_PAIRING: u32 = 38;
    pub const BUFFERED_AUDIO: u32 = 40;
    pub const PTP: u32 = 41;
    pub const SYSTEM_PAIRING: u32 = 43;
    pub const HOMEKIT_PAIRING: u32 = 46;
    pub const TRANSIENT_PAIRING: u32 = 48;
    pub const BINARY_PLIST_METADATA: u32 = 50;
    pub const UNIFIED_PAIR_SETUP_MFI: u32 = 51;
    pub const EXTENDED_SETPEERS: u32 = 52;

    /// One or two hex numbers separated by a comma: bits 0 to 31, then
    /// optionally bits 32 to 63. Each may carry `0x`. A third part or a part
    /// wider than 32 bits makes the value malformed.
    pub fn parse_txt(value: &str) -> Option<Features> {
        let mut parts = value.split(',');
        let low = txt::parse_hex_number(parts.next()?)?;
        let high = match parts.next() {
            Some(part) => txt::parse_hex_number(part)?,
            None => 0,
        };
        if parts.next().is_some() || low > u64::from(u32::MAX) || high > u64::from(u32::MAX) {
            return None;
        }
        Some(Features(high << 32 | low))
    }

    /// `/info` reports the mask as one decimal integer (P14).
    pub fn from_info(value: u64) -> Features {
        Features(value)
    }

    pub fn has(self, bit: u32) -> bool {
        bit < 64 && self.0 >> bit & 1 == 1
    }
}

/// Status flags of AirPlay `flags` and RAOP `sf` (P15).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct StatusFlags(pub u32);

impl StatusFlags {
    pub const AUDIO_CABLE_ATTACHED: u32 = 2;
    pub const PIN_REQUIRED: u32 = 3;
    pub const PASSWORD_REQUIRED: u32 = 7;
    pub const ONE_TIME_PAIRING_REQUIRED: u32 = 9;
    pub const SESSION_ACTIVE: u32 = 17;

    /// A hex number, with or without `0x`, that fits in 32 bits.
    pub fn parse_txt(value: &str) -> Option<StatusFlags> {
        let value = txt::parse_hex_number(value)?;
        u32::try_from(value).ok().map(StatusFlags)
    }

    pub fn has(self, bit: u32) -> bool {
        bit < 32 && self.0 >> bit & 1 == 1
    }
}

/// An address with its interface scope (non-zero only for IPv6 link-local).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopedAddr {
    pub ip: IpAddr,
    pub scope_id: u32,
}

impl ScopedAddr {
    pub fn new(ip: IpAddr) -> ScopedAddr {
        ScopedAddr { ip, scope_id: 0 }
    }

    pub fn with_scope(ip: IpAddr, scope_id: u32) -> ScopedAddr {
        ScopedAddr { ip, scope_id }
    }
}

/// One resolved service: instance name, SRV target and port, TXT strings and
/// the target's addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRecord {
    pub kind: ServiceKind,
    pub instance: String,
    /// The SRV target host.
    pub host: String,
    /// The SRV port.
    pub port: u16,
    /// The raw TXT strings.
    pub txt: Vec<Vec<u8>>,
    pub addresses: Vec<ScopedAddr>,
}

impl ServiceRecord {
    /// The device this record belongs to: its identifier (from the RAOP
    /// instance name or the AirPlay `deviceid`), or, when that is malformed,
    /// the instance itself, so the service is still listed.
    pub fn device_key(&self) -> DeviceKey {
        let id = match self.kind {
            ServiceKind::Raop => parse_raop_instance(&self.instance).map(|(id, _)| id),
            ServiceKind::AirPlay => Txt::parse(&self.txt)
                .string("deviceid")
                .and_then(DeviceId::from_colon_form),
        };
        match id {
            Some(id) => DeviceKey::Id(id),
            None => DeviceKey::Instance(self.kind, self.instance.clone()),
        }
    }
}

/// What a browser reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseEvent {
    /// A service was resolved, or one of its records changed.
    Resolved(ServiceRecord),
    /// A service said goodbye or expired (P18).
    Removed { kind: ServiceKind, instance: String },
}

/// The seam for a live mDNS browser (a later stage): it browses both service
/// types in `local.` (P7) and hands over what it found.
pub trait Browser {
    /// The next event, if one is ready; never blocks.
    fn poll_event(&mut self) -> Option<BrowseEvent>;
}

/// How the directory identifies a device.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeviceKey {
    Id(DeviceId),
    /// A service without a usable identifier, listed on its own.
    Instance(ServiceKind, String),
}

/// Where one service of a device answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub instance: String,
    /// The SRV target host.
    pub host: String,
    /// The SRV port.
    pub port: u16,
    pub addresses: Vec<ScopedAddr>,
}

impl Endpoint {
    fn from_record(record: &ServiceRecord) -> Endpoint {
        Endpoint {
            instance: record.instance.clone(),
            host: record.host.clone(),
            port: record.port,
            addresses: record.addresses.clone(),
        }
    }
}

/// What the sender concludes about a device from its records (P16), reported
/// to the host with the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// RAOP `pw` true, AirPlay `pw` present and not false, or flags bit 7.
    pub needs_password: bool,
    /// Flags bit 3 or 9.
    pub needs_pairing: bool,
    /// Flags bit 17: another sender may be playing (P137). Only a hint.
    pub busy_hint: bool,
    /// An Apple TV that needs the legacy PIN pairing (P46).
    pub legacy_pairing: bool,
    /// Whether AirPlay 1 can drive the device, and if not, why.
    pub airplay1: Result<(), UnsupportedReason>,
    /// Any of features bits 38, 43, 46 or 48.
    pub airplay2: bool,
    /// Features bit 40, noted.
    pub buffered_audio: bool,
    /// Features bit 41, noted.
    pub ptp: bool,
}

/// The security modes of P45 that apply to a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecurityMode {
    /// (a) `et` allows unencrypted audio and neither a password nor a pairing
    /// is needed; a missing `et` means "try unencrypted" (P16).
    Open,
    /// (b) Digest authentication (P36 to P39).
    Password,
    /// (c) Apple TV legacy pairing (P46 to P58).
    LegacyPairing,
    /// (d) RSA, `et` 1 without 0: unsupported (P59).
    Rsa,
    /// (e) AirPlay 2 HomeKit-style pairing, partly documented (P61 to P71).
    HomeKit,
}

/// The protocol path for a device (P2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolPath {
    AirPlay1,
    AirPlay2,
}

/// One device: up to one service of each kind, their TXT keys and the
/// verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub key: DeviceKey,
    /// The `_airplay._tcp` instance name when that service exists, otherwise
    /// the part of the RAOP name after `@` (P10).
    pub name: String,
    pub raop: Option<Endpoint>,
    pub airplay: Option<Endpoint>,
    pub raop_txt: Option<RaopTxt>,
    pub airplay_txt: Option<AirPlayTxt>,
    pub verdict: Verdict,
}

impl Device {
    /// Merges the records of one device. `None` when there is neither.
    pub fn from_records(
        key: DeviceKey,
        raop: Option<&ServiceRecord>,
        airplay: Option<&ServiceRecord>,
    ) -> Option<Device> {
        let name = match (airplay, raop) {
            (Some(record), _) => record.instance.clone(),
            (None, Some(record)) => match parse_raop_instance(&record.instance) {
                Some((_, name)) => name.to_owned(),
                None => record.instance.clone(),
            },
            (None, None) => return None,
        };
        let raop_txt = raop.map(|record| RaopTxt::read(&Txt::parse(&record.txt)));
        let airplay_txt = airplay.map(|record| AirPlayTxt::read(&Txt::parse(&record.txt)));
        let mut device = Device {
            key,
            name,
            raop: raop.map(Endpoint::from_record),
            airplay: airplay.map(Endpoint::from_record),
            raop_txt,
            airplay_txt,
            verdict: Verdict {
                needs_password: false,
                needs_pairing: false,
                busy_hint: false,
                legacy_pairing: false,
                airplay1: Ok(()),
                airplay2: false,
                buffered_audio: false,
                ptp: false,
            },
        };
        device.verdict = device.compute_verdict();
        Some(device)
    }

    /// RAOP `sf` and AirPlay `flags` together.
    pub fn status_flags(&self) -> StatusFlags {
        let raop = self.raop_txt.as_ref().and_then(|txt| txt.status_flags);
        let airplay = self.airplay_txt.as_ref().and_then(|txt| txt.flags);
        StatusFlags(raop.unwrap_or_default().0 | airplay.unwrap_or_default().0)
    }

    /// AirPlay `features` when present, otherwise RAOP `ft` (the same mask,
    /// P14).
    pub fn features(&self) -> Features {
        self.airplay_txt
            .as_ref()
            .and_then(|txt| txt.features)
            .or_else(|| self.raop_txt.as_ref().and_then(|txt| txt.features))
            .unwrap_or_default()
    }

    /// RAOP `am` or AirPlay `model` starts with `AppleTV` (P46).
    pub fn is_apple_tv(&self) -> bool {
        let starts = |model: Option<&String>| model.is_some_and(|m| m.starts_with("AppleTV"));
        starts(self.raop_txt.as_ref().and_then(|txt| txt.model.as_ref()))
            || starts(self.airplay_txt.as_ref().and_then(|txt| txt.model.as_ref()))
    }

    fn compute_verdict(&self) -> Verdict {
        let flags = self.status_flags();
        let features = self.features();
        let raop_password = self.raop_txt.as_ref().and_then(|txt| txt.password) == Some(true);
        let airplay_password = self.airplay_txt.as_ref().and_then(|txt| txt.password) == Some(true);
        let needs_pairing = flags.has(StatusFlags::PIN_REQUIRED)
            || flags.has(StatusFlags::ONE_TIME_PAIRING_REQUIRED);
        let legacy_pairing = self.is_apple_tv() && needs_pairing;
        Verdict {
            needs_password: raop_password
                || airplay_password
                || flags.has(StatusFlags::PASSWORD_REQUIRED),
            needs_pairing,
            busy_hint: flags.has(StatusFlags::SESSION_ACTIVE),
            legacy_pairing,
            airplay1: self.airplay1_capability(legacy_pairing),
            airplay2: [
                Features::COREUTILS_PAIRING,
                Features::SYSTEM_PAIRING,
                Features::HOMEKIT_PAIRING,
                Features::TRANSIENT_PAIRING,
            ]
            .into_iter()
            .any(|bit| features.has(bit)),
            buffered_audio: features.has(Features::BUFFERED_AUDIO),
            ptp: features.has(Features::PTP),
        }
    }

    /// The AirPlay 1 conditions of P16, checked in the order the clause lists
    /// them. Without a RAOP service only the AirPlay 2 path could serve the
    /// device.
    fn airplay1_capability(&self, legacy_pairing: bool) -> Result<(), UnsupportedReason> {
        let Some(txt) = &self.raop_txt else {
            return Err(UnsupportedReason::AirPlay2Only);
        };
        let absent_or_has = |list: &Option<Vec<u64>>, wanted: &[u64]| {
            list.as_ref()
                .is_none_or(|list| list.iter().any(|value| wanted.contains(value)))
        };
        if !absent_or_has(&txt.encryption, &[0]) && !legacy_pairing {
            return Err(UnsupportedReason::Encryption);
        }
        if !absent_or_has(&txt.codecs, &[0, 1]) {
            return Err(UnsupportedReason::Codec);
        }
        let udp = txt.transports.as_ref().is_none_or(|transports| {
            transports
                .iter()
                .any(|transport| transport.eq_ignore_ascii_case("UDP"))
        });
        if !udp {
            return Err(UnsupportedReason::Transport);
        }
        let format_ok = txt.sample_rate.is_none_or(|rate| rate == 44_100)
            && txt.sample_size.is_none_or(|size| size == 16)
            && txt.channels.is_none_or(|channels| channels == 2);
        if !format_ok {
            return Err(UnsupportedReason::Format);
        }
        Ok(())
    }

    /// The modes of P45 that apply to this device.
    pub fn security_modes(&self) -> Vec<SecurityMode> {
        let encryption = self
            .raop_txt
            .as_ref()
            .and_then(|txt| txt.encryption.as_ref());
        let unencrypted = encryption.is_none_or(|list| list.contains(&0));
        let rsa_only = encryption.is_some_and(|list| list.contains(&1) && !list.contains(&0));
        let verdict = &self.verdict;
        let mut modes = Vec::new();
        if self.raop.is_some() && unencrypted && !verdict.needs_password && !verdict.needs_pairing {
            modes.push(SecurityMode::Open);
        }
        if verdict.needs_password {
            modes.push(SecurityMode::Password);
        }
        if verdict.legacy_pairing {
            modes.push(SecurityMode::LegacyPairing);
        }
        if rsa_only {
            modes.push(SecurityMode::Rsa);
        }
        if verdict.airplay2 {
            modes.push(SecurityMode::HomeKit);
        }
        modes
    }

    /// Whether to send `POST /auth-setup` right after OPTIONS: RAOP `et`
    /// contains 4, or features set bit 26 or 51 (P60).
    pub fn needs_auth_setup(&self) -> bool {
        let et4 = self
            .raop_txt
            .as_ref()
            .and_then(|txt| txt.encryption.as_ref())
            .is_some_and(|list| list.contains(&4));
        let features = self.features();
        et4 || features.has(Features::BIT_26) || features.has(Features::UNIFIED_PAIR_SETUP_MFI)
    }

    /// The metadata types RAOP `md` lists (P123).
    pub fn metadata_support(&self) -> MetadataSupport {
        MetadataSupport::from_raop_md(
            self.raop_txt
                .as_ref()
                .and_then(|txt| txt.metadata.as_deref()),
        )
    }

    /// The codec for an AirPlay 1 session (P74): PCM when `cn` contains 0 and
    /// either `pcm_allowed` (the host enabled PCM for the device and no PCM
    /// ANNOUNCE was refused before) or `cn` lacks 1; otherwise ALAC when `cn`
    /// contains 1 or is absent (a missing `cn` means ALAC only, P16);
    /// otherwise the device is unsupported.
    pub fn choose_codec(&self, pcm_allowed: bool) -> Result<Codec, UnsupportedReason> {
        let codecs = self.raop_txt.as_ref().and_then(|txt| txt.codecs.as_ref());
        let Some(codecs) = codecs else {
            return Ok(Codec::Alac);
        };
        let pcm = codecs.contains(&0);
        let alac = codecs.contains(&1);
        if pcm && (pcm_allowed || !alac) {
            Ok(Codec::Pcm)
        } else if alac {
            Ok(Codec::Alac)
        } else {
            Err(UnsupportedReason::Codec)
        }
    }

    /// Whether a refused PCM ANNOUNCE is retried with ALAC: `cn` contains 1
    /// (P74).
    pub fn alac_fallback(&self) -> bool {
        self.raop_txt
            .as_ref()
            .and_then(|txt| txt.codecs.as_ref())
            .is_some_and(|codecs| codecs.contains(&1))
    }

    /// The protocol path (P2). AirPlay 1 serves every device it can, AirPlay 2
    /// devices included. The AirPlay 2 path is chosen only when it is enabled,
    /// the device is AirPlay 2 capable, and AirPlay 1 can't serve it: no RAOP
    /// service, a RAOP record that is unsupported, or an AirPlay 1 attempt
    /// that ended in Unsupported or Refused (`airplay1_outcome`). Any other
    /// outcome (password, pairing, busy, ...) never switches paths. With the
    /// path off, such a device is Unsupported with "AirPlay 2 only".
    pub fn choose_path(
        &self,
        airplay2_enabled: bool,
        airplay1_outcome: Option<ErrorKind>,
    ) -> Result<ProtocolPath, UnsupportedReason> {
        let attempt_failed = matches!(
            airplay1_outcome,
            Some(ErrorKind::Unsupported(_) | ErrorKind::Refused)
        );
        let airplay1_reason = match (self.verdict.airplay1, airplay1_outcome) {
            (Err(reason), _) => Some(reason),
            (Ok(()), Some(ErrorKind::Unsupported(reason))) => Some(reason),
            _ => None,
        };
        if self.raop.is_some() && !attempt_failed && airplay1_reason.is_none() {
            return Ok(ProtocolPath::AirPlay1);
        }
        if self.verdict.airplay2 {
            return if airplay2_enabled {
                Ok(ProtocolPath::AirPlay2)
            } else {
                Err(UnsupportedReason::AirPlay2Only)
            };
        }
        match airplay1_reason {
            Some(reason) => Err(reason),
            // Refused, without an AirPlay 2 path to switch to: AirPlay 1 stays.
            None => Ok(ProtocolPath::AirPlay1),
        }
    }

    /// Whether an AirPlay 1 session may start (P46, P139): an unsupported
    /// device is reported without connecting, and an Apple TV that needs the
    /// legacy pairing reports PairingRequired until the host has stored
    /// credentials.
    pub fn airplay1_plan(&self, has_credentials: bool) -> Result<(), ErrorKind> {
        self.verdict.airplay1.map_err(ErrorKind::Unsupported)?;
        if self.verdict.legacy_pairing && !has_credentials {
            return Err(ErrorKind::PairingRequired);
        }
        Ok(())
    }

    /// The RAOP service's addresses in connect order (P17).
    pub fn connect_addresses(&self) -> Vec<ScopedAddr> {
        self.raop
            .as_ref()
            .map(|endpoint| connect_order(&endpoint.addresses))
            .unwrap_or_default()
    }
}

/// The connect order of P17: IPv4, then IPv6 global, then IPv6 link-local
/// with its scope. An IPv4 link-local address (169.254/16) is used only when
/// there is no other address. Duplicates are dropped; each group keeps the
/// order it came in. Every IPv6 address outside fe80::/10 counts as global.
pub fn connect_order(addresses: &[ScopedAddr]) -> Vec<ScopedAddr> {
    let rank = |address: &ScopedAddr| match address.ip {
        IpAddr::V4(ip) if ip.is_link_local() => 3,
        IpAddr::V4(_) => 0,
        IpAddr::V6(ip) if (ip.segments()[0] & 0xffc0) == 0xfe80 => 2,
        IpAddr::V6(_) => 1,
    };
    let mut unique: Vec<ScopedAddr> = Vec::new();
    for address in addresses {
        if !unique.contains(address) {
            unique.push(*address);
        }
    }
    unique.sort_by_key(rank);
    if unique.iter().any(|address| rank(address) != 3) {
        unique.retain(|address| rank(address) != 3);
    }
    unique
}

/// A change the directory reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryEvent {
    /// A device appeared, or its records or verdict changed (P19).
    Updated(Box<Device>),
    /// The device's last service went away (P18).
    Removed(DeviceKey),
}

/// The devices on the network, merged from browse events (P10).
#[derive(Debug, Default)]
pub struct Directory {
    services: BTreeMap<(ServiceKind, String), (DeviceKey, ServiceRecord)>,
    devices: BTreeMap<DeviceKey, Device>,
    pcm_refused: BTreeSet<DeviceKey>,
}

impl Directory {
    pub fn new() -> Directory {
        Directory::default()
    }

    /// Applies one browse event and reports what changed. A device keeps at
    /// most one service of each kind; a newer instance of the same kind
    /// replaces the older one. Running sessions are never touched (P18,
    /// P19).
    pub fn apply(&mut self, event: BrowseEvent) -> Vec<DirectoryEvent> {
        match event {
            BrowseEvent::Resolved(record) => {
                let key = record.device_key();
                let id = (record.kind, record.instance.clone());
                let mut affected = vec![key.clone()];
                if let Some((old_key, _)) = self.services.remove(&id) {
                    if old_key != key {
                        affected.push(old_key);
                    }
                }
                self.services
                    .retain(|(kind, _), (owner, _)| !(*kind == record.kind && *owner == key));
                self.services.insert(id, (key, record));
                affected.into_iter().map(|key| self.rebuild(key)).collect()
            }
            BrowseEvent::Removed { kind, instance } => {
                match self.services.remove(&(kind, instance)) {
                    Some((key, _)) => vec![self.rebuild(key)],
                    None => Vec::new(),
                }
            }
        }
    }

    fn rebuild(&mut self, key: DeviceKey) -> DirectoryEvent {
        let find = |kind: ServiceKind| {
            self.services
                .values()
                .find(|(owner, record)| *owner == key && record.kind == kind)
                .map(|(_, record)| record)
        };
        match Device::from_records(
            key.clone(),
            find(ServiceKind::Raop),
            find(ServiceKind::AirPlay),
        ) {
            Some(device) => {
                self.devices.insert(key, device.clone());
                DirectoryEvent::Updated(Box::new(device))
            }
            None => {
                self.devices.remove(&key);
                self.pcm_refused.remove(&key);
                DirectoryEvent::Removed(key)
            }
        }
    }

    pub fn device(&self, key: &DeviceKey) -> Option<&Device> {
        self.devices.get(key)
    }

    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.devices.values()
    }

    /// Remembers that the device refused a PCM ANNOUNCE, so later sessions
    /// use ALAC (P74).
    pub fn remember_pcm_refused(&mut self, key: &DeviceKey) {
        self.pcm_refused.insert(key.clone());
    }

    /// The codec for a new session with the device (P74), given whether the
    /// host enabled PCM for it.
    pub fn codec_for(
        &self,
        key: &DeviceKey,
        pcm_enabled: bool,
    ) -> Option<Result<Codec, UnsupportedReason>> {
        let device = self.devices.get(key)?;
        Some(device.choose_codec(pcm_enabled && !self.pcm_refused.contains(key)))
    }
}
