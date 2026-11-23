//! A receiver another sender took (P249): the evidence window after the
//! receiver closed our connection, and what counts as evidence in a TXT
//! update.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::discovery::Device;

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
    if gid_at_close.is_some_and(|old| old.eq_ignore_ascii_case(gid)) {
        return None;
    }
    let pi = txt.pairing_id.as_deref().map(str::to_ascii_lowercase);
    let ours = gid
        .split('+')
        .map(|field| field.trim().to_ascii_lowercase())
        .any(|field| {
            field.is_empty() || own.contains(&field) || pi.as_deref() == Some(field.as_str())
        });
    if ours {
        return None;
    }
    txt.group.group_key()
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
}
