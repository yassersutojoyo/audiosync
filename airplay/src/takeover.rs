//! A receiver another sender took (P249): the evidence window after the
//! receiver closed our connection, and what counts as evidence in a TXT
//! update.

use std::collections::BTreeSet;
use std::time::Duration;

use plist::Value;

use crate::ap2::bodies;
use crate::discovery::txt::group_key;
use crate::discovery::{AirPlayTxt, Device, StatusFlags, Txt};

/// How long after a receiver's close no retry starts while evidence of a
/// takeover may still arrive (P249).
pub const EVIDENCE_WINDOW: Duration = Duration::from_secs(10);

/// What the crate reports about a receiver that closed our connection
/// (P249).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeoverReport {
    /// The receiver closed the connection with no TEARDOWN: no attempt
    /// starts before the window ends.
    ClosedByReceiver,
    /// Another sender has it, with that sender's group key when known. No
    /// session starts until the host adds the speaker again.
    Taken { group: Option<String> },
    /// The window ended with no evidence. No session starts until the host
    /// adds the speaker again (S482).
    NoEvidence,
}

/// The group key of `device`'s `gid` when it shows another sender: it
/// changed from `gid_at_close`, and none of its `+` fields is a
/// `groupUUID` this sender used (`own`, lowercased) or the receiver's own
/// `pi`. Flags bit 17 plays no part (P174).
pub fn foreign_group(
    device: &Device,
    gid_at_close: Option<&str>,
    own: &BTreeSet<String>,
) -> Option<String> {
    let txt = device.airplay_txt.as_ref()?;
    let gid = txt.group.gid.as_deref()?;
    foreign(gid, txt.pairing_id.as_deref(), gid_at_close, own)
}

/// The group key of another sender in an `/info` answer (P249 (b)): flags
/// bit 17 together with a `gid` that [`foreign_group`] would count, read
/// from the answer's own keys or from its `txtAirPlay`.
pub fn info_evidence(
    body: &[u8],
    gid_at_close: Option<&str>,
    own: &BTreeSet<String>,
) -> Option<String> {
    let flags = bodies::read_info(body).status_flags?;
    if !StatusFlags(flags as u32).has(StatusFlags::SESSION_ACTIVE) {
        return None;
    }
    let dictionary = bodies::read_dictionary(body)?;
    let txt = dictionary
        .get("txtAirPlay")
        .and_then(Value::as_data)
        .map(|data| AirPlayTxt::read(&Txt::parse(&bodies::wire_strings(data))));
    let text = |key: &str| dictionary.get(key)?.as_string().map(str::to_owned);
    let gid = text("gid").or_else(|| txt.as_ref()?.group.gid.clone())?;
    let pi = text("pi").or_else(|| txt.as_ref()?.pairing_id.clone());
    foreign(gid.trim(), pi.as_deref(), gid_at_close, own)
}

fn foreign(
    gid: &str,
    pi: Option<&str>,
    gid_at_close: Option<&str>,
    own: &BTreeSet<String>,
) -> Option<String> {
    if gid_at_close.is_some_and(|old| old.eq_ignore_ascii_case(gid)) {
        return None;
    }
    let pi = pi.map(str::to_ascii_lowercase);
    let ours = gid
        .split('+')
        .map(|field| field.trim().to_ascii_lowercase())
        .any(|field| {
            field.is_empty() || own.contains(&field) || pi.as_deref() == Some(field.as_str())
        });
    if ours {
        return None;
    }
    group_key(gid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord};

    const PI: &str = "5C1D6A7E-0000-4000-8000-00000000AAAA";
    const OURS: &str = "0a0b0c0d-0000-4000-8000-000000000001";
    const OTHER: &str = "9F8E7D6C-1111-4222-8333-444455556666";

    fn device(txt: &[String]) -> Device {
        let record = ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: "Speaker".to_owned(),
            host: "speaker.local".to_owned(),
            port: 7000,
            txt: txt.iter().map(|s| s.clone().into_bytes()).collect(),
            addresses: vec![ScopedAddr::new([127, 0, 0, 1].into())],
        };
        let key = DeviceKey::Id(DeviceId([2, 0, 0x5E, 0, 0x53, 1]));
        Device::from_records(key, None, Some(&record)).expect("device")
    }

    fn with_gid(gid: &str, flags: &str) -> Device {
        device(&[
            "features=0x0,0x4000".to_owned(),
            format!("flags={flags}"),
            "deviceid=02:00:5E:00:53:01".to_owned(),
            format!("pi={PI}"),
            format!("gid={gid}"),
        ])
    }

    fn own() -> BTreeSet<String> {
        [OURS.to_owned()].into_iter().collect()
    }

    // covers: P249
    #[test]
    fn another_senders_group_is_evidence() {
        let found = foreign_group(&with_gid(OTHER, "0x4"), Some(PI), &own());
        assert_eq!(found, Some(OTHER.to_ascii_lowercase()));
        let plus = format!("{OTHER}+0+{PI}");
        assert!(foreign_group(&with_gid(&plus, "0x4"), Some(PI), &own()).is_none());
    }

    // T8 and T9: bit 17 alone, an unchanged gid, the receiver's own pi and
    // a group of ours are no evidence.
    // covers: P249
    #[test]
    fn bit_17_an_unchanged_gid_pi_and_our_groups_are_no_evidence() {
        assert!(foreign_group(&with_gid(OTHER, "0x20004"), Some(OTHER), &own()).is_none());
        assert!(foreign_group(&with_gid(PI, "0x20004"), Some(OURS), &own()).is_none());
        let ours = OURS.to_ascii_uppercase();
        assert!(foreign_group(&with_gid(&ours, "0x4"), Some(PI), &own()).is_none());
        assert!(foreign_group(&with_gid("", "0x4"), Some(PI), &own()).is_none());
    }

    fn info(entries: Vec<(&str, Value)>) -> Vec<u8> {
        let dictionary = entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        bodies::binary(Value::Dictionary(dictionary))
    }

    // T3: an `/info` answer with bit 17 and another sender's gid is
    // evidence, in its own keys or in its `txtAirPlay`; without bit 17, or
    // with the receiver's own pi, it is not.
    // covers: P249
    #[test]
    fn an_info_answer_with_bit_17_and_another_gid_is_evidence() {
        let flags = |value: u64| ("statusFlags", Value::Integer(value.into()));
        let text = |value: &str| Value::String(value.to_owned());
        let keys = info(vec![flags(0x20004), ("gid", text(OTHER)), ("pi", text(PI))]);
        let found = info_evidence(&keys, Some(PI), &own());
        assert_eq!(found, Some(OTHER.to_ascii_lowercase()));
        let no_bit = info(vec![flags(0x4), ("gid", text(OTHER))]);
        assert!(info_evidence(&no_bit, Some(PI), &own()).is_none());
        let pi = info(vec![flags(0x20004), ("gid", text(PI)), ("pi", text(PI))]);
        assert!(info_evidence(&pi, Some(OURS), &own()).is_none());

        let mut wire = Vec::new();
        for entry in ["flags=0x20004".to_owned(), format!("gid={OTHER}")] {
            wire.push(entry.len() as u8);
            wire.extend_from_slice(entry.as_bytes());
        }
        let txt = info(vec![("txtAirPlay", Value::Data(wire))]);
        let found = info_evidence(&txt, Some(PI), &own());
        assert_eq!(found, Some(OTHER.to_ascii_lowercase()));
    }
}
