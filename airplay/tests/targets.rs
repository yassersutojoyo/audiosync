//! Stereo pairs and home theater groups recognized from the group keys of
//! discovery (P172 to P176), and the target the sender reports for them
//! through a browser.

use std::time::Duration;

use audiosync_airplay::discovery::{
    BrowseEvent, Browser, Device, DeviceId, DeviceKey, GroupKeys, ScopedAddr, ServiceKind,
    ServiceRecord, StatusFlags, Txt,
};
use audiosync_airplay::sender::{ReceiverEvent, Sender, SenderConfig, SenderEvent};
use audiosync_airplay::targets::{TargetChange, TargetId, TargetKind, Targets, find_targets};

const TSID: &str = "A1B2C3D4-0000-4000-8000-00000000AAAA";
const OTHER_UUID: &str = "11111111-2222-4333-8444-555555555555";
const TV_GROUP: &str = "C0FFEE00-1111-4222-8333-444455556666";

fn id(n: u8) -> DeviceId {
    DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, n])
}

fn record(n: u8, name: &str, txt: &[&str]) -> ServiceRecord {
    let mut strings: Vec<Vec<u8>> = txt.iter().map(|s| s.as_bytes().to_vec()).collect();
    strings.push(format!("deviceid=02:00:5E:00:53:{n:02X}").into_bytes());
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: name.to_owned(),
        host: format!("speaker-{n}.local"),
        port: 7000,
        txt: strings,
        addresses: vec![ScopedAddr::new([192, 0, 2, n].into())],
    }
}

fn device(n: u8, name: &str, txt: &[&str]) -> Device {
    Device::from_records(DeviceKey::Id(id(n)), None, Some(&record(n, name, txt))).expect("device")
}

fn key(n: u8) -> DeviceKey {
    DeviceKey::Id(id(n))
}

fn pair_member(n: u8, flags: &str, gid: &str) -> Device {
    device(
        n,
        &format!("HomePod {n}"),
        &[
            "model=AudioAccessory5,1",
            &format!("flags={flags}"),
            &format!("gid={gid}"),
            &format!("tsid={TSID}"),
            "gpn=Kitchen",
            "igl=1",
            "gcgl=1",
        ],
    )
}

// covers: P172
#[test]
fn group_keys_are_read_raw_with_both_gid_forms_giving_one_group_key() {
    let txt = Txt::parse(&[
        format!("gid={TSID}+0+{OTHER_UUID}"),
        "isGroupLeader=1".to_owned(),
        "gcgl=0".to_owned(),
        format!("pgid={TV_GROUP}"),
        "pgcgl=true".to_owned(),
        "gpn=Living Room".to_owned(),
        format!("tsid={TSID}"),
        "tsm=1".to_owned(),
        format!("psgid={TV_GROUP}"),
        "psgtp=1".to_owned(),
        "psgsz=3".to_owned(),
        "pgm=0".to_owned(),
        format!("hgid={OTHER_UUID}"),
    ]);
    let keys = GroupKeys::read(&txt);
    assert_eq!(
        keys.gid.as_deref(),
        Some(&*format!("{TSID}+0+{OTHER_UUID}"))
    );
    assert_eq!(
        keys.group_key().as_deref(),
        Some(&*TSID.to_ascii_lowercase())
    );
    assert_eq!(keys.gid_middle(), Some(0));
    assert_eq!(keys.is_group_leader, Some(true));
    assert_eq!(keys.contains_group_leader, Some(false));
    assert_eq!(keys.parent_group_id.as_deref(), Some(TV_GROUP));
    assert_eq!(keys.parent_contains_group_leader, Some(true));
    assert_eq!(keys.group_public_name.as_deref(), Some("Living Room"));
    assert_eq!(keys.tight_sync_id.as_deref(), Some(TSID));
    assert_eq!(keys.tsm, Some(1));
    assert_eq!(keys.psgid.as_deref(), Some(TV_GROUP));
    assert_eq!(keys.psgtp, Some(1));
    assert_eq!(keys.psgsz, Some(3));
    assert_eq!(keys.pgm, Some(0));
    assert_eq!(keys.home_group_id.as_deref(), Some(OTHER_UUID));

    // The plain form of the same group compares equal, in any case.
    let plain = GroupKeys::read(&Txt::parse(&[format!("gid={}", TSID.to_ascii_lowercase())]));
    assert_eq!(plain.group_key(), keys.group_key());
    assert_eq!(plain.gid_middle(), None);
    // `igl` is read like `isGroupLeader`.
    let igl = GroupKeys::read(&Txt::parse(&["igl=0"]));
    assert_eq!(igl.is_group_leader, Some(false));

    // A malformed value drops only its own key.
    let broken = GroupKeys::read(&Txt::parse(&[
        "igl=perhaps",
        "psgsz=three",
        "tsm=",
        "gpn=Den",
        format!("tsid={TSID}").as_str(),
    ]));
    assert_eq!(broken.is_group_leader, None);
    assert_eq!(broken.psgsz, None);
    assert_eq!(broken.tsm, None);
    assert_eq!(broken.group_public_name.as_deref(), Some("Den"));
    assert_eq!(broken.tight_sync_id.as_deref(), Some(TSID));

    // Status flags bits 11 to 14 are read with them.
    let flags = StatusFlags::parse_txt("0x7804").expect("flags");
    for bit in [
        StatusFlags::RELAYING_SUPPORTED,
        StatusFlags::SILENT_PRIMARY,
        StatusFlags::TIGHT_SYNC_LEADER,
        StatusFlags::TIGHT_SYNC_PARTNER_UNREACHABLE,
    ] {
        assert!(flags.has(bit), "bit {bit}");
    }
    // The device reports them raw.
    let member = pair_member(1, "0x2004", TSID);
    assert_eq!(member.group_keys().tight_sync_id.as_deref(), Some(TSID));
}

// covers: P173, P176
#[test]
fn a_stereo_pair_is_one_target_named_by_gpn_and_keyed_by_tsid() {
    let left = pair_member(2, "0x2004", &format!("{TSID}+0+{OTHER_UUID}"));
    let right = pair_member(1, "0x4", &format!("{TSID}+1+{OTHER_UUID}"));
    let targets = find_targets([&left, &right]);
    assert_eq!(targets.len(), 1, "{targets:?}");
    let pair = &targets[0];
    assert_eq!(pair.id.kind, TargetKind::StereoPair);
    assert_eq!(pair.id.key, TSID.to_ascii_lowercase());
    assert_eq!(pair.name, "Kitchen");
    let members: Vec<&DeviceKey> = pair.members.iter().map(|m| &m.key).collect();
    assert_eq!(members, vec![&key(1), &key(2)]);
    // Bit 13 decides, ahead of the lower identifier.
    assert_eq!(pair.leader, Some(key(2)));
    assert!(pair.complete());
    assert_eq!(pair.control, None);
    assert_eq!(pair.volume_source(), Some(&key(2)));

    // The identity survives `gid` switching form, and round-trips as text.
    let left_now = pair_member(2, "0x2004", OTHER_UUID);
    let right_now = pair_member(1, "0x4", OTHER_UUID);
    let again = find_targets([&left_now, &right_now]);
    assert_eq!(again[0].id, pair.id);
    assert_eq!(TargetId::parse(&pair.id.to_string()), Some(pair.id.clone()));
    assert_eq!(
        pair.id.to_string(),
        format!("pair:{}", TSID.to_ascii_lowercase())
    );
}

// covers: P173
#[test]
fn the_pair_leader_falls_back_to_the_gid_middle_field_then_the_lowest_identifier() {
    let a = pair_member(5, "0x4", &format!("{TSID}+1+{OTHER_UUID}"));
    let b = pair_member(6, "0x4", &format!("{TSID}+0+{OTHER_UUID}"));
    assert_eq!(find_targets([&a, &b])[0].leader, Some(key(6)));
    // Both set bit 13: the middle field decides.
    let a = pair_member(5, "0x2004", &format!("{TSID}+1+{OTHER_UUID}"));
    let b = pair_member(6, "0x2004", &format!("{TSID}+0+{OTHER_UUID}"));
    assert_eq!(find_targets([&a, &b])[0].leader, Some(key(6)));
    // Nothing decides: the lowest identifier.
    let a = pair_member(5, "0x4", OTHER_UUID);
    let b = pair_member(6, "0x4", OTHER_UUID);
    assert_eq!(find_targets([&a, &b])[0].leader, Some(key(5)));
}

// covers: P173, P216
#[test]
fn a_lone_member_is_an_incomplete_pair_with_bit_14_or_a_partner_seen_before() {
    let lone = pair_member(1, "0x4", TSID);
    assert!(find_targets([&lone]).is_empty());
    let unreachable = pair_member(1, "0x4004", TSID);
    let targets = find_targets([&unreachable]);
    assert_eq!(targets.len(), 1);
    assert!(!targets[0].complete());

    // Once the partner was seen, the incomplete pair names it.
    let partner = pair_member(2, "0x2004", TSID);
    let mut tracker = Targets::new();
    let changes = tracker.update([&unreachable, &partner]);
    assert_eq!(changes.len(), 1);
    let changes = tracker.update([&unreachable]);
    let [TargetChange::Updated(target)] = changes.as_slice() else {
        panic!("{changes:?}");
    };
    assert!(!target.complete());
    assert_eq!(target.missing().collect::<Vec<_>>(), vec![&key(2)]);
    assert_eq!(target.members.len(), 2);
    assert_eq!(tracker.target_of(&key(2)), Some(&target.id));
    // Without bit 14 it stays the pair it was seen in, its partner missing.
    let changes = tracker.update([&lone]);
    let [TargetChange::Updated(still)] = changes.as_slice() else {
        panic!("{changes:?}");
    };
    assert_eq!(still.id, target.id);
    assert_eq!(still.missing().collect::<Vec<_>>(), vec![&key(2)]);
    assert_eq!(tracker.target_of(&key(1)), Some(&target.id));
}

/// A pair member with its `_raop._tcp` record and without its
/// `_airplay._tcp` record, which carries the group keys.
fn raop_only(n: u8) -> Device {
    let record = ServiceRecord {
        kind: ServiceKind::Raop,
        instance: format!("02005E0053{n:02X}@HomePod {n}"),
        host: format!("speaker-{n}.local"),
        port: 7000,
        txt: ["et=0", "cn=0,1", "tp=UDP"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect(),
        addresses: vec![ScopedAddr::new([192, 0, 2, n].into())],
    };
    Device::from_records(key(n), Some(&record), None).expect("device")
}

// covers: P216, P181, P173
#[test]
fn a_pair_member_whose_partner_lapsed_is_named_missing() {
    let left = pair_member(1, "0x2004", TSID);
    let right = pair_member(2, "0x4", TSID);
    let nothing: [&Device; 0] = [];
    for left_now in [left.clone(), pair_member(1, "0x6004", TSID)] {
        let mut tracker = Targets::new();
        tracker.update([&left, &right]);
        let id = tracker.target_of(&key(1)).cloned().expect("the pair");

        let changes = tracker.update([&left_now]);
        let [TargetChange::Updated(pair)] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(pair.id, id);
        assert!(!pair.complete());
        assert_eq!(pair.missing().collect::<Vec<_>>(), vec![&key(2)]);
        assert_eq!(tracker.target_of(&key(1)), Some(&id));
        assert_eq!(tracker.target_of(&key(2)), Some(&id));

        // The remaining member's records lapse too: both are named missing.
        let changes = tracker.update(nothing);
        let [TargetChange::Updated(pair)] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(pair.id, id);
        assert_eq!(pair.name, "Kitchen");
        assert_eq!(pair.missing().collect::<Vec<_>>(), vec![&key(1), &key(2)]);
        assert!(pair.members.iter().all(|member| !member.in_view()));

        // Each comes back into the same pair.
        let changes = tracker.update([&left_now]);
        let [TargetChange::Updated(pair)] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(pair.missing().collect::<Vec<_>>(), vec![&key(2)]);
        tracker.update([&left, &right]);
        assert!(tracker.get(&id).expect("the pair").complete());
    }

    // The partner's `_airplay._tcp` record lapses before its `_raop._tcp`
    // record: it is still named missing, not dropped.
    let mut tracker = Targets::new();
    tracker.update([&left, &right]);
    let id = tracker.target_of(&key(1)).cloned().expect("the pair");
    let changes = tracker.update([&left, &raop_only(2)]);
    let [TargetChange::Updated(pair)] = changes.as_slice() else {
        panic!("{changes:?}");
    };
    assert_eq!(pair.id, id);
    assert_eq!(pair.missing().collect::<Vec<_>>(), vec![&key(2)]);
    tracker.update([&left]);
    assert_eq!(
        tracker
            .get(&id)
            .expect("the pair")
            .missing()
            .collect::<Vec<_>>(),
        vec![&key(2)]
    );

    // A device whose partner was never seen is a single speaker.
    let mut tracker = Targets::new();
    assert!(tracker.update([&left]).is_empty());
    assert_eq!(tracker.target_of(&key(1)), None);
}

fn apple_tv(n: u8) -> Device {
    device(
        n,
        "Living Room TV",
        &[
            "model=AppleTV11,1",
            "flags=0x4",
            "igl=1",
            &format!("gid={TV_GROUP}"),
            &format!("psgid={TV_GROUP}"),
            "psgtp=1",
            "psgsz=3",
        ],
    )
}

fn theater_speaker(n: u8, flags: &str) -> Device {
    device(
        n,
        &format!("HomePod {n}"),
        &[
            "model=AudioAccessory5,1",
            &format!("flags={flags}"),
            "igl=0",
            &format!("gid={OTHER_UUID}"),
            &format!("pgid={TV_GROUP}"),
            &format!("psgid={TV_GROUP}"),
            "pgcgl=1",
            "gpn=Living Room TV",
            &format!("tsid={TSID}"),
        ],
    )
}

// covers: P174, P176, P229
#[test]
fn an_apple_tv_with_homepods_is_one_home_theater_target() {
    // Bit 17 is set on both speakers and holds nothing back.
    let tv = apple_tv(9);
    let left = theater_speaker(3, "0x22004");
    let right = theater_speaker(4, "0x20004");
    let targets = find_targets([&tv, &left, &right]);
    assert_eq!(targets.len(), 1, "{targets:?}");
    let theater = &targets[0];
    assert_eq!(theater.id.kind, TargetKind::HomeTheater);
    assert_eq!(theater.id.key, TV_GROUP.to_ascii_lowercase());
    assert_eq!(theater.name, "Living Room TV");
    assert_eq!(theater.control, Some(key(9)));
    assert!(!theater.contains(&key(9)));
    let members: Vec<&DeviceKey> = theater.members.iter().map(|m| &m.key).collect();
    assert_eq!(members, vec![&key(3), &key(4)]);
    assert_eq!(theater.leader, Some(key(3)));
    assert_eq!(theater.expected_members, Some(2));
    assert!(theater.complete());

    // Without the Apple TV in view the speakers are still the group.
    let targets = find_targets([&left, &right]);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].id, theater.id);
    assert_eq!(targets[0].control, None);

    // Speakers that share a `pgid` but carry no `psgid` are not grouped.
    let loose = |n: u8| {
        device(
            n,
            &format!("Speaker {n}"),
            &["igl=0", &format!("pgid={TV_GROUP}"), "flags=0x4"],
        )
    };
    assert!(find_targets([&loose(5), &loose(6)]).is_empty());

    // One speaker short of `psgsz`: incomplete.
    let targets = find_targets([&tv, &left]);
    assert!(!targets[0].complete());
}

// covers: P175
#[test]
fn devices_sharing_only_a_group_key_stay_separate() {
    let a = device(
        7,
        "Den",
        &["igl=1", &format!("gid={OTHER_UUID}"), "flags=0x4"],
    );
    let b = device(
        8,
        "Hall",
        &["igl=0", &format!("gid={OTHER_UUID}"), "flags=0x4"],
    );
    assert!(find_targets([&a, &b]).is_empty());
    assert_eq!(a.group_keys().group_key(), b.group_keys().group_key());
    assert!(a.group_keys().group_key().is_some());
    let mut tracker = Targets::new();
    assert!(tracker.update([&a, &b]).is_empty());
    assert_eq!(tracker.target_of(&a.key), None);
}

fn single_speaker(n: u8) -> Device {
    device(
        n,
        &format!("HomePod {n}"),
        &["model=AudioAccessory5,1", "flags=0x4"],
    )
}

fn member_keys(tracker: &Targets, id: &TargetId) -> Vec<DeviceKey> {
    let target = tracker.get(id).expect("target");
    target.members.iter().map(|m| m.key.clone()).collect()
}

// covers: P176, P181
#[test]
fn a_former_member_in_view_on_its_own_is_never_named_missing() {
    let tv = apple_tv(9);
    let a_in_theater = device(
        1,
        "HomePod 1",
        &[
            "model=AudioAccessory5,1",
            "flags=0x4",
            "igl=0",
            &format!("gid={OTHER_UUID}"),
            &format!("pgid={TV_GROUP}"),
            &format!("psgid={TV_GROUP}"),
            "pgcgl=1",
            "gpn=Living Room TV",
        ],
    );
    let a_alone = single_speaker(1);
    let b1 = theater_speaker(3, "0x22004");
    let b2 = theater_speaker(4, "0x20004");
    let mut tracker = Targets::new();
    tracker.update([&tv, &a_in_theater]);
    let id = tracker
        .target_of(&key(1))
        .cloned()
        .expect("A plays for the TV");

    // The TV's sound moves to the pair B1 and B2; B1's TXT changes first.
    tracker.update([&tv, &a_alone, &b1, &single_speaker(4)]);
    assert_eq!(member_keys(&tracker, &id), vec![key(3)]);
    assert_eq!(tracker.target_of(&key(1)), None);
    tracker.update([&tv, &a_alone, &b1, &b2]);
    assert!(tracker.get(&id).expect("theater").complete());

    // B2 is unplugged and B1 reports its partner unreachable.
    let b1_unreachable = theater_speaker(3, "0x26004");
    tracker.update([&tv, &a_alone, &b1_unreachable]);
    let theater = tracker.get(&id).expect("theater");
    assert!(!theater.complete());
    assert_eq!(theater.missing().collect::<Vec<_>>(), vec![&key(4)]);
    assert_eq!(member_keys(&tracker, &id), vec![key(3), key(4)]);
    assert_eq!(tracker.target_of(&key(1)), None);
}

// covers: P176, P181
#[test]
fn a_missing_theater_member_is_named_until_it_is_in_view_without_the_group() {
    let tv = apple_tv(9);
    let b1 = theater_speaker(3, "0x22004");
    let b2 = theater_speaker(4, "0x20004");
    let mut tracker = Targets::new();
    tracker.update([&tv, &b1, &b2]);
    let id = tracker.target_of(&key(4)).cloned().expect("theater");

    let changes = tracker.update([&tv, &b1]);
    let [TargetChange::Updated(theater)] = changes.as_slice() else {
        panic!("{changes:?}");
    };
    assert!(!theater.complete());
    assert_eq!(theater.missing().collect::<Vec<_>>(), vec![&key(4)]);
    assert_eq!(tracker.target_of(&key(4)), Some(&id));

    // B2 comes back as a speaker of its own while B1 still sets bit 14.
    let b1_unreachable = theater_speaker(3, "0x26004");
    tracker.update([&tv, &b1_unreachable, &single_speaker(4)]);
    let theater = tracker.get(&id).expect("theater");
    assert!(!theater.complete());
    assert_eq!(theater.missing().count(), 0);
    assert_eq!(member_keys(&tracker, &id), vec![key(3)]);
    assert_eq!(tracker.target_of(&key(4)), None);

    // Out of view again, it is no longer taken for a member.
    tracker.update([&tv, &b1_unreachable]);
    assert_eq!(member_keys(&tracker, &id), vec![key(3)]);
    assert_eq!(tracker.target_of(&key(4)), None);
}

// covers: P181
#[test]
fn without_a_group_size_a_vanished_member_is_still_named() {
    let b1 = theater_speaker(3, "0x22004");
    let b2 = theater_speaker(4, "0x20004");
    let mut tracker = Targets::new();
    tracker.update([&b1, &b2]);
    let id = tracker.target_of(&key(3)).cloned().expect("theater");
    let theater = tracker.get(&id).expect("theater");
    assert_eq!(theater.expected_members, None);
    assert!(theater.complete());

    let changes = tracker.update([&b1]);
    let [TargetChange::Updated(theater)] = changes.as_slice() else {
        panic!("{changes:?}");
    };
    assert!(!theater.complete());
    assert_eq!(theater.missing().collect::<Vec<_>>(), vec![&key(4)]);
    assert_eq!(tracker.target_of(&key(4)), Some(&id));
}

struct Replay(Vec<BrowseEvent>);

impl Browser for Replay {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        (!self.0.is_empty()).then(|| self.0.remove(0))
    }
}

// covers: P176
#[test]
fn the_sender_reports_a_pair_as_one_target_after_its_members() {
    let (sender, _writer) = Sender::start(SenderConfig::default()).expect("sender");
    let left = record(
        2,
        "HomePod 2",
        &[
            "model=AudioAccessory5,1",
            "flags=0x2004",
            &format!("gid={TSID}+0+{OTHER_UUID}"),
            &format!("tsid={TSID}"),
            "gpn=Kitchen",
        ],
    );
    let right = record(
        1,
        "HomePod 1",
        &[
            "model=AudioAccessory5,1",
            "flags=0x4",
            &format!("gid={TSID}+1+{OTHER_UUID}"),
            &format!("tsid={TSID}"),
            "gpn=Kitchen",
        ],
    );
    sender.start_discovery_with(Box::new(Replay(vec![
        BrowseEvent::Resolved(left),
        BrowseEvent::Resolved(right),
    ])));
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Some(event) = sender.wait_event(Duration::from_millis(50)) {
            seen.push(event);
        }
        let targets = seen
            .iter()
            .filter(|e| matches!(e, SenderEvent::Receiver(ReceiverEvent::Target(_))))
            .count();
        if targets > 0 {
            break;
        }
    }
    let target_at = seen
        .iter()
        .position(|e| matches!(e, SenderEvent::Receiver(ReceiverEvent::Target(_))))
        .expect("a target event");
    let Some(SenderEvent::Receiver(ReceiverEvent::Target(TargetChange::Updated(pair)))) =
        seen.get(target_at)
    else {
        panic!("{seen:?}");
    };
    assert_eq!(pair.name, "Kitchen");
    assert_eq!(pair.leader, Some(key(2)));
    // Both members were listed before it, the first again once it joined
    // the pair, and each names the pair.
    let latest = |k: DeviceKey| {
        seen[..target_at]
            .iter()
            .rev()
            .find_map(|event| match event {
                SenderEvent::Receiver(
                    ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info),
                ) if info.device.key == k => Some(info.target.clone()),
                _ => None,
            })
    };
    assert_eq!(latest(key(1)), Some(Some(pair.id.clone())));
    assert_eq!(latest(key(2)), Some(Some(pair.id.clone())));
    sender.stop_discovery();
    let gone = (0..100).find_map(|_| match sender.wait_event(Duration::from_millis(50)) {
        Some(SenderEvent::Receiver(ReceiverEvent::Target(TargetChange::Gone(id)))) => Some(id),
        _ => None,
    });
    assert_eq!(gone, Some(pair.id.clone()));
}
