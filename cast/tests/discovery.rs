//! Discovery (section 2): records of `_googlecast._tcp` from the `mdns-sd`
//! crate's own types, injected through a channel, become devices and one
//! list keyed by `id`. No test browses the real network.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::mpsc;

use audiosync_cast::discovery::mdns::{MdnsBrowser, browse_event};
use audiosync_cast::discovery::{
    BrowseEvent, CastDevice, DEVICE_PORT, DeviceId, DeviceKey, DeviceKind, DeviceList, ListEvent,
    SERVICE_TYPE, ServiceRecord, Skipped, Txt,
};
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
    let mut browser = MdnsBrowser::from_events(receive);
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

// Partly C18: no record vouches for a device added by address, so it is
// listed offline until a check reaches it.
#[test]
fn a_manual_address_is_listed_offline_until_a_check_reaches_it() {
    let mut list = DeviceList::new();
    let address = SocketAddr::new(v4(10), 8009);
    list.add_manual(address);
    let entry = list.get(DeviceKey::Manual(address)).expect("listed");
    assert!(!entry.online, "nothing has reached it yet");
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
