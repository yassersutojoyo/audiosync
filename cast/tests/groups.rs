//! Cast groups (C3, C14, C89 to C103): a group is listed under its own
//! `id`, played as one device over one channel to its port, follows its
//! leader's records, lists its members from the multizone namespace and
//! keeps its own volume, offset and one stream response per address.

mod common;

use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioWriter, StreamFormat, audio_ring};
use audiosync_cast::client::FingerprintBook;
use audiosync_cast::discovery::{
    BrowseEvent, DEVICE_PORT, DeviceId, DeviceKey, DeviceKind, DeviceList, ListEvent, ServiceRecord,
};
use audiosync_cast::group::{GroupMember, GroupRecords, MemberList, MultizoneMessage};
use audiosync_cast::host::{CastHost, CastTarget, DeviceMemory, StartRefused};
use audiosync_cast::media::Metadata;
use audiosync_cast::message::{NS_MEDIA, NS_MULTIZONE, NS_RECEIVER, PLATFORM};
use audiosync_cast::session::{CastSession, Fingerprints, SessionConfig, SessionEnd, SessionEvent};
use audiosync_cast::stream::{
    Popped, ResponseEnd, Responses, ServerEvent, StreamConfig, StreamServer,
};
use common::{Behavior, Command, FakeDevice, http, json_message};
use serde_json::{Map, Value, json};

const GROUP: &str = "6a1c0e5b9d2f4e7a8b3c1d0e9f8a7b6c";
const MEMBER: &str = "3f2a8c41d27e4b8f9a06c1d5e7f80b12";
const MEMBER_DASHED: &str = "3F2A8C41-D27E-4B8F-9A06-C1D5E7F80B12";
const OTHER_MEMBER: &str = "0b1c2d3e4f5a46b7a8c9d0e1f2a3b4c5";

fn v4(last: u8) -> IpAddr {
    IpAddr::from([192, 0, 2, last])
}

fn record(instance: &str, id: &str, name: &str, port: u16, ip: IpAddr) -> BrowseEvent {
    BrowseEvent::Resolved(ServiceRecord {
        fullname: format!("{instance}._googlecast._tcp.local."),
        port,
        txt: vec![
            format!("id={id}").into_bytes(),
            format!("fn={name}").into_bytes(),
            b"md=Google Cast Group".to_vec(),
            b"ca=199204".to_vec(),
        ],
        addresses: vec![ip],
    })
}

fn group_key() -> DeviceKey {
    DeviceKey::Id(DeviceId::parse(GROUP).expect("uuid"))
}

fn member_key() -> DeviceKey {
    DeviceKey::Id(DeviceId::parse(MEMBER).expect("uuid"))
}

fn members_json() -> Value {
    json!([
        {"deviceId": MEMBER_DASHED, "name": "Kitchen", "capabilities": 4},
        {"deviceId": OTHER_MEMBER, "name": "Porch"},
    ])
}

struct Feed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn feed(writer: AudioWriter) -> Feed {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let block = vec![5i16; 960];
        let start = Instant::now();
        let mut written = 0u32;
        while !flag.load(Ordering::SeqCst) {
            writer.write_i16(&block);
            written += 1;
            let next = start + Duration::from_millis(10) * written;
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    Feed {
        stop,
        thread: Some(thread),
    }
}

/// A group session whose records point at `leader`; the configured
/// addresses are a dead end, so only the records can make it connect.
fn start_group(leader: &FakeDevice) -> (CastSession, GroupRecords, Feed) {
    start_group_with(leader, None)
}

fn start_group_with(
    leader: &FakeDevice,
    fingerprints: Option<Fingerprints>,
) -> (CastSession, GroupRecords, Feed) {
    let records = GroupRecords::new(vec![leader.address]);
    let config = SessionConfig {
        addresses: vec![SocketAddr::new(v4(99), 1)],
        format: StreamFormat::wav(48_000).expect("rate"),
        metadata: Metadata::default(),
        artwork: None,
        group: Some(records.clone()),
        records: None,
        fingerprints,
    };
    let (writer, reader) = audio_ring(48_000);
    let feed = feed(writer);
    let selected = Arc::new(AtomicBool::new(true));
    let session = CastSession::start(config, reader, selected, |_, _| {}).expect("plays");
    (session, records, feed)
}

fn run_until(
    session: &mut CastSession,
    limit: Duration,
    mut done: impl FnMut(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    let until = Instant::now() + limit;
    let mut seen = Vec::new();
    while Instant::now() < until {
        session.run_for(Duration::from_millis(50));
        let mut matched = false;
        while let Some(event) = session.poll_event() {
            matched |= done(&event);
            seen.push(event);
        }
        if matched {
            return seen;
        }
    }
    panic!("no matching event in {limit:?}: {seen:?}");
}

fn drain(session: &mut CastSession, duration: Duration) -> Vec<SessionEvent> {
    session.run_for(duration);
    let mut seen = Vec::new();
    while let Some(event) = session.poll_event() {
        seen.push(event);
    }
    seen
}

fn last_members(events: &[SessionEvent]) -> Option<Vec<String>> {
    events.iter().rev().find_map(|event| match event {
        SessionEvent::Members(members) => Some(
            members
                .iter()
                .map(|member| member.name.clone().unwrap_or_default())
                .collect(),
        ),
        _ => None,
    })
}

fn multizone(kind: &str, fields: Value) -> Command {
    let mut payload = json!({"type": kind});
    if let (Some(payload), Value::Object(fields)) = (payload.as_object_mut(), fields) {
        payload.extend(fields);
    }
    Command::Send(json_message(PLATFORM, "*", NS_MULTIZONE, payload))
}

fn payload(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("an object")
}

// covers: C3, C14, C89, C91
#[test]
fn a_group_is_its_own_instance_and_its_ca_and_md_are_only_logged() {
    let mut list = DeviceList::new();
    list.apply(record("Kitchen", MEMBER, "Kitchen", DEVICE_PORT, v4(10)));
    let event = list.apply(record("Downstairs", GROUP, "Downstairs", 32_187, v4(10)));
    assert_eq!(event, ListEvent::Added(group_key()));
    // A temporary group looks the same and is listed the same way.
    let temporary = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
    list.apply(record("Party", temporary, "Party", 42_001, v4(11)));
    assert_eq!(list.entries().len(), 3);

    let group = list.get(group_key()).expect("listed");
    let device = group.device.as_ref().expect("resolved");
    assert_eq!(device.kind, DeviceKind::Group);
    assert_eq!(device.name.as_deref(), Some("Downstairs"));
    assert_eq!(group.addresses, [SocketAddr::new(v4(10), 32_187)]);
    let line = device.group_log_line().expect("a group's line");
    assert!(
        line.contains("ca=199204") && line.contains("md=Google Cast Group"),
        "{line}"
    );

    // The same TXT on port 8009 is a single device: the port alone counts.
    let member = list.get(member_key()).expect("listed");
    let member = member.device.as_ref().expect("resolved");
    assert_eq!(member.kind, DeviceKind::Single);
    assert_eq!(member.group_log_line(), None);
}

// covers: C90
#[test]
fn a_groups_entry_follows_its_records_and_never_a_remembered_address() {
    let mut list = DeviceList::new();
    list.apply(record("Downstairs", GROUP, "Downstairs", 32_187, v4(10)));
    list.remember_address(group_key(), SocketAddr::new(v4(10), 32_187));
    // The leader handed over: new target, port and address, same id.
    let event = list.apply(record("Downstairs", GROUP, "Downstairs", 32_190, v4(11)));
    assert_eq!(event, ListEvent::Updated(group_key()));
    let entry = list.get(group_key()).expect("listed");
    assert_eq!(entry.addresses, [SocketAddr::new(v4(11), 32_190)]);
    assert_eq!(list.entries().len(), 1);
    list.remember_address(group_key(), SocketAddr::new(v4(10), 32_187));
    assert_eq!(
        list.get(group_key()).expect("listed").addresses,
        [SocketAddr::new(v4(11), 32_190)]
    );

    // Expired records during a session mark it offline (C17).
    list.set_in_session(group_key(), true);
    let event = list.apply(BrowseEvent::Removed {
        fullname: "Downstairs._googlecast._tcp.local.".to_owned(),
    });
    assert_eq!(event, ListEvent::Offline(group_key()));

    // An empty update keeps where the group was until its records return.
    let records = GroupRecords::new(vec![SocketAddr::new(v4(11), 32_190)]);
    records.set_group(Vec::new());
    assert_eq!(records.get().group, [SocketAddr::new(v4(11), 32_190)]);
}

// covers: C92, C93
#[test]
fn a_group_plays_over_one_channel_to_its_port_and_nothing_else() {
    let setup_interface = TcpListener::bind("127.0.0.1:0").expect("bind");
    setup_interface.set_nonblocking(true).expect("nonblocking");
    let leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    assert_ne!(leader.address.port(), DEVICE_PORT);
    let (mut session, _records, _feed) = start_group(&leader);
    drain(&mut session, Duration::from_millis(500));
    assert_eq!(leader.connection_count(), 1);
    assert_eq!(leader.payloads(NS_RECEIVER, "LAUNCH").len(), 1);
    assert_eq!(leader.payloads(NS_MEDIA, "LOAD").len(), 1);
    assert!(!leader.fetches().is_empty(), "the group fetched the stream");
    assert!(
        setup_interface.accept().is_err(),
        "no other port of the device was contacted"
    );
    session.stop();
    leader.join();
}

// covers: C94
#[test]
fn the_member_list_comes_from_the_multizone_namespace() {
    let leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    let (mut session, _records, _feed) = start_group(&leader);
    let requests = leader.payloads(NS_MULTIZONE, "GET_STATUS");
    assert_eq!(requests.len(), 1);
    let seen = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(last_members(&seen).expect("members"), ["Kitchen", "Porch"]);

    leader.send(multizone(
        "DEVICE_ADDED",
        json!({"device": {"deviceId": "c0ffee00c0ffee00c0ffee00c0ffee00", "name": "Attic"}}),
    ));
    let seen = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(
        last_members(&seen).expect("members"),
        ["Kitchen", "Porch", "Attic"]
    );

    leader.send(multizone(
        "DEVICE_UPDATED",
        json!({"device": {"deviceId": MEMBER, "name": "Kitchen Speaker"}}),
    ));
    let seen = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(
        last_members(&seen).expect("members"),
        ["Kitchen Speaker", "Porch", "Attic"]
    );

    leader.send(multizone(
        "DEVICE_REMOVED",
        json!({"deviceId": OTHER_MEMBER}),
    ));
    let seen = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(
        last_members(&seen).expect("members"),
        ["Kitchen Speaker", "Attic"]
    );

    leader.send(multizone(
        "MULTIZONE_STATUS",
        json!({"status": {"devices": [{"deviceId": OTHER_MEMBER, "name": "Porch"}]}}),
    ));
    let seen = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(last_members(&seen).expect("members"), ["Porch"]);
    session.stop();
    leader.join();

    // No answer at all: the group plays on with its members unknown.
    let silent = FakeDevice::start(Behavior::default());
    let (mut session, _records, _feed) = start_group(&silent);
    let seen = drain(&mut session, Duration::from_millis(5_500));
    assert!(last_members(&seen).is_none());
    assert!(!session.is_ended());
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::Ended(_)))
    );
    session.stop();
    silent.join();

    // The member messages read without a channel too.
    let mut list = MemberList::new();
    assert_eq!(list.members(), None);
    let removed =
        MultizoneMessage::from_payload("DEVICE_REMOVED", &payload(json!({"deviceId": MEMBER})));
    assert!(
        !list.apply(&removed),
        "a removal before any list knows nothing"
    );
    assert_eq!(list.members(), None, "still shown without members");
    let status = MultizoneMessage::from_payload(
        "MULTIZONE_STATUS",
        &payload(json!({"status": {"devices": members_json()}})),
    );
    assert!(list.apply(&status));
    assert!(!list.apply(&status), "the same list is no change");
    assert_eq!(list.members().expect("known").len(), 2);
}

// covers: C95
#[test]
fn a_member_matches_a_discovered_id_in_either_uuid_form() {
    let mut list = DeviceList::new();
    list.apply(record("Kitchen", MEMBER, "Kitchen", DEVICE_PORT, v4(10)));
    list.apply(record("Downstairs", GROUP, "Downstairs", 32_187, v4(10)));
    let member = |device_id: &str| GroupMember {
        device_id: device_id.to_owned(),
        id: DeviceId::parse(device_id),
        name: Some("Kitchen".to_owned()),
    };
    for form in [MEMBER, MEMBER_DASHED, &MEMBER.to_ascii_uppercase()] {
        let entry = list.member(&member(form)).expect("matched");
        assert_eq!(entry.key, member_key(), "{form}");
    }
    assert!(member(MEMBER_DASHED).is(MEMBER));
    // Not discovered, or not a UUID: shown by name alone.
    assert!(list.member(&member(OTHER_MEMBER)).is_none());
    assert!(list.member(&member("kitchen-speaker")).is_none());
    // A group never stands in for a member.
    assert!(list.member(&member(GROUP)).is_none());
}

// covers: C96
#[test]
fn other_multizone_messages_are_logged_once_and_change_nothing() {
    let leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    let (mut session, _records, _feed) = start_group(&leader);
    run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    for kind in [
        "PLAYBACK_SESSION_UPDATED",
        "CASTING_GROUPS",
        "PLAYBACK_SESSION_UPDATED",
        "SOMETHING_NEW",
    ] {
        leader.send(multizone(kind, json!({"groupId": GROUP})));
    }
    // A member message without its fields is another unknown.
    leader.send(multizone("DEVICE_ADDED", json!({})));
    let seen = drain(&mut session, Duration::from_millis(800));
    let logged: Vec<&str> = seen
        .iter()
        .filter_map(|event| match event {
            SessionEvent::GroupMessage(kind) => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        logged,
        [
            "PLAYBACK_SESSION_UPDATED",
            "CASTING_GROUPS",
            "SOMETHING_NEW",
            "DEVICE_ADDED"
        ]
    );
    assert!(last_members(&seen).is_none(), "the list did not change");
    let sent: Vec<Value> = leader
        .received()
        .into_iter()
        .filter(|(_, namespace, _)| namespace == NS_MULTIZONE)
        .map(|(_, _, payload)| payload)
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["type"], "GET_STATUS");
    session.stop();
    leader.join();
}

// covers: C97
#[test]
fn a_leader_handover_reconnects_to_the_new_records_and_relaunches() {
    let old_leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    let new_leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    let fingerprints = Fingerprints {
        id: DeviceId([0x6A; 16]),
        book: Arc::new(Mutex::new(FingerprintBook::new())),
    };
    let (mut session, records, _feed) = start_group_with(&old_leader, Some(fingerprints));
    records.set_group(vec![new_leader.address]);
    old_leader.join();
    let seen = run_until(&mut session, Duration::from_secs(15), |event| {
        matches!(event, SessionEvent::Recovered { .. })
    });
    assert!(
        seen.iter()
            .any(|event| matches!(event, SessionEvent::Recovered { relaunched: true })),
        "{seen:?}"
    );
    // C21: another leader is another device, not a changed certificate.
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::CertificateChanged { .. })),
        "{seen:?}"
    );
    assert_eq!(new_leader.connection_count(), 1);
    assert_eq!(new_leader.payloads(NS_RECEIVER, "LAUNCH").len(), 1);
    assert_eq!(new_leader.payloads(NS_MEDIA, "LOAD").len(), 1);
    assert_eq!(new_leader.payloads(NS_MULTIZONE, "GET_STATUS").len(), 1);
    session.stop();
    new_leader.join();
}

// covers: C98
#[test]
fn a_membership_change_that_stops_the_group_ends_it_as_taken_over() {
    let leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    let (mut session, _records, _feed) = start_group(&leader);
    // An added member changes only the list.
    leader.send(multizone(
        "DEVICE_ADDED",
        json!({"device": {"deviceId": "c0ffee00c0ffee00c0ffee00c0ffee00", "name": "Attic"}}),
    ));
    let seen = run_until(
        &mut session,
        Duration::from_secs(3),
        |event| matches!(event, SessionEvent::Members(members) if members.len() == 3),
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::Recovering(_) | SessionEvent::Ended(_)))
    );
    // A removed member or an edited group stops the app.
    leader.send(Command::TakeOver);
    let seen = run_until(&mut session, Duration::from_secs(5), |event| {
        matches!(event, SessionEvent::Ended(_))
    });
    assert!(
        seen.iter()
            .any(|event| matches!(event, SessionEvent::Ended(SessionEnd::TakenOver))),
        "{seen:?}"
    );
    assert_eq!(
        leader.payloads(NS_RECEIVER, "LAUNCH").len(),
        1,
        "no relaunch"
    );
    leader.join();
}

// covers: C2, C99
#[test]
fn a_member_stays_a_target_of_its_own() {
    let mut list = DeviceList::new();
    list.apply(record("Kitchen", MEMBER, "Kitchen", DEVICE_PORT, v4(10)));
    list.apply(record("Downstairs", GROUP, "Downstairs", 32_187, v4(10)));
    let member = list.get(member_key()).expect("listed");
    assert!(!member.is_group());
    assert_eq!(member.addresses, [SocketAddr::new(v4(10), DEVICE_PORT)]);
    assert!(list.get(group_key()).expect("listed").is_group());

    // Within this sender's own stream, the member waits for the group.
    let leader = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let group_id = DeviceId::parse(GROUP).expect("uuid");
    let member_id = DeviceId::parse(MEMBER).expect("uuid");
    let (_writer, reader) = audio_ring(4_800);
    let target = CastTarget {
        id: group_id,
        addresses: vec![leader.address],
        group: Some(GroupRecords::new(vec![leader.address])),
        records: None,
    };
    host.start(target, 48_000, Metadata::default(), reader)
        .expect("started");
    let (_writer, reader) = audio_ring(4_800);
    let member_target = CastTarget {
        id: member_id,
        addresses: vec![leader.address],
        group: None,
        records: None,
    };
    assert_eq!(
        host.start(member_target, 48_000, Metadata::default(), reader),
        Err(StartRefused::Busy { playing: group_id })
    );
    host.stop();
    assert!(host.shutdown());
    leader.join();
}

// covers: C100
#[test]
fn the_group_volume_goes_to_the_groups_platform_only() {
    let leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    let (mut session, _records, _feed) = start_group(&leader);
    session.set_volume(0.7).expect("sent");
    run_until(
        &mut session,
        Duration::from_secs(3),
        |event| matches!(event, SessionEvent::Volume(volume) if (volume.level - 0.7).abs() < 1e-9),
    );
    let sets: Vec<(String, Value)> = leader
        .received()
        .into_iter()
        .filter(|(_, namespace, payload)| {
            namespace == NS_RECEIVER && payload["type"] == "SET_VOLUME"
        })
        .map(|(destination, _, payload)| (destination, payload))
        .collect();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].0, PLATFORM);
    assert_eq!(leader.connection_count(), 1, "no channel to a member");
    session.stop();
    leader.join();
}

// covers: C101, C102
#[test]
fn a_groups_offset_is_its_own() {
    let group = DeviceId::parse(GROUP).expect("uuid");
    let member = DeviceId::parse(MEMBER).expect("uuid");
    let unset = DeviceId::parse(OTHER_MEMBER).expect("uuid");
    let mut memory = DeviceMemory::default();
    memory.set_offset(member, Duration::from_millis(1_800));
    assert_eq!(memory.offset(&group), None, "not seeded from a member");
    memory.set_offset(group, Duration::from_millis(2_400));
    assert_eq!(memory.offset(&member), Some(Duration::from_millis(1_800)));
    assert_eq!(memory.offset(&unset), None, "not seeded from the group");
    let stored = DeviceMemory::from_json(&memory.to_json());
    assert_eq!(stored.offset(&group), Some(Duration::from_millis(2_400)));
    assert_eq!(stored.offset(&member), Some(Duration::from_millis(1_800)));
    assert_eq!(stored.offset(&unset), None);
}

// covers: C103
#[test]
fn a_group_keeps_one_stream_response_per_address() {
    let (a, b, c, d) = (v4(1), v4(2), v4(3), v4(4));
    let block = vec![0u8; 3_840];

    // A single device: any new GET ends the older response (C61).
    let mut single = Responses::new(96_000);
    let (first, restart) = single.open(a);
    assert!(restart);
    let (_, restart) = single.open(b);
    assert!(restart, "nothing else was left open");
    assert_eq!(single.pop(first), Popped::Ended);
    assert_eq!(single.addresses(), [b]);

    // A group: one per address, the oldest gone beyond the limit.
    let mut group = Responses::new(96_000);
    group.set_group_limit(3);
    let (leader, restart) = group.open(a);
    assert!(restart);
    let (member, restart) = group.open(b);
    assert!(!restart, "the open response keeps its place in the audio");
    group.push(&block);
    assert!(matches!(
        group.pop(leader),
        Popped::Block { primary: true, .. }
    ));
    assert!(matches!(
        group.pop(member),
        Popped::Block { primary: false, .. }
    ));
    assert_eq!(group.pop(member), Popped::Empty);
    let (again, _) = group.open(b);
    assert_eq!(group.pop(member), Popped::Ended, "only its own address");
    assert_eq!(group.addresses(), [a, b]);
    group.open(c);
    group.open(d);
    assert_eq!(group.pop(leader), Popped::Ended, "at most members plus one");
    assert_eq!(group.addresses(), [b, c, d]);
    group.close(again);
    assert_eq!(group.addresses(), [c, d]);

    // The server answers what the group's records and members hold at the
    // time of each request, with nothing for the session to refresh.
    let local = IpAddr::from([127, 0, 0, 1]);
    let (writer, reader) = audio_ring(48_000);
    let config = StreamConfig {
        local_ip: local,
        allowed: vec![v4(9)],
        format: StreamFormat::wav(48_000).expect("rate"),
    };
    let server = StreamServer::start(config, reader).expect("starts");
    server.set_group_limit(3);
    let records = GroupRecords::new(vec![SocketAddr::new(v4(9), 32_187)]);
    server.follow_group(records.clone());
    let (address, path) = http::split_url(&server.url()).expect("url");
    let get = || {
        let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
        http::request(address, &request).expect("connects")
    };
    let mut refused = get();
    let (head, _) = http::read_response(&mut refused, Duration::ZERO);
    assert_eq!(http::status(&head), 403);
    records.set_group(vec![SocketAddr::new(local, 32_187)]);
    let _feed = feed(writer);
    let mut first = get();
    let (head, _) = http::read_response(&mut first, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    // A second GET from the same address ends the first.
    let mut second = get();
    let (head, _) = http::read_response(&mut second, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    let until = Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    while Instant::now() < until
        && !seen.contains(&ServerEvent::ResponseEnded(ResponseEnd::Replaced))
    {
        seen.extend(std::iter::from_fn(|| server.poll_event()));
        thread::sleep(Duration::from_millis(20));
    }
    assert!(seen.contains(&ServerEvent::ResponseEnded(ResponseEnd::Replaced)));
    records.set_group(vec![SocketAddr::new(v4(10), 32_187)]);
    records.set_members(vec![local]);
    let mut member = get();
    let (head, _) = http::read_response(&mut member, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    server.stop();
}

#[test]
fn an_address_the_group_dropped_gets_403() {
    // A group's session leaves the static list empty; the leader is seeded
    // here so the test shows the records alone decide after a handover (C97).
    let local = IpAddr::from([127, 0, 0, 1]);
    let (writer, reader) = audio_ring(48_000);
    let config = StreamConfig {
        local_ip: local,
        allowed: vec![local],
        format: StreamFormat::wav(48_000).expect("rate"),
    };
    let server = StreamServer::start(config, reader).expect("starts");
    server.set_group_limit(1);
    let records = GroupRecords::new(vec![SocketAddr::new(local, 32_187)]);
    server.follow_group(records.clone());
    let (address, path) = http::split_url(&server.url()).expect("url");
    let get = || {
        let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
        http::request(address, &request).expect("connects")
    };
    let _feed = feed(writer);
    let mut leader = get();
    let (head, _) = http::read_response(&mut leader, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    records.set_group(vec![SocketAddr::new(v4(10), 32_187)]);
    let mut dropped = get();
    let (head, _) = http::read_response(&mut dropped, Duration::ZERO);
    assert_eq!(
        http::status(&head),
        403,
        "the old leader is not the group's now"
    );
    server.stop();
}

// covers: C94, C95
#[test]
fn mid_session_member_messages_match_by_id_and_an_empty_status_is_a_known_list() {
    let leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    let (mut session, _records, _feed) = start_group(&leader);
    let mut seen = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(last_members(&seen).expect("members"), ["Kitchen", "Porch"]);

    // The list holds the dashed form; the undashed one refreshes that entry.
    leader.send(multizone(
        "DEVICE_ADDED",
        json!({"device": {"deviceId": MEMBER, "name": "Kitchen Speaker"}}),
    ));
    let changed = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(
        last_members(&changed).expect("members"),
        ["Kitchen Speaker", "Porch"]
    );
    seen.extend(changed);

    leader.send(multizone(
        "DEVICE_REMOVED",
        json!({"deviceId": "c0ffee00c0ffee00c0ffee00c0ffee00"}),
    ));
    let quiet = drain(&mut session, Duration::from_millis(600));
    assert!(
        last_members(&quiet).is_none(),
        "removing a stranger changes nothing: {quiet:?}"
    );
    seen.extend(quiet);

    leader.send(multizone(
        "DEVICE_REMOVED",
        json!({"deviceId": "0B1C2D3E-4F5A-46B7-A8C9-D0E1F2A3B4C5"}),
    ));
    let changed = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(
        last_members(&changed).expect("members"),
        ["Kitchen Speaker"]
    );
    seen.extend(changed);

    leader.send(multizone(
        "MULTIZONE_STATUS",
        json!({"status": {"devices": []}}),
    ));
    let changed = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Members(_))
    });
    assert_eq!(
        last_members(&changed).expect("a known, empty list"),
        Vec::<String>::new()
    );
    seen.extend(changed);

    assert!(!session.is_ended());
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::Recovering(_) | SessionEvent::Ended(_))),
        "{seen:?}"
    );
    assert_eq!(leader.connection_count(), 1);
    session.stop();
    leader.join();
}

// covers: C94, C96
#[test]
fn member_messages_without_the_fields_they_need_change_nothing() {
    let mut list = MemberList::new();
    let status = MultizoneMessage::from_payload(
        "MULTIZONE_STATUS",
        &payload(json!({"status": {"devices": [
            {"name": "No id"},
            {"deviceId": MEMBER, "name": "Kitchen"},
        ]}})),
    );
    assert!(list.apply(&status));
    let known = list.members().expect("known");
    assert_eq!(known.len(), 1, "an entry without deviceId is no member");
    assert!(known[0].is(MEMBER_DASHED));

    for (kind, fields) in [
        ("DEVICE_ADDED", json!({"name": "Attic"})),
        ("DEVICE_UPDATED", json!({"device": {"name": "Attic"}})),
        ("DEVICE_REMOVED", json!({"deviceId": 7})),
        ("MULTIZONE_STATUS", json!({"status": {}})),
    ] {
        let message = MultizoneMessage::from_payload(kind, &payload(fields));
        assert_eq!(message, MultizoneMessage::Other(kind.to_owned()), "{kind}");
        assert!(!list.apply(&message), "{kind}");
    }
    assert_eq!(list.members().expect("known").len(), 1);

    // An update for a member the list lacks adds it.
    let updated = MultizoneMessage::from_payload(
        "DEVICE_UPDATED",
        &payload(json!({"device": {"deviceId": OTHER_MEMBER, "name": "Porch"}})),
    );
    assert!(list.apply(&updated));
    assert_eq!(list.members().expect("known").len(), 2);
}

// covers: C93, C97
#[test]
fn records_that_move_under_a_live_channel_open_no_second_channel() {
    let leader = FakeDevice::start(Behavior {
        members: Some(members_json()),
        ..Behavior::default()
    });
    let elsewhere = FakeDevice::start(Behavior::default());
    let (mut session, records, _feed) = start_group(&leader);
    records.set_group(vec![elsewhere.address]);
    let seen = drain(&mut session, Duration::from_millis(1_500));
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::Recovering(_) | SessionEvent::Ended(_))),
        "{seen:?}"
    );
    assert!(!session.is_ended());
    assert_eq!(leader.connection_count(), 1);
    assert_eq!(
        elsewhere.connection_count(),
        0,
        "no channel until a reconnect"
    );
    session.stop();
    leader.join();
    elsewhere.join();
}

// covers: C103
#[test]
fn the_response_limit_follows_the_member_count() {
    let (a, b, c, d) = (v4(1), v4(2), v4(3), v4(4));
    let mut responses = Responses::new(96_000);
    responses.set_group_limit(3);
    let (first, _) = responses.open(a);
    responses.open(b);
    responses.open(c);
    assert_eq!(responses.addresses(), [a, b, c]);

    // Members left: the next GET keeps the response count within the new limit.
    responses.set_group_limit(1);
    let (kept, _) = responses.open(d);
    assert_eq!(responses.addresses(), [d]);
    assert_eq!(responses.pop(first), Popped::Ended);

    // Members joined: new addresses no longer end the older responses.
    responses.set_group_limit(3);
    responses.open(a);
    responses.open(b);
    assert_eq!(responses.addresses(), [d, a, b]);
    assert_ne!(responses.pop(kept), Popped::Ended);
}

// covers: C58, C103
#[test]
fn the_stream_answers_members_only_while_they_are_members() {
    let local = IpAddr::from([127, 0, 0, 1]);
    let (writer, reader) = audio_ring(48_000);
    let config = StreamConfig {
        local_ip: local,
        allowed: vec![local],
        format: StreamFormat::wav(48_000).expect("rate"),
    };
    let server = StreamServer::start(config, reader).expect("starts");
    server.set_group_limit(3);
    let records = GroupRecords::new(vec![SocketAddr::new(v4(10), 32_187)]);
    records.set_members(vec![v4(11), local]);
    server.follow_group(records.clone());
    let (address, path) = http::split_url(&server.url()).expect("url");
    let ask = |method: &str| {
        let request = format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n");
        let mut stream = http::request(address, &request).expect("connects");
        let (head, _) = http::read_response(&mut stream, Duration::ZERO);
        http::status(&head)
    };
    let _feed = feed(writer);
    assert_eq!(ask("GET"), 200, "a member's address");

    // The member left the group: the static list no longer counts.
    records.set_members(vec![v4(11)]);
    assert_eq!(ask("GET"), 403);
    assert_eq!(ask("HEAD"), 403);

    // Any of the group's own addresses is served.
    records.set_group(vec![
        SocketAddr::new(v4(10), 32_187),
        SocketAddr::new(local, 32_187),
    ]);
    assert_eq!(ask("GET"), 200, "the group's second address");
    server.stop();
}
