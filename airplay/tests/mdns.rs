//! The live browser's adapter (P7): `mdns-sd` events, built with the
//! crate's own public types and injected through a channel, become browse
//! events and devices. One ignored test browses the real network; it never
//! runs in CI.

use std::net::{IpAddr, Ipv6Addr};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::mdns::{MdnsBrowser, browse_event, instance_name};
use audiosync_airplay::discovery::{
    BrowseEvent, Browser, DeviceId, DeviceKey, Directory, DirectoryEvent, ScopedAddr, ServiceKind,
};
use mdns_sd::{ServiceEvent, ServiceInfo, TxtProperty};

const RAOP_TYPE: &str = "_raop._tcp.local.";
const AIRPLAY_TYPE: &str = "_airplay._tcp.local.";
const ID: DeviceId = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 0x01]);

fn resolved(
    service_type: &str,
    instance: &str,
    addresses: &[IpAddr],
    txt: Vec<TxtProperty>,
) -> ServiceEvent {
    let info = ServiceInfo::new(
        service_type,
        instance,
        "kitchen.local.",
        addresses,
        7000,
        txt,
    )
    .expect("a valid service");
    ServiceEvent::ServiceResolved(Box::new(info.as_resolved_service()))
}

fn props(pairs: &[(&str, &str)]) -> Vec<TxtProperty> {
    pairs.iter().map(TxtProperty::from).collect()
}

fn raop_instance() -> String {
    format!("{ID}@Kitchen")
}

// covers: P7
#[test]
fn resolved_services_of_both_types_become_one_device() {
    let v4 = IpAddr::from([192, 0, 2, 30]);
    let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x30));
    let mut raop_txt = props(&[
        ("txtvers", "1"),
        ("cn", "0,1"),
        ("et", "0"),
        ("md", "0,1,2"),
    ]);
    // A boolean key without a value (P11).
    raop_txt.push(TxtProperty::from("da"));
    let raop = resolved(RAOP_TYPE, &raop_instance(), &[v6, v4], raop_txt);
    let airplay = resolved(
        AIRPLAY_TYPE,
        "Kitchen",
        &[v4],
        props(&[("deviceid", "02:00:5E:00:53:01"), ("features", "0x200,0x0")]),
    );

    let Some(BrowseEvent::Resolved(record)) = browse_event(raop.clone()) else {
        panic!("the RAOP service resolves");
    };
    assert_eq!(record.kind, ServiceKind::Raop);
    assert_eq!(record.instance, raop_instance());
    assert_eq!(record.host, "kitchen.local.");
    assert_eq!(record.port, 7000);
    assert_eq!(
        record.txt,
        vec![
            b"txtvers=1".to_vec(),
            b"cn=0,1".to_vec(),
            b"et=0".to_vec(),
            b"md=0,1,2".to_vec(),
            b"da".to_vec(),
        ]
    );
    assert_eq!(
        record.addresses,
        vec![ScopedAddr::new(v4), ScopedAddr::new(v6)],
        "addresses in a fixed order"
    );

    let (events, browser_events) = mpsc::channel();
    let mut browser = MdnsBrowser::from_events(browser_events);
    events
        .send(ServiceEvent::SearchStarted(RAOP_TYPE.to_owned()))
        .expect("send");
    events.send(raop).expect("send");
    events.send(airplay).expect("send");
    let mut directory = Directory::new();
    let mut last = None;
    while let Some(event) = browser.poll_event() {
        for change in directory.apply(event) {
            last = Some(change);
        }
    }
    let Some(DirectoryEvent::Updated(device)) = last else {
        panic!("a device");
    };
    assert_eq!(device.key, DeviceKey::Id(ID));
    assert_eq!(device.name, "Kitchen");
    assert!(device.raop.is_some() && device.airplay.is_some());
    assert_eq!(directory.devices().count(), 1);

    let fullname = format!("{}.{RAOP_TYPE}", raop_instance());
    events
        .send(ServiceEvent::ServiceRemoved(RAOP_TYPE.to_owned(), fullname))
        .expect("send");
    assert_eq!(
        browser.poll_event(),
        Some(BrowseEvent::Removed {
            kind: ServiceKind::Raop,
            instance: raop_instance(),
        })
    );
    assert_eq!(browser.poll_event(), None);
}

#[test]
fn other_service_types_notices_and_services_without_addresses_are_skipped() {
    let http = resolved(
        "_http._tcp.local.",
        "Printer",
        &[IpAddr::from([192, 0, 2, 31])],
        Vec::new(),
    );
    assert_eq!(browse_event(http), None);
    assert_eq!(
        browse_event(ServiceEvent::SearchStarted(RAOP_TYPE.to_owned())),
        None
    );
    assert_eq!(
        browse_event(ServiceEvent::ServiceFound(
            RAOP_TYPE.to_owned(),
            format!("{}.{RAOP_TYPE}", raop_instance())
        )),
        None
    );
    let empty: &[IpAddr] = &[];
    let unaddressed = resolved(RAOP_TYPE, &raop_instance(), empty, Vec::new());
    assert_eq!(browse_event(unaddressed), None);
}

#[test]
fn instance_names_are_unescaped() {
    // RFC 6763 §4.3: dots and backslashes inside an instance are escaped.
    assert_eq!(
        instance_name(r"Living Room\.2._airplay._tcp.local.", ServiceKind::AirPlay),
        Some("Living Room.2".to_owned())
    );
    assert_eq!(
        instance_name(r"A\\B._airplay._tcp.local", ServiceKind::AirPlay),
        Some(r"A\B".to_owned())
    );
    assert_eq!(
        instance_name(r"Caf\195\169._airplay._tcp.local.", ServiceKind::AirPlay),
        Some("Café".to_owned())
    );
    assert_eq!(
        instance_name("Den._RAOP._TCP.LOCAL.", ServiceKind::Raop),
        Some("Den".to_owned()),
        "names compare case-insensitively"
    );
    assert_eq!(
        instance_name("Den._raop._tcp.local.", ServiceKind::AirPlay),
        None
    );
    assert_eq!(instance_name("._raop._tcp.local.", ServiceKind::Raop), None);

    // A name with a dot, the way the daemon writes it.
    let dotted = resolved(
        AIRPLAY_TYPE,
        "Living Room.2",
        &[IpAddr::from([192, 0, 2, 32])],
        props(&[("deviceid", "02:00:5E:00:53:02")]),
    );
    let Some(BrowseEvent::Resolved(record)) = browse_event(dotted) else {
        panic!("resolves");
    };
    assert_eq!(record.instance, "Living Room.2");
}

// A browser whose daemon is gone (its thread ended, so the channel's
// sending side was dropped) still hands over what was queued, then says it
// is dead instead of looking like a quiet network forever.
#[test]
fn a_browser_whose_daemon_is_gone_is_dead() {
    let (events, browser_events) = mpsc::channel();
    let mut browser = MdnsBrowser::from_events(browser_events);
    assert_eq!(browser.poll_event(), None);
    assert!(!browser.is_dead(), "an empty channel is a quiet network");
    let removed =
        ServiceEvent::ServiceRemoved(RAOP_TYPE.to_owned(), format!("{ID}@Den.{RAOP_TYPE}"));
    events.send(removed).expect("send");
    drop(events);
    assert_eq!(
        browser.poll_event(),
        Some(BrowseEvent::Removed {
            kind: ServiceKind::Raop,
            instance: format!("{ID}@Den"),
        })
    );
    assert!(!browser.is_dead(), "not before everything queued was read");
    assert_eq!(browser.poll_event(), None);
    assert!(browser.is_dead());
}

/// Browses the real network for a few seconds and prints what it finds.
/// Run by hand with `cargo test --test mdns -- --ignored`; never in CI.
#[test]
#[ignore = "browses the real network"]
fn live_browse_prints_receivers() {
    let mut browser = MdnsBrowser::new().expect("the daemon starts");
    let mut directory = Directory::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        while let Some(event) = browser.poll_event() {
            directory.apply(event);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for device in directory.devices() {
        println!("{:?}: {} {:?}", device.key, device.name, device.verdict);
    }
}
