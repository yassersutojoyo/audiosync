//! Discovery: records, TXT keys, merged devices, verdicts and the decisions
//! that rest on them (section 2 of docs/AIRPLAY_SPEC.md, with P1, P2, P45,
//! P46, P59, P74 and P139).

mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    BrowseEvent, Browser, Device, DeviceId, DeviceKey, Directory, DirectoryEvent, DnsRecord,
    Features, ProtocolPath, Resolver, ScopedAddr, SecurityMode, ServiceKind, ServiceRecord,
    StatusFlags, Txt, connect_order, parse_raop_instance,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::metadata::MetadataSupport;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::session::State;
use common::{FakeReceiver, Link, config};

const RAOP_NAME: &str = "0050C212A23F@Living Room";
const ID: DeviceId = DeviceId([0x00, 0x50, 0xC2, 0x12, 0xA2, 0x3F]);

fn v4(a: u8, b: u8, c: u8, d: u8) -> ScopedAddr {
    ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)))
}

fn v6(segments: [u16; 8], scope: u32) -> ScopedAddr {
    let [a, b, c, d, e, f, g, h] = segments;
    ScopedAddr::with_scope(IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h)), scope)
}

fn strings(txt: &[&str]) -> Vec<Vec<u8>> {
    txt.iter().map(|s| s.as_bytes().to_vec()).collect()
}

fn raop(instance: &str, txt: &[&str]) -> ServiceRecord {
    ServiceRecord {
        kind: ServiceKind::Raop,
        instance: instance.to_owned(),
        host: "livingroom.local".to_owned(),
        port: 7000,
        txt: strings(txt),
        addresses: vec![v4(192, 0, 2, 30)],
    }
}

fn airplay(instance: &str, txt: &[&str]) -> ServiceRecord {
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: instance.to_owned(),
        host: "livingroom.local".to_owned(),
        port: 7001,
        txt: strings(txt),
        addresses: vec![v4(192, 0, 2, 30)],
    }
}

/// A device from a RAOP TXT record alone.
fn raop_device(txt: &[&str]) -> Device {
    Device::from_records(DeviceKey::Id(ID), Some(&raop(RAOP_NAME, txt)), None).expect("device")
}

fn device(raop_txt: Option<&[&str]>, airplay_txt: Option<&[&str]>) -> Device {
    let raop = raop_txt.map(|txt| raop(RAOP_NAME, txt));
    let airplay = airplay_txt.map(|txt| airplay("Living Room", txt));
    Device::from_records(DeviceKey::Id(ID), raop.as_ref(), airplay.as_ref()).expect("device")
}

fn updated(events: &[DirectoryEvent]) -> Vec<&Device> {
    events
        .iter()
        .filter_map(|event| match event {
            DirectoryEvent::Updated(device) => Some(device.as_ref()),
            DirectoryEvent::Removed(_) => None,
        })
        .collect()
}

/// A browser that replays a list of events, as a live one would hand them over.
struct ReplayBrowser(Vec<BrowseEvent>);

impl Browser for ReplayBrowser {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        (!self.0.is_empty()).then(|| self.0.remove(0))
    }
}

// covers: P7
#[test]
fn discovery_browse_follows_ptr_to_srv_txt_and_addresses_for_both_types() {
    assert_eq!(ServiceKind::Raop.service_type(), "_raop._tcp");
    assert_eq!(ServiceKind::AirPlay.service_type(), "_airplay._tcp");
    assert_eq!(ServiceKind::DOMAIN, "local.");
    assert_eq!(
        ServiceKind::from_service_type("_RAOP._tcp.local."),
        Some(ServiceKind::Raop)
    );
    assert_eq!(
        ServiceKind::from_service_type("_airplay._tcp"),
        Some(ServiceKind::AirPlay)
    );
    assert_eq!(ServiceKind::from_service_type("_http._tcp.local."), None);

    let now = Instant::now();
    let mut resolver = Resolver::new();
    let mut events = Vec::new();
    let records = [
        DnsRecord::Ptr {
            kind: ServiceKind::Raop,
            instance: RAOP_NAME.to_owned(),
            ttl: 4500,
        },
        DnsRecord::Srv {
            kind: ServiceKind::Raop,
            instance: RAOP_NAME.to_owned(),
            target: "LivingRoom.local.".to_owned(),
            port: 7000,
            ttl: 120,
        },
        DnsRecord::Txt {
            kind: ServiceKind::Raop,
            instance: RAOP_NAME.to_owned(),
            strings: strings(&["txtvers=1", "cn=0,1", "et=0", "tp=UDP"]),
            ttl: 4500,
        },
        DnsRecord::Ptr {
            kind: ServiceKind::AirPlay,
            instance: "Living Room".to_owned(),
            ttl: 4500,
        },
        DnsRecord::Srv {
            kind: ServiceKind::AirPlay,
            instance: "Living Room".to_owned(),
            target: "livingroom.local".to_owned(),
            port: 7000,
            ttl: 120,
        },
        DnsRecord::Txt {
            kind: ServiceKind::AirPlay,
            instance: "Living Room".to_owned(),
            strings: strings(&["deviceid=00:50:c2:12:a2:3f", "features=0x1234ABCD,0x6"]),
            ttl: 4500,
        },
    ];
    for record in records {
        events.extend(resolver.record(record, now));
    }
    assert!(
        events.is_empty(),
        "nothing resolves before an address is known"
    );
    // An A record and an IPv6 link-local AAAA record, on two interfaces.
    events.extend(resolver.record(
        DnsRecord::Address {
            host: "livingroom.local.".to_owned(),
            address: v4(192, 0, 2, 30),
            ttl: 120,
        },
        now,
    ));
    events.extend(resolver.record(
        DnsRecord::Address {
            host: "LIVINGROOM.local".to_owned(),
            address: v6([0xfe80, 0, 0, 0, 0x1, 0x2, 0x3, 0x4], 7),
            ttl: 120,
        },
        now,
    ));
    let resolved: Vec<&ServiceRecord> = events
        .iter()
        .filter_map(|event| match event {
            BrowseEvent::Resolved(record) => Some(record),
            BrowseEvent::Removed { .. } => None,
        })
        .collect();
    assert_eq!(
        resolved.len(),
        4,
        "both services, then both again with the second address"
    );
    let last_raop = resolved
        .iter()
        .rev()
        .find(|record| record.kind == ServiceKind::Raop)
        .expect("RAOP resolved");
    assert_eq!(last_raop.port, 7000);
    assert_eq!(last_raop.host, "livingroom.local");
    assert_eq!(last_raop.addresses.len(), 2);

    // The directory, fed through the browser seam, makes one device of both.
    let mut browser = ReplayBrowser(events);
    let mut directory = Directory::new();
    while let Some(event) = browser.poll_event() {
        directory.apply(event);
    }
    let devices: Vec<&Device> = directory.devices().collect();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].key, DeviceKey::Id(ID));
    assert!(devices[0].raop.is_some() && devices[0].airplay.is_some());
    assert_eq!(resolver.next_expiry(), Some(now + Duration::from_secs(120)));
}

// covers: P8
#[test]
fn a_raop_instance_name_is_id_at_name() {
    assert_eq!(parse_raop_instance(RAOP_NAME), Some((ID, "Living Room")));
    assert_eq!(
        parse_raop_instance("0050c212a23f@Kitchen @ Home"),
        Some((ID, "Kitchen @ Home"))
    );
    assert_eq!(parse_raop_instance("0050C212A23@Short"), None);
    assert_eq!(parse_raop_instance("0050C212A23G@Not hex"), None);
    assert_eq!(parse_raop_instance("Living Room"), None);
}

// covers: P9
#[test]
fn airplay_deviceid_is_six_colon_separated_bytes() {
    assert_eq!(DeviceId::from_colon_form("00:50:C2:12:A2:3F"), Some(ID));
    assert_eq!(DeviceId::from_colon_form("00:50:c2:12:a2:3f"), Some(ID));
    assert_eq!(DeviceId::from_colon_form("0:50:C2:12:A2:3F"), None);
    assert_eq!(DeviceId::from_colon_form("00:50:C2:12:A2"), None);
    assert_eq!(DeviceId::from_colon_form("0050C212A23F"), None);
    let record = airplay("Living Room", &["deviceid=00:50:C2:12:A2:3F"]);
    assert_eq!(record.device_key(), DeviceKey::Id(ID));
}

// covers: P10
#[test]
fn discovery_merges_raop_and_airplay_into_one_device() {
    assert_eq!(ID.to_string(), "0050C212A23F");
    let mut directory = Directory::new();
    let events = directory.apply(BrowseEvent::Resolved(raop("0050c212a23f@Speaker", &[])));
    let devices = updated(&events);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "Speaker", "RAOP name after the @");
    assert_eq!(devices[0].raop.as_ref().map(|e| e.port), Some(7000));
    assert!(devices[0].airplay.is_none(), "one service is still listed");

    let events = directory.apply(BrowseEvent::Resolved(airplay(
        "Living Room",
        &["deviceid=00:50:C2:12:A2:3F"],
    )));
    let devices = updated(&events);
    assert_eq!(devices.len(), 1);
    let merged = devices[0];
    assert_eq!(merged.key, DeviceKey::Id(ID));
    assert_eq!(merged.name, "Living Room", "the AirPlay name wins");
    let raop_end = merged.raop.as_ref().expect("RAOP endpoint");
    let airplay_end = merged.airplay.as_ref().expect("AirPlay endpoint");
    assert_eq!(
        (raop_end.host.as_str(), raop_end.port),
        ("livingroom.local", 7000)
    );
    assert_eq!(
        (airplay_end.host.as_str(), airplay_end.port),
        ("livingroom.local", 7001)
    );
    assert_eq!(directory.devices().count(), 1);

    // An AirPlay-only device is listed under its own identifier.
    let other = airplay("Bedroom", &["deviceid=AA:BB:CC:DD:EE:FF"]);
    directory.apply(BrowseEvent::Resolved(other));
    assert_eq!(directory.devices().count(), 2);
    // A record without a usable identifier is listed on its own.
    directory.apply(BrowseEvent::Resolved(raop("Odd Speaker", &[])));
    let odd = directory
        .device(&DeviceKey::Instance(
            ServiceKind::Raop,
            "Odd Speaker".to_owned(),
        ))
        .expect("listed");
    assert_eq!(odd.name, "Odd Speaker");
    assert_eq!(directory.devices().count(), 3);

    // Removing the AirPlay service brings the RAOP name back; removing the
    // last service removes the device.
    let events = directory.apply(BrowseEvent::Removed {
        kind: ServiceKind::AirPlay,
        instance: "Living Room".to_owned(),
    });
    assert_eq!(updated(&events)[0].name, "Speaker");
    let events = directory.apply(BrowseEvent::Removed {
        kind: ServiceKind::Raop,
        instance: "0050c212a23f@Speaker".to_owned(),
    });
    assert_eq!(events, [DirectoryEvent::Removed(DeviceKey::Id(ID))]);
}

// covers: P11
#[test]
fn txt_keys_are_case_insensitive_and_values_forgiving() {
    let txt = Txt::parse(&strings(&[
        "CN=0,1,2",
        "pw",
        "da=TRUE",
        "sv=0",
        "sr=0xAC44",
        "ss=sixteen",
        "tp=UDP, TCP",
        "",
        "=orphan",
        "cn=3",
        "zz=unknown",
        "et=0,x",
    ]));
    assert_eq!(
        txt.int_list("cn"),
        Some(vec![0, 1, 2]),
        "keys compare case-insensitively"
    );
    assert_eq!(txt.flag("PW"), Some(true), "a key without = is true");
    assert_eq!(txt.flag("da"), Some(true));
    assert_eq!(txt.flag("sv"), Some(false));
    assert_eq!(txt.int("sr"), Some(44_100), "0x hex");
    assert_eq!(txt.int("ss"), None, "a malformed value drops its key");
    assert_eq!(
        txt.list("tp"),
        Some(vec!["UDP".to_owned(), "TCP".to_owned()])
    );
    assert_eq!(txt.int_list("et"), None, "one bad item drops the list");
    assert_eq!(txt.string("zz"), Some("unknown"));
    assert_eq!(txt.raw("missing"), None);
    // A malformed value drops only that key: the rest of the record reads.
    let raop = audiosync_airplay::discovery::RaopTxt::read(&txt);
    assert_eq!(raop.sample_size, None);
    assert_eq!(raop.sample_rate, Some(44_100));
    assert_eq!(raop.encryption, None);
    assert_eq!(raop.password, Some(true));
}

// covers: P12
#[test]
fn raop_txt_keys_are_read() {
    let txt = Txt::parse(&strings(&[
        "txtvers=1",
        "ch=2",
        "sr=44100",
        "ss=16",
        "cn=0,1,2,3",
        "et=0,3,5",
        "md=0,1,2",
        "pw=false",
        "da=true",
        "tp=UDP",
        "sv=false",
        "sm=false",
        "sf=0x4",
        "ft=0x1234ABCD,0x6",
        "am=AppleTV9,1",
        "vs=1.0",
        "vn=2",
        "fv=1.2.3",
        "ov=4.5.6",
        "pk=0a0b0c",
    ]));
    let raop = audiosync_airplay::discovery::RaopTxt::read(&txt);
    assert_eq!(raop.txt_version.as_deref(), Some("1"));
    assert_eq!(
        (raop.channels, raop.sample_rate, raop.sample_size),
        (Some(2), Some(44_100), Some(16))
    );
    assert_eq!(raop.codecs, Some(vec![0, 1, 2, 3]));
    assert_eq!(raop.encryption, Some(vec![0, 3, 5]));
    assert_eq!(raop.metadata, Some(vec![0, 1, 2]));
    assert_eq!(raop.password, Some(false));
    assert_eq!(raop.digest_auth, Some(true));
    assert_eq!(raop.transports, Some(vec!["UDP".to_owned()]));
    assert_eq!(
        (raop.software_volume, raop.software_mute),
        (Some(false), Some(false))
    );
    assert_eq!(raop.status_flags, Some(StatusFlags(4)));
    assert_eq!(raop.features, Some(Features(0x6_1234_ABCD)));
    assert_eq!(raop.model.as_deref(), Some("AppleTV9,1"));
    assert_eq!(raop.server_version.as_deref(), Some("1.0"));
    assert_eq!(raop.protocol_version.as_deref(), Some("2"));
    assert_eq!(raop.firmware_version.as_deref(), Some("1.2.3"));
    assert_eq!(raop.os_version.as_deref(), Some("4.5.6"));
    assert_eq!(raop.public_key, Some(vec![0x0a, 0x0b, 0x0c]));
}

// covers: P13
#[test]
fn airplay_txt_keys_are_read() {
    let key = "ab".repeat(32);
    let txt = Txt::parse(&strings(&[
        "deviceid=00:50:C2:12:A2:3F",
        "features=0x1234ABCD,0x6",
        "flags=0x244",
        "model=Speaker1,1",
        "manufacturer=Example",
        "srcvers=1.0",
        "protovers=1.1",
        "pw",
        &format!("pk={key}"),
        "pi=00000000-0000-0000-0000-0000000000a1",
        "psi=00000000-0000-0000-0000-000000000001",
        "gid=6f4a2b1c-0000-0000-0000-000000000002",
        "igl=1",
        "gcgl=0",
        "acl=0",
        "rsf=0x0",
        "fv=7.8",
        "osvers=14.0",
    ]));
    let airplay = audiosync_airplay::discovery::AirPlayTxt::read(&txt);
    assert_eq!(airplay.device_id, Some(ID));
    assert_eq!(airplay.features, Some(Features(0x6_1234_ABCD)));
    assert_eq!(airplay.flags, Some(StatusFlags(0x244)));
    assert_eq!(airplay.model.as_deref(), Some("Speaker1,1"));
    assert_eq!(airplay.manufacturer.as_deref(), Some("Example"));
    assert_eq!(airplay.source_version.as_deref(), Some("1.0"));
    assert_eq!(airplay.protocol_version.as_deref(), Some("1.1"));
    assert_eq!(airplay.password, Some(true));
    assert_eq!(airplay.public_key, Some([0xab; 32]));
    assert!(airplay.pairing_id.is_some() && airplay.pairing_system_id.is_some());
    assert!(airplay.group_id.is_some());
    assert_eq!(airplay.is_group_leader.as_deref(), Some("1"));
    assert_eq!(airplay.group_contains_group_leader.as_deref(), Some("0"));
    assert_eq!(airplay.access_control.as_deref(), Some("0"));
    assert_eq!(airplay.required_sender_features.as_deref(), Some("0x0"));
    assert_eq!(airplay.firmware_version.as_deref(), Some("7.8"));
    assert_eq!(airplay.os_version.as_deref(), Some("14.0"));
    // A key of the wrong length is dropped.
    let short = Txt::parse(&strings(&["pk=abcd"]));
    assert_eq!(
        audiosync_airplay::discovery::AirPlayTxt::read(&short).public_key,
        None
    );
}

// covers: P14, P15
#[test]
fn discovery_features_and_flags() {
    let features = Features::parse_txt("0x1234ABCD,0x6").expect("two parts");
    assert_eq!(features, Features(0x0000_0006_1234_ABCD));
    assert!(features.has(Features::AUDIO));
    assert!(
        features.has(33) && features.has(34),
        "the second part is bits 32 to 63"
    );
    assert!(!features.has(32));
    assert_eq!(
        Features::parse_txt("0x00C0FFEE"),
        Some(Features(0x00C0_FFEE))
    );
    assert_eq!(Features::parse_txt("00C0FFEE"), Some(Features(0x00C0_FFEE)));
    assert_eq!(Features::parse_txt("0x1,0x2,0x3"), None);
    assert_eq!(
        Features::parse_txt("0x1FFFFFFFF"),
        None,
        "wider than 32 bits"
    );
    assert_eq!(Features::parse_txt("zz"), None);
    // RAOP `ft` is the same mask; /info reports it as one decimal integer.
    assert_eq!(Features::from_info(26_075_245_517), Features(0x6_1234_ABCD));
    assert!(!features.has(64));

    let flags = StatusFlags::parse_txt("0x2028C").expect("hex");
    for bit in [
        StatusFlags::AUDIO_CABLE_ATTACHED,
        StatusFlags::PIN_REQUIRED,
        StatusFlags::PASSWORD_REQUIRED,
        StatusFlags::ONE_TIME_PAIRING_REQUIRED,
        StatusFlags::SESSION_ACTIVE,
    ] {
        assert!(flags.has(bit), "bit {bit}");
    }
    assert_eq!(StatusFlags::parse_txt("4"), Some(StatusFlags(4)));
    assert!(!StatusFlags(4).has(StatusFlags::PIN_REQUIRED));
    assert_eq!(StatusFlags::parse_txt("0x100000000"), None);
}

// covers: P16
#[test]
fn discovery_verdict_table() {
    struct Row {
        raop: Option<&'static [&'static str]>,
        airplay: Option<&'static [&'static str]>,
        password: bool,
        pairing: bool,
        busy: bool,
        airplay1: Result<(), UnsupportedReason>,
        airplay2: bool,
    }
    let rows = [
        // Missing cn and et: ALAC, try unencrypted.
        Row {
            raop: Some(&[]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: Some(&["pw=true"]),
            airplay: None,
            password: true,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: Some(&[]),
            airplay: Some(&["pw"]),
            password: true,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: Some(&[]),
            airplay: Some(&["pw=false"]),
            password: false,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: Some(&["sf=0x80"]),
            airplay: None,
            password: true,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: Some(&[]),
            airplay: Some(&["flags=0x8"]),
            password: false,
            pairing: true,
            busy: false,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: Some(&["sf=0x200"]),
            airplay: None,
            password: false,
            pairing: true,
            busy: false,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: Some(&["sf=0x20000"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: true,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: Some(&["et=3,5"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Err(UnsupportedReason::Encryption),
            airplay2: false,
        },
        Row {
            raop: Some(&["cn=2,3"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Err(UnsupportedReason::Codec),
            airplay2: false,
        },
        Row {
            raop: Some(&["tp=TCP"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Err(UnsupportedReason::Transport),
            airplay2: false,
        },
        Row {
            raop: Some(&["sr=48000"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Err(UnsupportedReason::Format),
            airplay2: false,
        },
        Row {
            raop: Some(&["ss=24"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Err(UnsupportedReason::Format),
            airplay2: false,
        },
        Row {
            raop: Some(&["ch=1"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Err(UnsupportedReason::Format),
            airplay2: false,
        },
        Row {
            raop: Some(&["ch=2", "sr=44100", "ss=16", "cn=0", "et=0,4", "tp=TCP,UDP"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: false,
        },
        Row {
            raop: None,
            airplay: Some(&["features=0x0,0x10000"]),
            password: false,
            pairing: false,
            busy: false,
            airplay1: Err(UnsupportedReason::AirPlay2Only),
            airplay2: true,
        },
        Row {
            raop: Some(&[]),
            airplay: Some(&["features=0x0,0x40"]),
            password: false,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: true,
        },
        Row {
            raop: Some(&["ft=0x0,0x800"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: true,
        },
        Row {
            raop: Some(&["ft=0x0,0x4000"]),
            airplay: None,
            password: false,
            pairing: false,
            busy: false,
            airplay1: Ok(()),
            airplay2: true,
        },
    ];
    for (index, row) in rows.iter().enumerate() {
        let device = device(row.raop, row.airplay);
        let verdict = device.verdict;
        assert_eq!(verdict.needs_password, row.password, "row {index}");
        assert_eq!(verdict.needs_pairing, row.pairing, "row {index}");
        assert_eq!(verdict.busy_hint, row.busy, "row {index}");
        assert_eq!(verdict.airplay1, row.airplay1, "row {index}");
        assert_eq!(verdict.airplay2, row.airplay2, "row {index}");
    }
    // Buffered audio and PTP are noted.
    let noted = device(Some(&[]), Some(&["features=0x0,0x300"])).verdict;
    assert!(noted.buffered_audio && noted.ptp && !noted.airplay2);

    // The verdict follows TXT changes (P19 reports them).
    let mut directory = Directory::new();
    directory.apply(BrowseEvent::Resolved(raop(RAOP_NAME, &[])));
    let events = directory.apply(BrowseEvent::Resolved(raop(RAOP_NAME, &["sf=0x20000"])));
    assert!(updated(&events)[0].verdict.busy_hint);
}

// covers: P74
#[test]
fn discovery_codec_choice_is_alac_unless_pcm_is_enabled_or_alone() {
    let alac = raop_device(&["cn=0,1"]);
    assert_eq!(alac.choose_codec(false), Ok(Codec::Alac), "ALAC by default");
    assert_eq!(
        alac.choose_codec(true),
        Ok(Codec::Pcm),
        "PCM when the host enables it"
    );
    assert!(alac.alac_fallback());
    assert_eq!(
        raop_device(&[]).choose_codec(true),
        Ok(Codec::Alac),
        "missing cn: ALAC only"
    );
    assert_eq!(raop_device(&["cn=1"]).choose_codec(true), Ok(Codec::Alac));
    let pcm_only = raop_device(&["cn=0"]);
    assert_eq!(
        pcm_only.choose_codec(false),
        Ok(Codec::Pcm),
        "PCM when it is the only codec"
    );
    assert!(!pcm_only.alac_fallback());
    assert_eq!(
        raop_device(&["cn=2,3,4"]).choose_codec(true),
        Err(UnsupportedReason::Codec)
    );

    // After a refused PCM ANNOUNCE the directory remembers ALAC for the device.
    let mut directory = Directory::new();
    directory.apply(BrowseEvent::Resolved(raop(RAOP_NAME, &["cn=0,1"])));
    let key = DeviceKey::Id(ID);
    assert_eq!(directory.codec_for(&key, true), Some(Ok(Codec::Pcm)));
    directory.remember_pcm_refused(&key);
    assert_eq!(directory.codec_for(&key, true), Some(Ok(Codec::Alac)));
    assert_eq!(
        directory.codec_for(&DeviceKey::Id(DeviceId([0; 6])), true),
        None
    );
}

// covers: P17
#[test]
fn discovery_addresses_ipv4_first_then_ipv6_global_then_link_local() {
    let link_local_v4 = v4(169, 254, 10, 20);
    let global_v6 = v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 5], 0);
    let link_local_v6 = v6([0xfe80, 0, 0, 0, 0, 0, 0, 5], 3);
    let ipv4 = v4(192, 0, 2, 40);
    assert_eq!(
        connect_order(&[link_local_v6, link_local_v4, global_v6, ipv4, ipv4]),
        [ipv4, global_v6, link_local_v6],
        "IPv4 link-local is dropped while another address exists, duplicates too"
    );
    assert_eq!(
        connect_order(&[link_local_v4]),
        [link_local_v4],
        "unless it is the only one"
    );
    assert_eq!(
        connect_order(&[link_local_v6])[0].scope_id,
        3,
        "the scope is kept"
    );
    let mut record = raop(RAOP_NAME, &[]);
    record.addresses = vec![link_local_v6, ipv4];
    let device = Device::from_records(DeviceKey::Id(ID), Some(&record), None).expect("device");
    assert_eq!(device.connect_addresses(), [ipv4, link_local_v6]);
}

// covers: P18, P19
#[test]
fn discovery_goodbye_and_txt_changes_update_the_device_but_not_the_session() {
    let now = Instant::now();
    let mut resolver = Resolver::new();
    let mut directory = Directory::new();
    let feed = |resolver: &mut Resolver, directory: &mut Directory, record: DnsRecord| {
        let mut changes = Vec::new();
        for event in resolver.record(record, now) {
            changes.extend(directory.apply(event));
        }
        changes
    };
    let ptr = |ttl| DnsRecord::Ptr {
        kind: ServiceKind::Raop,
        instance: RAOP_NAME.to_owned(),
        ttl,
    };
    let txt = |items: &[&str]| DnsRecord::Txt {
        kind: ServiceKind::Raop,
        instance: RAOP_NAME.to_owned(),
        strings: strings(items),
        ttl: 4500,
    };
    feed(&mut resolver, &mut directory, ptr(4500));
    feed(
        &mut resolver,
        &mut directory,
        DnsRecord::Srv {
            kind: ServiceKind::Raop,
            instance: RAOP_NAME.to_owned(),
            target: "livingroom.local".to_owned(),
            port: 7000,
            ttl: 120,
        },
    );
    feed(&mut resolver, &mut directory, txt(&[]));
    feed(
        &mut resolver,
        &mut directory,
        DnsRecord::Address {
            host: "livingroom.local".to_owned(),
            address: v4(192, 0, 2, 30),
            ttl: 120,
        },
    );
    assert_eq!(directory.devices().count(), 1);

    // A session plays to the device.
    let mut link = Link::new(config(), FakeReceiver::new());
    link.connect();
    assert_eq!(link.session.state(), State::Playing);

    // Another sender starts playing: the flags change, the verdict follows.
    let changes = feed(&mut resolver, &mut directory, txt(&["sf=0x20000"]));
    assert!(updated(&changes)[0].verdict.busy_hint);
    // A goodbye: the device is gone for new sessions.
    let changes = feed(&mut resolver, &mut directory, ptr(0));
    assert_eq!(changes, [DirectoryEvent::Removed(DeviceKey::Id(ID))]);
    assert_eq!(directory.devices().count(), 0);
    // The running session is untouched; its own connection decides.
    link.advance(Duration::from_secs(5));
    assert_eq!(link.session.state(), State::Playing);
    assert!(link.session.media_allowed());

    // Expiry works the same way.
    let mut resolver = Resolver::new();
    for record in [
        ptr(4500),
        DnsRecord::Srv {
            kind: ServiceKind::Raop,
            instance: RAOP_NAME.to_owned(),
            target: "livingroom.local".to_owned(),
            port: 7000,
            ttl: 120,
        },
        txt(&[]),
        DnsRecord::Address {
            host: "livingroom.local".to_owned(),
            address: v4(192, 0, 2, 30),
            ttl: 120,
        },
    ] {
        resolver.record(record, now);
    }
    assert!(resolver.expire(now + Duration::from_secs(119)).is_empty());
    assert_eq!(
        resolver.expire(now + Duration::from_secs(120)),
        [BrowseEvent::Removed {
            kind: ServiceKind::Raop,
            instance: RAOP_NAME.to_owned()
        }]
    );
}

// covers: P1, P2, P45
#[test]
fn scope_and_protocol_choice() {
    // Only encryption types 1, 3, 4 or 5: unsupported (no FairPlay, MFi or RSA).
    for et in ["et=1", "et=3", "et=4", "et=5", "et=1,3,4,5"] {
        let device = raop_device(&[et]);
        assert_eq!(
            device.verdict.airplay1,
            Err(UnsupportedReason::Encryption),
            "{et}"
        );
        assert_eq!(
            device.airplay1_plan(false),
            Err(ErrorKind::Unsupported(UnsupportedReason::Encryption))
        );
    }

    // An AirPlay 2 device that also has RAOP goes over AirPlay 1.
    let both = device(Some(&["et=0,4"]), Some(&["features=0x0,0x10000"]));
    assert!(both.verdict.airplay2);
    assert_eq!(both.choose_path(true, None), Ok(ProtocolPath::AirPlay1));
    assert_eq!(both.choose_path(false, None), Ok(ProtocolPath::AirPlay1));
    // Password, pairing and busy outcomes never switch paths.
    for outcome in [
        ErrorKind::PasswordRequired,
        ErrorKind::WrongPassword,
        ErrorKind::Busy,
        ErrorKind::PairingRequired,
        ErrorKind::PairingFailed,
        ErrorKind::Timeout,
    ] {
        assert_eq!(
            both.choose_path(true, Some(outcome)),
            Ok(ProtocolPath::AirPlay1),
            "{outcome:?}"
        );
    }
    // Unsupported or Refused switch it when the AirPlay 2 path is on...
    let unsupported = ErrorKind::Unsupported(UnsupportedReason::Methods);
    assert_eq!(
        both.choose_path(true, Some(ErrorKind::Refused)),
        Ok(ProtocolPath::AirPlay2)
    );
    assert_eq!(
        both.choose_path(true, Some(unsupported)),
        Ok(ProtocolPath::AirPlay2)
    );
    // ...and give "AirPlay 2 only" while it is off (the default).
    assert_eq!(
        both.choose_path(false, Some(ErrorKind::Refused)),
        Err(UnsupportedReason::AirPlay2Only)
    );
    // An AirPlay 2 device without RAOP.
    let only = device(None, Some(&["features=0x0,0x10000"]));
    assert_eq!(
        only.choose_path(false, None),
        Err(UnsupportedReason::AirPlay2Only)
    );
    assert_eq!(only.choose_path(true, None), Ok(ProtocolPath::AirPlay2));
    // A plain AirPlay 1 receiver that refused stays on AirPlay 1; one that is
    // unsupported keeps its reason.
    let plain = raop_device(&[]);
    assert_eq!(
        plain.choose_path(true, Some(ErrorKind::Refused)),
        Ok(ProtocolPath::AirPlay1)
    );
    assert_eq!(
        raop_device(&["cn=2"]).choose_path(true, None),
        Err(UnsupportedReason::Codec)
    );

    // The mode table (P45).
    let apple_tv = &["am=AppleTV9,2", "sf=0x8", "pw=true", "et=0,3,5"][..];
    let cases: [(&[&str], Vec<SecurityMode>); 5] = [
        (&["et=0"], vec![SecurityMode::Open]),
        (&[], vec![SecurityMode::Open]),
        (&["et=0", "pw=true"], vec![SecurityMode::Password]),
        (&["et=1"], vec![SecurityMode::Rsa]),
        (
            apple_tv,
            vec![SecurityMode::Password, SecurityMode::LegacyPairing],
        ),
    ];
    for (txt, modes) in cases {
        assert_eq!(raop_device(txt).security_modes(), modes, "{txt:?}");
    }
    assert_eq!(
        device(Some(&["et=0"]), Some(&["features=0x0,0x10000"])).security_modes(),
        [SecurityMode::Open, SecurityMode::HomeKit]
    );
}

// covers: P46
#[test]
fn an_apple_tv_with_the_pin_flags_needs_legacy_pairing() {
    for flags in ["sf=0x8", "sf=0x200"] {
        let apple_tv = raop_device(&["am=AppleTV9,3", flags, "et=3,5"]);
        assert!(apple_tv.is_apple_tv());
        assert!(apple_tv.verdict.legacy_pairing, "{flags}");
        // Legacy pairing makes it AirPlay 1 capable without `et` 0.
        assert_eq!(apple_tv.verdict.airplay1, Ok(()));
        assert_eq!(
            apple_tv.airplay1_plan(false),
            Err(ErrorKind::PairingRequired)
        );
        assert_eq!(
            apple_tv.airplay1_plan(true),
            Ok(()),
            "verifies with credentials"
        );
    }
    // The AirPlay `model` counts too.
    let by_model = device(Some(&["et=0"]), Some(&["model=AppleTV9,4", "flags=0x8"]));
    assert!(by_model.verdict.legacy_pairing);
    // Other receivers with the PIN flag aren't Apple TVs.
    let other = raop_device(&["am=Speaker1,1", "sf=0x8"]);
    assert!(!other.verdict.legacy_pairing);
    assert_eq!(other.airplay1_plan(false), Ok(()));
    // An Apple TV without the flags plays without pairing.
    assert!(!raop_device(&["am=AppleTV9,1"]).verdict.legacy_pairing);
}

// covers: P59, P139
#[test]
fn rsa_only_and_other_unsupported_receivers_are_reported_without_connecting() {
    let rsa = raop_device(&["et=1"]);
    assert_eq!(rsa.verdict.airplay1, Err(UnsupportedReason::Encryption));
    assert_eq!(
        rsa.airplay1_plan(false),
        Err(ErrorKind::Unsupported(UnsupportedReason::Encryption))
    );
    assert!(rsa.security_modes().contains(&SecurityMode::Rsa));
    for (txt, reason) in [
        ("cn=2", UnsupportedReason::Codec),
        ("tp=TCP", UnsupportedReason::Transport),
        ("sr=48000", UnsupportedReason::Format),
    ] {
        assert_eq!(
            raop_device(&[txt]).airplay1_plan(false),
            Err(ErrorKind::Unsupported(reason)),
            "{txt}"
        );
    }
    let only = device(None, Some(&["features=0x0,0x10000"]));
    assert_eq!(
        only.airplay1_plan(false),
        Err(ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only))
    );
    assert_eq!(
        UnsupportedReason::AirPlay2Only.to_string(),
        "AirPlay 2 only"
    );
    assert!(ErrorKind::Unsupported(UnsupportedReason::Codec).needs_user());
}

// covers: P137
#[test]
fn flags_bit_17_is_only_a_busy_hint() {
    let busy = raop_device(&["sf=0x20000"]);
    assert!(busy.verdict.busy_hint);
    assert_eq!(busy.airplay1_plan(false), Ok(()), "the sender still tries");
}

#[test]
fn auth_setup_and_metadata_follow_the_txt_record() {
    assert!(raop_device(&["et=0,4"]).needs_auth_setup());
    assert!(raop_device(&["ft=0x4000000"]).needs_auth_setup(), "bit 26");
    assert!(
        raop_device(&["ft=0x0,0x80000"]).needs_auth_setup(),
        "bit 51"
    );
    assert!(!raop_device(&["et=0"]).needs_auth_setup());
    assert_eq!(
        raop_device(&["md=0,2"]).metadata_support(),
        MetadataSupport {
            text: true,
            artwork: false,
            progress: true
        }
    );
    assert_eq!(
        raop_device(&[]).metadata_support(),
        MetadataSupport::default()
    );
}
