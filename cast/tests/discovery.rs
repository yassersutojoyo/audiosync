//! Discovery (section 2): records of `_googlecast._tcp` from the `mdns-sd`
//! crate's own types, injected through a channel, become devices and one
//! list keyed by `id`. No test browses the real network.

mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use audiosync_cast::discovery::mdns::{
    CONFIRM_WAIT, LocalNet, MdnsBrowser, browse_event, on_local_link,
};
use audiosync_cast::discovery::reach::{CHECK_INTERVAL, Reachability, connects};
use audiosync_cast::discovery::{
    Announcements, BrowseEvent, CastDevice, DEVICE_PORT, DeviceId, DeviceKey, DeviceKind,
    DeviceList, ListEvent, MAX_DEVICES, SERVICE_TYPE, ServiceRecord, Skipped, Txt,
};
use common::{Behavior, FakeDevice};
use mdns_sd::{ServiceEvent, ServiceInfo, TxtProperty};

const ID: &str = "3f2a8c41d27e4b8f9a06c1d5e7f80b12";

fn resolved(instance: &str, port: u16, addresses: &[IpAddr], txt: &[(&str, &str)]) -> ServiceEvent {
    let txt: Vec<TxtProperty> = txt.iter().map(TxtProperty::from).collect();
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        instance,
        "speaker.local.",
        addresses,
        port,
        txt,
    )
    .expect("a valid service");
    ServiceEvent::ServiceResolved(Box::new(info.as_resolved_service()))
}

fn v4(last: u8) -> IpAddr {
    IpAddr::from([192, 0, 2, last])
}

/// The link [`v4`] addresses are on, as this computer's own (C135).
fn test_net() -> Vec<LocalNet> {
    vec![LocalNet {
        address: v4(1),
        prefix: 24,
    }]
}

fn record(fullname: &str, port: u16, txt: &[&[u8]]) -> ServiceRecord {
    ServiceRecord {
        fullname: fullname.to_owned(),
        port,
        txt: txt.iter().map(|s| s.to_vec()).collect(),
        addresses: vec![v4(10)],
    }
}

// covers: C10, C15
#[test]
fn a_resolved_instance_becomes_a_device_with_its_announced_address_and_port() {
    assert_eq!(SERVICE_TYPE, "_googlecast._tcp.local.");
    let (send, receive) = mpsc::channel();
    let mut browser = MdnsBrowser::from_events(receive).with_local_nets(test_net());
    send.send(ServiceEvent::SearchStarted(SERVICE_TYPE.to_owned()))
        .expect("send");
    send.send(resolved(
        "Chromecast-3f2a",
        8009,
        &[v4(10)],
        &[
            ("id", ID),
            ("fn", "Living Room"),
            ("md", "Chromecast Audio"),
        ],
    ))
    .expect("send");
    let Some(BrowseEvent::Resolved(record)) = browser.poll_event() else {
        panic!("a resolved record");
    };
    assert!(browser.poll_event().is_none());
    let device = CastDevice::from_record(&record).expect("a device");
    assert_eq!(device.name.as_deref(), Some("Living Room"));
    assert_eq!(device.kind, DeviceKind::Single);
    assert_eq!(
        device.socket_addresses(),
        [SocketAddr::new(v4(10), 8009)],
        "the address and port from the records"
    );
    drop(send);
    assert!(browser.poll_event().is_none());
    assert!(browser.is_dead());

    // Another service type is not a Cast device.
    let other = ServiceInfo::new(
        "_http._tcp.local.",
        "x",
        "speaker.local.",
        &[v4(1)][..],
        80,
        None,
    )
    .expect("service");
    assert_eq!(
        browse_event(ServiceEvent::ServiceResolved(Box::new(
            other.as_resolved_service()
        ))),
        None
    );
}

// covers: C11
#[test]
fn instances_under_a_subtype_are_ignored() {
    let sub = record(
        "_3f2a8c41._sub._googlecast._tcp.local.",
        8009,
        &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
    );
    assert_eq!(CastDevice::from_record(&sub), Err(Skipped::Subtype));
    let device = record(
        "Speaker._googlecast._tcp.local.",
        8009,
        &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
    );
    assert!(CastDevice::from_record(&device).is_ok());
}

// covers: C12
#[test]
fn txt_strings_are_key_value_pairs_read_as_raw_bytes() {
    let txt = Txt::parse(&[
        b"id=abc".to_vec(),
        b"flag".to_vec(),
        b"fn=K\xc3\xbcche \\195".to_vec(),
        b"FN=second".to_vec(),
        b"empty=".to_vec(),
        b"zz=unknown".to_vec(),
    ]);
    assert_eq!(txt.get("id"), Some(&b"abc"[..]));
    assert_eq!(txt.get("flag"), None, "a string without = is skipped");
    assert_eq!(
        txt.text("fn").as_deref(),
        Some("K\u{fc}che \\195"),
        "UTF-8 as received; a backslash stays a backslash"
    );
    assert_eq!(txt.get("empty"), Some(&b""[..]));
    assert!(txt.keys().any(|key| key == "zz"), "unknown keys are kept");

    // Through the mDNS adapter the bytes arrive unescaped.
    let event = resolved(
        "Kitchen",
        8009,
        &[v4(11)],
        &[("id", ID), ("fn", "K\u{fc}che")],
    );
    let Some(BrowseEvent::Resolved(record)) = browse_event(event) else {
        panic!("resolved");
    };
    let device = CastDevice::from_record(&record).expect("device");
    assert_eq!(device.name.as_deref(), Some("K\u{fc}che"));
}

// covers: C13
#[test]
fn the_keys_the_sender_reads_and_an_id_that_must_be_a_uuid() {
    let full = record(
        "Speaker._googlecast._tcp.local.",
        8009,
        &[
            b"id=3F2A8C41-D27E-4B8F-9A06-C1D5E7F80B12",
            b"fn=Office",
            b"md=Chromecast",
            b"ve=05",
            b"ic=/setup/icon.png",
            b"ca=4100",
            b"rs=",
            b"st=0",
        ],
    );
    let device = CastDevice::from_record(&full).expect("device");
    assert_eq!(device.id, DeviceId::parse(ID).expect("id"));
    assert_eq!(device.id.to_string(), ID);
    assert_eq!(
        format!("{:?}", ListEvent::Added(DeviceKey::Id(device.id))),
        format!("Added(Id({ID}))")
    );
    assert_eq!(device.name.as_deref(), Some("Office"));
    assert_eq!(device.model.as_deref(), Some("Chromecast"));
    assert_eq!(device.version.as_deref(), Some("05"));
    assert_eq!(device.icon_path.as_deref(), Some("/setup/icon.png"));
    assert_eq!(device.other_keys, ["ca", "rs", "st"]);

    // `ca` changes nothing.
    let mut without_ca = full.clone();
    without_ca.txt.retain(|s| !s.starts_with(b"ca="));
    let mut other_ca = full.clone();
    other_ca.txt.push(b"ca=5".to_vec());
    other_ca.txt.retain(|s| s != b"ca=4100");
    for variant in [without_ca, other_ca] {
        let other = CastDevice::from_record(&variant).expect("device");
        assert_eq!(other.kind, device.kind);
        assert_eq!(other.socket_addresses(), device.socket_addresses());
    }

    // A missing model is not an error.
    let bare = record(
        "Bare._googlecast._tcp.local.",
        8009,
        &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
    );
    assert_eq!(CastDevice::from_record(&bare).expect("device").model, None);

    for bad in [
        &b"fn=No id"[..],
        b"id=",
        b"id=not-a-uuid",
        b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b1",
        b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12ff",
        b"id=3f2a8c41-d27e4b8f-9a06-c1d5e7f80b12",
        b"id=zf2a8c41d27e4b8f9a06c1d5e7f80b12",
    ] {
        let broken = record("Emulator._googlecast._tcp.local.", 8009, &[bad]);
        assert_eq!(
            CastDevice::from_record(&broken),
            Err(Skipped::BadId),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

// covers: C14
#[test]
fn a_port_other_than_8009_marks_a_group() {
    assert_eq!(DEVICE_PORT, 8009);
    let group = record(
        "Group._googlecast._tcp.local.",
        32187,
        &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12", b"fn=Downstairs"],
    );
    let device = CastDevice::from_record(&group).expect("listed");
    assert_eq!(device.kind, DeviceKind::Group);
    let mut list = DeviceList::new();
    assert!(matches!(
        list.apply(BrowseEvent::Resolved(group)),
        ListEvent::Added(_)
    ));
    assert_eq!(list.entries().len(), 1, "a group is listed");
}

// covers: C16
#[test]
fn one_id_on_several_interfaces_is_one_device_with_ipv4_first() {
    let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x10));
    let mut list = DeviceList::new();
    let first = ServiceRecord {
        addresses: vec![v6, v4(20)],
        ..record(
            "A._googlecast._tcp.local.",
            8009,
            &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
        )
    };
    assert!(matches!(
        list.apply(BrowseEvent::Resolved(first)),
        ListEvent::Added(_)
    ));
    let key = DeviceKey::Id(DeviceId::parse(ID).expect("id"));
    let entry = list.get(key).expect("listed");
    assert_eq!(
        entry.addresses,
        [SocketAddr::new(v4(20), 8009), SocketAddr::new(v6, 8009)]
    );
    let second = ServiceRecord {
        addresses: vec![v4(21), v4(20), v6],
        ..record(
            "A._googlecast._tcp.local.",
            8009,
            &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
        )
    };
    list.remember_address(key, SocketAddr::new(v4(20), 8009));
    assert_eq!(
        list.apply(BrowseEvent::Resolved(second)),
        ListEvent::Updated(key)
    );
    assert_eq!(list.entries().len(), 1);
    let entry = list.get(key).expect("listed");
    assert_eq!(
        entry.addresses,
        [
            SocketAddr::new(v4(20), 8009),
            SocketAddr::new(v4(21), 8009),
            SocketAddr::new(v6, 8009)
        ],
        "the address that worked stays first"
    );
    list.remember_address(key, SocketAddr::new(v4(21), 8009));

    // A link-local IPv6 address has no scope here, so it is never offered.
    let link_local = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x10));
    let scoped_away = ServiceRecord {
        addresses: vec![link_local, v4(22)],
        ..record(
            "B._googlecast._tcp.local.",
            8009,
            &[b"id=00000000000000000000000000000001"],
        )
    };
    let device = CastDevice::from_record(&scoped_away).expect("device");
    assert_eq!(device.addresses, [v4(22)]);
    assert_eq!(
        list.get(key).expect("listed").addresses[0],
        SocketAddr::new(v4(21), 8009)
    );
}

// covers: C17
#[test]
fn a_goodbye_during_a_session_keeps_the_device_listed_as_offline() {
    let mut list = DeviceList::new();
    let name = "A._googlecast._tcp.local.";
    let device = record(name, 8009, &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"]);
    list.apply(BrowseEvent::Resolved(device.clone()));
    let key = DeviceKey::Id(DeviceId::parse(ID).expect("id"));
    list.set_in_session(key, true);
    let goodbye = browse_event(ServiceEvent::ServiceRemoved(
        SERVICE_TYPE.to_owned(),
        name.to_owned(),
    ))
    .expect("a removal");
    assert_eq!(list.apply(goodbye.clone()), ListEvent::Offline(key));
    let entry = list.get(key).expect("still listed");
    assert!(!entry.online);
    list.set_in_session(key, false);
    assert!(list.get(key).is_none(), "gone once the session ends");

    list.apply(BrowseEvent::Resolved(device));
    assert_eq!(list.apply(goodbye), ListEvent::Removed(key));
    assert!(list.entries().is_empty());
}

// covers: C134
#[test]
fn a_goodbye_is_not_reported_while_the_device_answers_again() {
    let (send, receive) = mpsc::channel();
    let mut browser = MdnsBrowser::from_events(receive).with_local_nets(test_net());
    let device = || resolved("Chromecast-3f2a", 8009, &[v4(10)], &[("id", ID)]);
    send.send(device()).expect("send");
    let Some(BrowseEvent::Resolved(record)) = browser.poll_event() else {
        panic!("a resolved record");
    };
    send.send(ServiceEvent::ServiceRemoved(
        SERVICE_TYPE.to_owned(),
        record.fullname.clone(),
    ))
    .expect("send");
    assert_eq!(
        browser.poll_event(),
        None,
        "a goodbye alone removes nothing"
    );
    send.send(device()).expect("send");
    assert!(matches!(
        browser.poll_event(),
        Some(BrowseEvent::Resolved(_))
    ));
    assert_eq!(browser.poll_event(), None);
}

// covers: C134
#[test]
fn an_unanswered_goodbye_is_reported_after_the_wait_and_held_goodbyes_are_bounded() {
    let (send, receive) = mpsc::channel();
    let mut browser = MdnsBrowser::from_events(receive);
    let goodbye =
        |name: &str| ServiceEvent::ServiceRemoved(SERVICE_TYPE.to_owned(), name.to_owned());
    let start = Instant::now();
    let fullname = format!("Chromecast-3f2a.{SERVICE_TYPE}");
    send.send(goodbye(&fullname)).expect("send");
    send.send(goodbye(&fullname)).expect("send");
    assert_eq!(browser.poll_event_at(start), None);
    let almost = start + CONFIRM_WAIT - Duration::from_millis(1);
    assert_eq!(browser.poll_event_at(almost), None);
    let due = start + CONFIRM_WAIT;
    assert_eq!(
        browser.poll_event_at(due),
        Some(BrowseEvent::Removed { fullname })
    );
    assert_eq!(browser.poll_event_at(due), None, "reported once");

    for n in 0..MAX_DEVICES + 50 {
        send.send(goodbye(&format!("D{n}.{SERVICE_TYPE}")))
            .expect("send");
    }
    assert_eq!(browser.poll_event_at(due), None);
    let mut reported = 0;
    while browser.poll_event_at(due + CONFIRM_WAIT).is_some() {
        reported += 1;
    }
    assert_eq!(reported, MAX_DEVICES);
}

// covers: C135
#[test]
fn a_record_at_loopback_multicast_or_an_off_link_public_address_lists_nothing() {
    let (send, receive) = mpsc::channel();
    let mut browser = MdnsBrowser::from_events(receive);
    for address in [
        IpAddr::from([127, 0, 0, 1]),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::from([224, 0, 0, 251]),
        IpAddr::from([203, 0, 113, 7]),
    ] {
        send.send(resolved("Chromecast-3f2a", 8009, &[address], &[("id", ID)]))
            .expect("send");
        assert_eq!(browser.poll_event(), None, "{address}");
    }
}

// covers: C135
#[test]
fn only_addresses_on_our_links_or_private_routed_ones_are_kept() {
    let v6 = |segments: [u16; 8]| IpAddr::V6(Ipv6Addr::from(segments));
    let own = IpAddr::from([192, 168, 1, 20]);
    let net = |address, prefix| LocalNet { address, prefix };
    let nets = [
        net(own, 24),
        net(v6([0xfe80, 0, 0, 0, 0, 0, 0, 1]), 64),
        net(v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 5]), 64),
        net(v6([0x2001, 0, 0xabcd, 0, 0, 0, 0, 1]), 32),
    ];
    for address in [
        IpAddr::from([192, 168, 1, 30]),
        IpAddr::from([10, 9, 8, 7]),
        v6([0xfd00, 0, 0, 0, 0, 0, 0, 5]),
        v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 9]),
        v6([0xfe80, 0, 0, 0, 0, 0, 0, 9]),
    ] {
        assert!(on_local_link(address, &nets), "{address} is kept");
    }
    for address in [
        IpAddr::from([127, 0, 0, 1]),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::from([0, 0, 0, 0]),
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        IpAddr::from([224, 0, 0, 251]),
        v6([0xff02, 0, 0, 0, 0, 0, 0, 0xfb]),
        IpAddr::from([255, 255, 255, 255]),
        IpAddr::from([192, 168, 1, 255]),
        own,
        IpAddr::V6(Ipv4Addr::from([192, 168, 1, 20]).to_ipv6_mapped()),
        IpAddr::from([203, 0, 113, 7]),
        v6([0x2001, 0, 0xabcd, 0, 0, 0, 0, 9]),
        v6([0x2002, 0xcb00, 0x7107, 0, 0, 0, 0, 1]),
        IpAddr::from([169, 254, 3, 4]),
    ] {
        assert!(!on_local_link(address, &nets), "{address} is dropped");
    }
    let link_local_v4 = [net(IpAddr::from([169, 254, 1, 1]), 16)];
    assert!(on_local_link(
        IpAddr::from([169, 254, 3, 4]),
        &link_local_v4
    ));

    let (send, receive) = mpsc::channel();
    let mut browser = MdnsBrowser::from_events(receive).with_local_nets(nets.to_vec());
    let kept = IpAddr::from([192, 168, 1, 30]);
    let mixed = [IpAddr::from([127, 0, 0, 1]), kept];
    send.send(resolved("Chromecast-3f2a", 8009, &mixed, &[("id", ID)]))
        .expect("send");
    let Some(BrowseEvent::Resolved(record)) = browser.poll_event() else {
        panic!("a resolved record");
    };
    assert_eq!(record.addresses, [kept]);
}

// covers: C17, C81, C90
#[test]
fn only_a_device_found_back_or_at_other_addresses_is_announced() {
    let mut list = DeviceList::new();
    let mut announcements = Announcements::default();
    let id = DeviceId::parse(ID).expect("id");
    let name = "A._googlecast._tcp.local.";
    let mut device = record(name, 8009, &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"]);
    let mut apply = |list: &mut DeviceList, event| {
        let change = list.apply(event);
        announcements.note(list, &change)
    };
    assert_eq!(
        apply(&mut list, BrowseEvent::Resolved(device.clone())),
        Some(id)
    );
    assert_eq!(
        apply(&mut list, BrowseEvent::Resolved(device.clone())),
        None
    );
    device.txt.push(b"st=1".to_vec());
    assert_eq!(
        apply(&mut list, BrowseEvent::Resolved(device.clone())),
        None,
        "a TXT change"
    );

    list.set_in_session(DeviceKey::Id(id), true);
    let goodbye = BrowseEvent::Removed {
        fullname: name.to_owned(),
    };
    assert_eq!(apply(&mut list, goodbye), None);
    assert_eq!(
        apply(&mut list, BrowseEvent::Resolved(device.clone())),
        Some(id),
        "back"
    );
    device.addresses = vec![v4(11)];
    assert_eq!(
        apply(&mut list, BrowseEvent::Resolved(device)),
        Some(id),
        "other addresses"
    );
}

fn numbered(n: u32) -> (DeviceKey, BrowseEvent) {
    let id = format!("{n:032x}");
    let key = DeviceKey::Id(DeviceId::parse(&id).expect("id"));
    let record = ServiceRecord {
        fullname: format!("D{n}._googlecast._tcp.local."),
        port: DEVICE_PORT,
        txt: vec![format!("id={id}").into_bytes()],
        addresses: vec![v4(10)],
    };
    (key, BrowseEvent::Resolved(record))
}

// covers: C108
#[test]
fn a_flood_of_devices_never_displaces_a_listed_device() {
    let mut list = DeviceList::new();
    let manual = SocketAddr::from(([192, 0, 2, 200], DEVICE_PORT));
    list.add_manual(manual);
    let (playing, event) = numbered(1);
    list.apply(event);
    list.set_in_session(playing, true);
    let last = 10_002;
    for n in 2..=last {
        let (key, event) = numbered(n);
        let change = list.apply(event);
        if (n as usize) < MAX_DEVICES {
            assert_eq!(change, ListEvent::Added(key));
        } else {
            assert_eq!(change, ListEvent::Skipped(Skipped::ListFull), "device {n}");
        }
    }
    assert_eq!(list.entries().len(), MAX_DEVICES);
    for key in [playing, DeviceKey::Manual(manual), numbered(2).0] {
        assert!(list.get(key).is_some(), "{key:?} stays listed");
    }
    assert!(list.get(numbered(last).0).is_none(), "a new one is ignored");
    let (listed, mut again) = numbered(2);
    if let BrowseEvent::Resolved(record) = &mut again {
        record.addresses = vec![v4(11)];
    }
    assert_eq!(
        list.apply(again),
        ListEvent::Updated(listed),
        "a listed device still updates"
    );
}

// covers: C18
#[test]
fn a_manual_address_works_without_txt_until_a_browse_finds_its_id() {
    let mut list = DeviceList::new();
    let address = SocketAddr::new(v4(10), 8009);
    assert_eq!(
        list.add_manual(address),
        ListEvent::Added(DeviceKey::Manual(address))
    );
    let entry = list.get(DeviceKey::Manual(address)).expect("listed");
    assert_eq!(entry.device, None);
    assert_eq!(entry.addresses, [address]);
    let found = record(
        "A._googlecast._tcp.local.",
        8009,
        &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
    );
    let id = DeviceId::parse(ID).expect("id");
    assert_eq!(
        list.apply(BrowseEvent::Resolved(found)),
        ListEvent::Identified { from: address, id }
    );
    assert!(list.get(DeviceKey::Manual(address)).is_none());
    assert_eq!(list.entries().len(), 1);
    assert!(list.get(DeviceKey::Id(id)).is_some());
    assert_eq!(list.remove_manual(address), None, "found, it stays");
    assert_eq!(list.entries().len(), 1);
}

// covers: C18
// No record vouches for a device added by address, so it is listed
// offline until a check reaches it.
#[test]
fn a_manual_address_is_listed_offline_until_a_check_reaches_it() {
    let mut list = DeviceList::new();
    let address = SocketAddr::new(v4(10), 8009);
    list.add_manual(address);
    let entry = list.get(DeviceKey::Manual(address)).expect("listed");
    assert!(!entry.online, "nothing has reached it yet");

    let key = DeviceKey::Manual(address);
    assert_eq!(
        list.set_reachable(address, true),
        Some(ListEvent::Updated(key))
    );
    assert!(list.get(key).expect("listed").online);
    assert_eq!(list.set_reachable(address, true), None, "no change");
    assert_eq!(
        list.set_reachable(address, false),
        Some(ListEvent::Updated(key))
    );
    assert!(!list.get(key).expect("listed").online);
    let other = SocketAddr::new(v4(11), 8009);
    assert_eq!(list.set_reachable(other, true), None, "not listed");
}

// covers: C17, C18
// A session's own health decides while it runs, and a device added by
// address stays listed when its session ends.
#[test]
fn a_session_holds_the_reachability_of_a_manual_address() {
    let mut list = DeviceList::new();
    let address = SocketAddr::new(v4(10), 8009);
    let key = DeviceKey::Manual(address);
    list.add_manual(address);
    list.set_in_session(key, true);
    assert_eq!(list.set_reachable(address, true), None);
    list.set_in_session(key, false);
    let entry = list.get(key).expect("still listed");
    assert!(!entry.online);
}

/// Polls `reach` until a check changed `list`, for checks that answer on
/// their own threads.
fn poll_until_changed(reach: &mut Reachability, list: &mut DeviceList) -> Vec<ListEvent> {
    for _ in 0..400 {
        let events = reach.poll(list, Instant::now());
        if !events.is_empty() {
            return events;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no check changed the list");
}

static STALE_PROBES: AtomicUsize = AtomicUsize::new(0);
static STALE_ANSWER: AtomicBool = AtomicBool::new(true);

// covers: C17, C18
// A check that started before a session opened and ended says nothing
// about the device now; it is dropped, and a fresh one runs at once.
#[test]
fn a_check_older_than_the_last_session_is_dropped_and_run_again() {
    fn answers(_: SocketAddr) -> bool {
        let reached = STALE_ANSWER.load(Ordering::SeqCst);
        STALE_PROBES.fetch_add(1, Ordering::SeqCst);
        reached
    }
    let mut list = DeviceList::new();
    let address = SocketAddr::new(v4(10), 8009);
    let key = DeviceKey::Manual(address);
    list.add_manual(address);
    let mut reach = Reachability::new(answers);
    let now = Instant::now();
    assert!(reach.poll(&mut list, now).is_empty());
    for _ in 0..400 {
        if STALE_PROBES.load(Ordering::SeqCst) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        STALE_PROBES.load(Ordering::SeqCst),
        1,
        "the first check ran"
    );

    // A short session to a dead device opens and ends while that check
    // reports a device that answered.
    list.set_in_session(key, true);
    list.set_in_session(key, false);
    STALE_ANSWER.store(false, Ordering::SeqCst);
    for _ in 0..400 {
        reach.poll(&mut list, now);
        assert!(
            !list.get(key).expect("listed").online,
            "the stale answer is dropped"
        );
        if STALE_PROBES.load(Ordering::SeqCst) == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        STALE_PROBES.load(Ordering::SeqCst),
        2,
        "checked again at once, not after the interval"
    );
    for _ in 0..20 {
        reach.poll(&mut list, now);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!list.get(key).expect("listed").online);
}

static PROBES: AtomicUsize = AtomicUsize::new(0);

// covers: C18
// A device added by address is checked at once and again on the
// interval, and goes offline when a check fails.
#[test]
fn a_manual_address_is_checked_at_once_and_again_on_the_interval() {
    fn answers(address: SocketAddr) -> bool {
        PROBES.fetch_add(1, Ordering::SeqCst);
        address.port() == 8009
    }
    let mut list = DeviceList::new();
    let on = SocketAddr::new(v4(10), 8009);
    let off = SocketAddr::new(v4(12), 8010);
    list.add_manual(on);
    list.add_manual(off);
    let mut reach = Reachability::new(answers);
    assert_eq!(
        poll_until_changed(&mut reach, &mut list),
        [ListEvent::Updated(DeviceKey::Manual(on))]
    );
    assert!(list.get(DeviceKey::Manual(on)).expect("listed").online);
    assert!(!list.get(DeviceKey::Manual(off)).expect("listed").online);
    for _ in 0..40 {
        reach.poll(&mut list, Instant::now());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        PROBES.load(Ordering::SeqCst),
        2,
        "nothing more before the interval"
    );

    let later = Instant::now() + CHECK_INTERVAL + Duration::from_secs(1);
    reach.poll(&mut list, later);
    for _ in 0..400 {
        if PROBES.load(Ordering::SeqCst) == 4 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(PROBES.load(Ordering::SeqCst), 4, "both checked again");

    // The device stops answering.
    fn silent(_: SocketAddr) -> bool {
        false
    }
    let mut reach = Reachability::new(silent);
    assert_eq!(
        poll_until_changed(&mut reach, &mut list),
        [ListEvent::Updated(DeviceKey::Manual(on))]
    );
    assert!(!list.get(DeviceKey::Manual(on)).expect("listed").online);
}

// covers: C18
// A check whose probe panics (a bug) counts as one that did not reach the
// device, instead of leaving the address checked forever.
#[test]
fn a_check_that_panics_reports_the_device_unreached() {
    fn panics(_: SocketAddr) -> bool {
        panic!("a bug in a check (test)");
    }
    let mut list = DeviceList::new();
    let address = SocketAddr::new(v4(13), 8009);
    let key = DeviceKey::Manual(address);
    list.add_manual(address);
    list.set_reachable(address, true);
    let mut reach = Reachability::new(panics);
    assert_eq!(
        poll_until_changed(&mut reach, &mut list),
        [ListEvent::Updated(key)]
    );
    assert!(!list.get(key).expect("listed").online);
}

// A proxy or filter on the path may take the TCP connect with no device
// behind it (a Mac reached 192.0.2.1 that way, 2026-10-28).
// covers: C18
#[test]
fn the_live_check_reaches_a_device_that_completes_the_tls_handshake_only() {
    let device = FakeDevice::start(Behavior::default());
    assert!(connects(device.address));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
    let silent = listener.local_addr().expect("its address");
    assert!(!connects(silent), "a TCP accept alone is no answer");
    let closed = {
        let spare = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        spare.local_addr().expect("its address")
    };
    assert!(!connects(closed));
}

// covers: C18
#[test]
fn a_manual_address_no_browse_found_can_be_taken_back() {
    let mut list = DeviceList::new();
    let address = SocketAddr::new(v4(10), 8009);
    list.add_manual(address);
    assert_eq!(
        list.add_manual(address),
        ListEvent::Updated(DeviceKey::Manual(address)),
        "listed once"
    );
    assert_eq!(
        list.remove_manual(address),
        Some(ListEvent::Removed(DeviceKey::Manual(address)))
    );
    assert!(list.entries().is_empty());
    assert_eq!(list.remove_manual(address), None);
}

// covers: C16, C18
#[test]
fn adding_the_address_of_a_listed_device_names_that_device() {
    let mut list = DeviceList::new();
    let found = record(
        "A._googlecast._tcp.local.",
        8009,
        &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
    );
    list.apply(BrowseEvent::Resolved(found));
    let id = DeviceId::parse(ID).expect("id");
    let address = SocketAddr::new(v4(10), 8009);
    assert_eq!(
        list.add_manual(address),
        ListEvent::Updated(DeviceKey::Id(id)),
        "the listed device, not a second row"
    );
    assert_eq!(list.entries().len(), 1);
    let other_port = SocketAddr::new(v4(10), 8010);
    assert_eq!(
        list.add_manual(other_port),
        ListEvent::Added(DeviceKey::Manual(other_port)),
        "another port is another device"
    );
}

// Partly C16 and C18: a manual address that a found device already has
// stays that device's one row.
// covers: C16
#[test]
fn a_manual_address_of_a_listed_device_names_that_device() {
    let mut list = DeviceList::new();
    let found = record(
        "A._googlecast._tcp.local.",
        8009,
        &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
    );
    list.apply(BrowseEvent::Resolved(found));
    let id = DeviceId::parse(ID).expect("id");
    let address = SocketAddr::new(v4(10), 8009);
    assert_eq!(
        list.add_manual(address),
        ListEvent::Updated(DeviceKey::Id(id))
    );
    assert_eq!(list.entries().len(), 1);
    assert!(list.get(DeviceKey::Manual(address)).is_none());

    let other = SocketAddr::new(v4(10), 8010);
    assert_eq!(
        list.add_manual(other),
        ListEvent::Added(DeviceKey::Manual(other))
    );
    assert_eq!(list.entries().len(), 2);
}

// Attacks A and B of the 2026-11-04 security review: a record from another
// instance names a listed device's `id` with an address of its own.
// covers: C116
#[test]
fn a_record_from_another_instance_never_moves_a_listed_device() {
    let mut list = DeviceList::new();
    let key = DeviceKey::Id(DeviceId::parse(ID).expect("id"));
    let own = record(
        "A._googlecast._tcp.local.",
        8009,
        &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
    );
    list.apply(BrowseEvent::Resolved(own));
    for addresses in [vec![v4(66)], vec![v4(10), v4(66)]] {
        let posing = ServiceRecord {
            addresses,
            ..record(
                "Kitchen._googlecast._tcp.local.",
                8009,
                &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
            )
        };
        list.apply(BrowseEvent::Resolved(posing));
        let entry = list.get(key).expect("listed");
        assert_eq!(entry.addresses, [SocketAddr::new(v4(10), 8009)]);
        assert_eq!(
            entry.device.as_ref().map(|device| device.instance.as_str()),
            Some("A._googlecast._tcp.local.")
        );
    }
    assert_eq!(list.entries().len(), 1);

    let moved = ServiceRecord {
        addresses: vec![v4(11)],
        ..record(
            "a._googlecast._tcp.local.",
            8009,
            &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
        )
    };
    assert_eq!(
        list.apply(BrowseEvent::Resolved(moved)),
        ListEvent::Updated(key)
    );
    assert_eq!(
        list.get(key).expect("listed").addresses,
        [SocketAddr::new(v4(11), 8009)],
        "its own instance moves it"
    );
}

// covers: C116
#[test]
fn a_record_naming_a_manual_address_adds_none_of_its_other_addresses() {
    let mut list = DeviceList::new();
    let typed = SocketAddr::new(v4(10), 8009);
    list.add_manual(typed);
    let posing = ServiceRecord {
        addresses: vec![v4(10), v4(66)],
        ..record(
            "A._googlecast._tcp.local.",
            8009,
            &[b"id=3f2a8c41d27e4b8f9a06c1d5e7f80b12"],
        )
    };
    list.apply(BrowseEvent::Resolved(posing.clone()));
    assert_eq!(list.entries().len(), 1);
    assert_eq!(list.entries()[0].addresses, [typed]);
    let moved = ServiceRecord {
        addresses: vec![v4(66)],
        ..posing
    };
    list.apply(BrowseEvent::Resolved(moved));
    assert_eq!(list.entries().len(), 1);
    assert_eq!(list.entries()[0].addresses, [typed], "nor a later record");
}
