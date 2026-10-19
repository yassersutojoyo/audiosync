//! TXT records of the two services and the keys the sender reads (P11, P12,
//! P13).
//!
//! Each TXT string is `key=value`. Keys compare case-insensitively, a key
//! without `=` is a boolean true, lists are comma-separated, booleans accept
//! `true`, `false`, `1` and `0` in any case, and integers accept decimal or
//! `0x` hex. Unknown keys are ignored and a malformed value drops only its
//! own key: every accessor returns `None` for it, as if it were absent.

use super::{DeviceId, Features, StatusFlags};

/// A parsed TXT record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Txt {
    /// Lowercased key and value; `None` for a key without `=`.
    entries: Vec<(String, Option<String>)>,
}

impl Txt {
    /// Reads the strings of a TXT record. Empty strings and strings that
    /// start with `=` carry no key and are skipped; when a key appears more
    /// than once, only the first counts (RFC 6763 §6.4). Values that aren't
    /// UTF-8 are read with replacement characters, so they fail any typed
    /// accessor.
    pub fn parse<S: AsRef<[u8]>>(strings: &[S]) -> Txt {
        let mut entries: Vec<(String, Option<String>)> = Vec::new();
        for string in strings {
            let text = String::from_utf8_lossy(string.as_ref());
            let (key, value) = match text.split_once('=') {
                Some((key, value)) => (key, Some(value.to_owned())),
                None => (text.as_ref(), None),
            };
            if key.is_empty() {
                continue;
            }
            let key = key.to_ascii_lowercase();
            if entries.iter().any(|(known, _)| *known == key) {
                continue;
            }
            entries.push((key, value));
        }
        Txt { entries }
    }

    /// The raw entry: `Some(None)` for a key without `=`.
    pub fn raw(&self, key: &str) -> Option<Option<&str>> {
        let key = key.to_ascii_lowercase();
        self.entries
            .iter()
            .find(|(known, _)| *known == key)
            .map(|(_, value)| value.as_deref())
    }

    /// A text value; `None` for a missing, empty or value-less key.
    pub fn string(&self, key: &str) -> Option<&str> {
        self.raw(key).flatten().filter(|value| !value.is_empty())
    }

    /// A boolean: a key without `=` is true.
    pub fn flag(&self, key: &str) -> Option<bool> {
        match self.raw(key)? {
            None => Some(true),
            Some(value) => parse_bool(value),
        }
    }

    /// An integer in decimal or `0x` hex.
    pub fn int(&self, key: &str) -> Option<u64> {
        parse_int(self.string(key)?)
    }

    /// A comma-separated list of integers; one malformed item drops the key.
    pub fn int_list(&self, key: &str) -> Option<Vec<u64>> {
        self.string(key)?
            .split(',')
            .map(|item| parse_int(item.trim()))
            .collect()
    }

    /// A comma-separated list of words, trimmed, empty items left out.
    pub fn list(&self, key: &str) -> Option<Vec<String>> {
        Some(
            self.string(key)?
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    /// Bytes written as hex digits (`pk`).
    pub fn hex_bytes(&self, key: &str) -> Option<Vec<u8>> {
        parse_hex_bytes(self.string(key)?)
    }

    /// A features bitmask (P14).
    pub fn features(&self, key: &str) -> Option<Features> {
        Features::parse_txt(self.string(key)?)
    }

    /// Status flags in hex (P15).
    pub fn status_flags(&self, key: &str) -> Option<StatusFlags> {
        StatusFlags::parse_txt(self.string(key)?)
    }
}

/// `true`, `false`, `1` or `0`, in any case.
pub fn parse_bool(text: &str) -> Option<bool> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("true") || text == "1" {
        Some(true)
    } else if text.eq_ignore_ascii_case("false") || text == "0" {
        Some(false)
    } else {
        None
    }
}

/// Decimal, or hex after `0x` or `0X`.
pub fn parse_int(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => parse_hex(hex),
        None if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => text.parse().ok(),
        None => None,
    }
}

/// Hex digits, with or without `0x`, as a number.
pub(crate) fn parse_hex_number(text: &str) -> Option<u64> {
    let text = text.trim();
    parse_hex(
        text.strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .unwrap_or(text),
    )
}

fn parse_hex(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

fn parse_hex_bytes(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.is_empty()
        || !text.len().is_multiple_of(2)
        || !text.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

/// The RAOP TXT keys of P12.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RaopTxt {
    /// `txtvers`, informational.
    pub txt_version: Option<String>,
    /// `ch`, must be 2 when present.
    pub channels: Option<u64>,
    /// `sr`, must be 44100 when present.
    pub sample_rate: Option<u64>,
    /// `ss`, must be 16 when present.
    pub sample_size: Option<u64>,
    /// `cn`: 0 PCM, 1 ALAC, 2 AAC, 3 AAC-ELD, 4 Opus.
    pub codecs: Option<Vec<u64>>,
    /// `et`: 0 none, 1 RSA, 3 FairPlay, 4 MFi-SAP, 5 FairPlay SAPv2.5.
    pub encryption: Option<Vec<u64>>,
    /// `md`: 0 text, 1 artwork, 2 progress.
    pub metadata: Option<Vec<u64>>,
    /// `pw`: a password is required.
    pub password: Option<bool>,
    /// `da`: digest authentication supported, informational.
    pub digest_auth: Option<bool>,
    /// `tp`: `UDP` and/or `TCP`.
    pub transports: Option<Vec<String>>,
    /// `sv`: the sender applies the volume to the samples (P121).
    pub software_volume: Option<bool>,
    /// `sm`: the sender applies mute to the samples (P121).
    pub software_mute: Option<bool>,
    /// `sf`: status flags.
    pub status_flags: Option<StatusFlags>,
    /// `ft`: features.
    pub features: Option<Features>,
    /// `am`: model.
    pub model: Option<String>,
    /// `vs`: server version.
    pub server_version: Option<String>,
    /// `vn`: protocol version.
    pub protocol_version: Option<String>,
    /// `fv`: firmware version.
    pub firmware_version: Option<String>,
    /// `ov`: OS version.
    pub os_version: Option<String>,
    /// `pk`: the receiver's public key.
    pub public_key: Option<Vec<u8>>,
}

impl RaopTxt {
    pub fn read(txt: &Txt) -> RaopTxt {
        RaopTxt {
            txt_version: txt.string("txtvers").map(str::to_owned),
            channels: txt.int("ch"),
            sample_rate: txt.int("sr"),
            sample_size: txt.int("ss"),
            codecs: txt.int_list("cn"),
            encryption: txt.int_list("et"),
            metadata: txt.int_list("md"),
            password: txt.flag("pw"),
            digest_auth: txt.flag("da"),
            transports: txt.list("tp"),
            software_volume: txt.flag("sv"),
            software_mute: txt.flag("sm"),
            status_flags: txt.status_flags("sf"),
            features: txt.features("ft"),
            model: txt.string("am").map(str::to_owned),
            server_version: txt.string("vs").map(str::to_owned),
            protocol_version: txt.string("vn").map(str::to_owned),
            firmware_version: txt.string("fv").map(str::to_owned),
            os_version: txt.string("ov").map(str::to_owned),
            public_key: txt.hex_bytes("pk"),
        }
    }
}

/// The AirPlay TXT keys of P13. Group state, access control and required
/// sender features are kept as text: the sender only reports them (P19).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AirPlayTxt {
    /// `deviceid`: six colon-separated hex bytes (P9).
    pub device_id: Option<DeviceId>,
    pub features: Option<Features>,
    pub flags: Option<StatusFlags>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    /// `srcvers`: the AirPlay version.
    pub source_version: Option<String>,
    pub protocol_version: Option<String>,
    /// `pw`: present (or true) when a password is set.
    pub password: Option<bool>,
    /// `pk`: a 32-byte public key; any other length drops the key.
    pub public_key: Option<[u8; 32]>,
    /// `pi`: pairing identity.
    pub pairing_id: Option<String>,
    /// `psi`: pairing identity.
    pub pairing_system_id: Option<String>,
    /// `gid`: group ID.
    pub group_id: Option<String>,
    /// `igl`: group state.
    pub is_group_leader: Option<String>,
    /// `gcgl`: group state.
    pub group_contains_group_leader: Option<String>,
    /// `acl`: access control level.
    pub access_control: Option<String>,
    /// `rsf`: required sender features.
    pub required_sender_features: Option<String>,
    pub firmware_version: Option<String>,
    pub os_version: Option<String>,
}

impl AirPlayTxt {
    pub fn read(txt: &Txt) -> AirPlayTxt {
        let text = |key: &str| txt.string(key).map(str::to_owned);
        AirPlayTxt {
            device_id: txt.string("deviceid").and_then(DeviceId::from_colon_form),
            features: txt.features("features"),
            flags: txt.status_flags("flags"),
            model: text("model"),
            manufacturer: text("manufacturer"),
            source_version: text("srcvers"),
            protocol_version: text("protovers"),
            password: txt.flag("pw"),
            public_key: txt
                .hex_bytes("pk")
                .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok()),
            pairing_id: text("pi"),
            pairing_system_id: text("psi"),
            group_id: text("gid"),
            is_group_leader: text("igl"),
            group_contains_group_leader: text("gcgl"),
            access_control: text("acl"),
            required_sender_features: text("rsf"),
            firmware_version: text("fv"),
            os_version: text("osvers"),
        }
    }
}
