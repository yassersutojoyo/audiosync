//! Several AirPlay 2 receivers at once: one PTP timeline the sender leads,
//! and the SETPEERS lists and updates that keep each receiver told who else
//! is on it (P205 to P210). The driver's places and waits are in
//! `target_sessions.rs`.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use audiosync_airplay::ap2::{Ap2Config, Ap2Event, PtpPeer, Timing};
use audiosync_airplay::clock::SenderClock;
use audiosync_airplay::error::Warning;
use audiosync_airplay::ptp::{
    DELAY_REQ_BYTES, MessageType, PtpPorts, PtpService, PtpTransmit, SYNC_INTERVAL,
};
use plist::Value;

use common::ap2::{Ap2Rig, FakeAp2, ap2_config, dict, int, plist, read_value};
use common::{RECEIVER, SENDER};

const IDENTITY: [u8; 8] = [0xFF, 0xFF, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F];
const FIRST: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 31));
const SECOND: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 32));
const THIRD: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 33));

/// Everything the service queued, each Sync reported sent so its
/// Follow_Up follows, as the driver does.
fn drain(service: &mut PtpService) -> Vec<PtpTransmit> {
    let mut sent = Vec::new();
    while let Some(transmit) = service.poll_transmit() {
        if transmit.kind == MessageType::Sync {
            service.sync_sent_to(transmit.sequence_id, transmit.to.ip(), Instant::now());
        }
        sent.push(transmit);
    }
    sent
}

fn sent_to(sent: &[PtpTransmit], peer: IpAddr, kind: MessageType) -> Vec<&PtpTransmit> {
    sent.iter()
        .filter(|transmit| transmit.to.ip() == peer && transmit.kind == kind)
        .collect()
}

fn clock_identity_of(transmit: &PtpTransmit) -> [u8; 8] {
    transmit.bytes[20..28].try_into().expect("eight bytes")
}

/// Runs the service's timers from `from` for `span`, in 5 ms steps.
fn run(service: &mut PtpService, from: Instant, span: Duration) -> Vec<PtpTransmit> {
    let mut sent = Vec::new();
    let mut now = from;
    while now <= from + span {
        service.handle_timeout(now);
        sent.extend(drain(service));
        now += Duration::from_millis(5);
    }
    sent
}

fn delay_req(sequence_id: u16) -> Vec<u8> {
    let mut bytes = vec![0u8; DELAY_REQ_BYTES];
    bytes[0] = MessageType::DelayReq as u8;
    bytes[1] = 2;
    bytes[20..30].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 0, 1]);
    bytes[30..32].copy_from_slice(&sequence_id.to_be_bytes());
    bytes
}

/// The sender's own Announce with its grandmaster priority1 moved by
/// `by`: below the sender's ranks first (IEEE 1588-2008 9.3.4).
fn announce_with_priority(own: &PtpTransmit, by: i16) -> Vec<u8> {
    let mut bytes = own.bytes.clone();
    let at = 34 + 10 + 3;
    bytes[at] = (i16::from(bytes[at]) + by) as u8;
    // Another clock's grandmaster identity.
    bytes[at + 6..at + 14].copy_from_slice(&[0x00, 0x11, 0x22, 0xFF, 0xFE, 0x33, 0x44, 0x55]);
    bytes
}

// covers: P205, P206
#[test]
fn ap2_one_ptp_timeline() {
    let origin = Instant::now();
    let mut service = PtpService::new(SenderClock::new(origin), IDENTITY, PtpPorts::default());
    service.start(SocketAddr::new(FIRST, 7000), origin);
    let later = origin + Duration::from_millis(40);
    service.start(SocketAddr::new(SECOND, 7000), later);
    let mut sent = drain(&mut service);
    sent.extend(run(&mut service, later, Duration::from_millis(1100)));
    for peer in [FIRST, SECOND] {
        for kind in [
            MessageType::Announce,
            MessageType::Sync,
            MessageType::FollowUp,
        ] {
            let to_peer = sent_to(&sent, peer, kind);
            assert!(!to_peer.is_empty(), "{peer} {kind:?}");
            assert!(
                to_peer
                    .iter()
                    .all(|transmit| clock_identity_of(transmit) == IDENTITY),
                "one clock identity"
            );
            let ids: Vec<u16> = to_peer
                .iter()
                .map(|transmit| transmit.sequence_id)
                .collect();
            let expected: Vec<u16> = (0..ids.len() as u16).collect();
            assert_eq!(ids, expected, "{peer} {kind:?}: its own sequenceIds from 0");
        }
    }

    // A third receiver starts from 0 and moves no other schedule.
    let first_syncs = |sent: &[PtpTransmit]| sent_to(sent, FIRST, MessageType::Sync).len();
    let mut before = PtpService::new(SenderClock::new(origin), IDENTITY, PtpPorts::default());
    before.start(SocketAddr::new(FIRST, 7000), origin);
    let alone = {
        let mut sent = drain(&mut before);
        sent.extend(run(&mut before, origin, Duration::from_millis(2000)));
        first_syncs(&sent)
    };
    let mut joined = PtpService::new(SenderClock::new(origin), IDENTITY, PtpPorts::default());
    joined.start(SocketAddr::new(FIRST, 7000), origin);
    let mut sent = drain(&mut joined);
    sent.extend(run(&mut joined, origin, Duration::from_millis(700)));
    let join_at = origin + Duration::from_millis(705);
    joined.start(SocketAddr::new(THIRD, 7000), join_at);
    let third = drain(&mut joined);
    assert_eq!(
        sent_to(&third, THIRD, MessageType::Announce)[0].sequence_id,
        0
    );
    sent.extend(third);
    sent.extend(run(&mut joined, join_at, Duration::from_millis(1295)));
    assert_eq!(
        first_syncs(&sent),
        alone,
        "the first receiver's Sync times stay"
    );
    assert!(SYNC_INTERVAL < Duration::from_millis(700));

    // A Delay_Req from each served receiver is answered.
    for (peer, sequence_id) in [(FIRST, 5), (THIRD, 9)] {
        joined.handle_message(&delay_req(sequence_id), SocketAddr::new(peer, 319), join_at);
        let answers = drain(&mut joined);
        assert!(
            answers
                .iter()
                .any(|transmit| transmit.kind == MessageType::DelayResp
                    && transmit.to.ip() == peer
                    && transmit.sequence_id == sequence_id),
            "{peer}: {answers:?}"
        );
    }

    // P206: a receiver that keeps announcing a grandmaster ahead of the
    // sender is named once; one that doesn't outrank, never.
    let own = sent_to(&sent, FIRST, MessageType::Announce)[0].clone();
    joined.handle_message(
        &announce_with_priority(&own, 1),
        SocketAddr::new(FIRST, 320),
        join_at,
    );
    assert_eq!(joined.poll_outranked(), None);
    for (second, sequence_id) in [(0u64, 7u16), (1, 8), (2, 9)] {
        let mut better = announce_with_priority(&own, -1);
        better[30..32].copy_from_slice(&sequence_id.to_be_bytes());
        joined.handle_message(
            &better,
            SocketAddr::new(FIRST, 320),
            join_at + Duration::from_secs(second),
        );
    }
    assert_eq!(joined.poll_outranked(), Some(FIRST));
    assert_eq!(joined.poll_outranked(), None);
    let stranger = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 99));
    for (second, sequence_id) in [(3u64, 1u16), (4, 2)] {
        let mut better = announce_with_priority(&own, -1);
        better[30..32].copy_from_slice(&sequence_id.to_be_bytes());
        joined.handle_message(
            &better,
            SocketAddr::new(stranger, 320),
            join_at + Duration::from_secs(second),
        );
    }
    assert_eq!(joined.poll_outranked(), None, "only served receivers count");
}

/// An Announce a receiver sends naming its own clock `identity` as
/// grandmaster with `priority1` and `clock_class`, the other dataset fields
/// as the sender's (P160), at logMessageInterval 0.
fn receiver_announce(
    identity: [u8; 8],
    priority1: u8,
    clock_class: u8,
    sequence_id: u16,
) -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[0] = 0x10 | MessageType::Announce as u8;
    bytes[1] = 2;
    bytes[2..4].copy_from_slice(&64u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&0x0408u16.to_be_bytes());
    bytes[20..28].copy_from_slice(&identity);
    bytes[28..30].copy_from_slice(&1u16.to_be_bytes());
    bytes[30..32].copy_from_slice(&sequence_id.to_be_bytes());
    bytes[32] = 5;
    let at = 34 + 10 + 3;
    bytes[at] = priority1;
    bytes[at + 1..at + 5].copy_from_slice(&[clock_class, 0x21, 0x43, 0x6A]);
    bytes[at + 5] = 128;
    bytes[at + 6..at + 14].copy_from_slice(&identity);
    bytes[at + 16] = 0xA0;
    bytes
}

// An open receiver's PTP helper sends a short mastership claim when told
// to follow a clock it has heard no Follow_Up from yet, then follows it;
// IEEE 1588-2008 9.3.2.5 qualifies a foreign master only after 2 Announces
// within 4 announce intervals.
// covers: P206
#[test]
fn ap2_a_brief_mastership_claim_is_no_other_grandmaster() {
    let rx = [0x02, 0x15, 0x5D, 0xFF, 0xFE, 0xED, 0x01, 0x71];
    let origin = Instant::now();
    let mut service = PtpService::new(SenderClock::new(origin), IDENTITY, PtpPorts::default());
    service.start(SocketAddr::new(FIRST, 7000), origin);
    drain(&mut service);
    let from = SocketAddr::new(FIRST, 320);
    let at = |ms: u64| origin + Duration::from_millis(ms);

    // One claim, then the receiver withdraws (slave-only values).
    service.handle_message(&receiver_announce(rx, 127, 6, 0), from, at(1000));
    service.handle_message(&receiver_announce(rx, 255, 255, 1), from, at(1010));
    service.handle_timeout(at(6000));
    drain(&mut service);
    assert_eq!(service.poll_outranked(), None, "a lone claim");

    // Two claims in one burst, then silence.
    service.handle_message(&receiver_announce(rx, 127, 6, 2), from, at(7000));
    service.handle_message(&receiver_announce(rx, 127, 6, 3), from, at(7005));
    assert_eq!(service.poll_outranked(), None, "a burst is one claim");

    // A receiver that keeps announcing a better clock once a second leads
    // its own timeline: named once.
    for (n, ms) in [(4u16, 20_000u64), (5, 21_000), (6, 22_000)] {
        service.handle_message(&receiver_announce(rx, 127, 6, n), from, at(ms));
    }
    assert_eq!(service.poll_outranked(), Some(FIRST));
    assert_eq!(service.poll_outranked(), None);
}

fn ptp_config(peers: Vec<IpAddr>) -> Ap2Config {
    let mut config = ap2_config();
    config.ptp = Some(PtpPeer {
        clock_id: 0xFFFF_0000_0000_0001,
        id: "6f1e7c9a-3b2d-4c5e-8f90-a1b2c3d4e5f6".to_owned(),
    });
    config.timing = Timing::Ptp;
    config.peers = peers;
    config
}

fn ptp_fake() -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.info_body = plist(dict(&[
        ("features", int((1 << 38) | (1 << 41) | (1 << 48))),
        ("statusFlags", int(0x4)),
    ]));
    fake
}

fn list(addresses: &[IpAddr]) -> Option<Value> {
    Some(Value::Array(
        addresses
            .iter()
            .map(|ip| Value::String(ip.to_string()))
            .collect(),
    ))
}

fn last_peers(rig: &Ap2Rig) -> Option<Value> {
    read_value(&rig.fake.last("SETPEERS").expect("SETPEERS").body)
}

fn set_peers(rig: &mut Ap2Rig, peers: Vec<IpAddr>) {
    let now = rig.now;
    rig.session.set_peers(peers, now);
    rig.pump();
}

fn refusals(rig: &Ap2Rig) -> Vec<(Option<u16>, bool, bool)> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            Ap2Event::PeersRefused {
                status,
                added,
                first_accepted,
            } => Some((*status, *added, *first_accepted)),
            _ => None,
        })
        .collect()
}

fn warnings(rig: &Ap2Rig) -> usize {
    rig.events
        .iter()
        .filter(|event| matches!(event, Ap2Event::Warning(Warning::PeersUpdateRefused { .. })))
        .count()
}

// covers: P207
#[test]
fn ap2_setpeers_lists_every_ptp_receiver() {
    let rig = Ap2Rig::new(ptp_config(vec![FIRST, SECOND]), ptp_fake());
    assert!(rig.ready(), "{:?}", rig.keys());
    let keys = rig.keys();
    let record = keys
        .iter()
        .position(|key| *key == "RECORD")
        .expect("RECORD");
    let peers = keys
        .iter()
        .position(|key| *key == "SETPEERS")
        .expect("SETPEERS");
    assert!(
        peers > record,
        "the first SETPEERS follows RECORD's answer: {keys:?}"
    );
    assert_eq!(
        last_peers(&rig),
        list(&[RECEIVER, FIRST, SECOND, SENDER]),
        "the receiver, the other PTP receivers, then the sender"
    );
    assert!(rig.events.contains(&Ap2Event::Recorded));
}

// covers: P208, P209
#[test]
fn ap2_join_and_leave_mid_stream() {
    let mut rig = Ap2Rig::new(ptp_config(Vec::new()), ptp_fake());
    assert!(rig.ready());
    assert_eq!(rig.times_of("SETPEERS").len(), 1);
    let setups = rig.times_of("SETUP stream").len() + rig.times_of("SETUP session").len();
    let flushes = rig.times_of("FLUSH").len();

    // Two receivers join 500 ms apart: one update, within 1 s of the first.
    let start = rig.now;
    set_peers(&mut rig, vec![FIRST]);
    rig.advance(Duration::from_millis(500));
    set_peers(&mut rig, vec![FIRST, SECOND]);
    assert_eq!(
        rig.times_of("SETPEERS").len(),
        1,
        "nothing goes out at once"
    );
    rig.advance(Duration::from_millis(700));
    let times = rig.times_of("SETPEERS");
    assert_eq!(times.len(), 2, "two changes within 1 s make one update");
    assert!(
        times[1] - start <= Duration::from_millis(1100),
        "{:?}",
        times[1] - start
    );
    assert_eq!(last_peers(&rig), list(&[RECEIVER, FIRST, SECOND, SENDER]));
    assert_eq!(
        rig.times_of("SETUP stream").len() + rig.times_of("SETUP session").len(),
        setups
    );
    assert_eq!(
        rig.times_of("FLUSH").len(),
        flushes,
        "the others are not flushed"
    );

    // One leaves: the shorter list.
    set_peers(&mut rig, vec![SECOND]);
    rig.advance(Duration::from_millis(1200));
    assert_eq!(rig.times_of("SETPEERS").len(), 3);
    assert_eq!(last_peers(&rig), list(&[RECEIVER, SECOND, SENDER]));

    // An update left unanswered ends nothing.
    rig.fake.silent.push("SETPEERS".to_owned());
    set_peers(&mut rig, Vec::new());
    rig.advance(Duration::from_secs(12));
    assert!(rig.failure().is_none(), "{:?}", rig.failure());
    assert_eq!(refusals(&rig), [(None, false, true)]);
    assert!(rig.ready());
}

// covers: P210
#[test]
fn ap2_refused_peer_update() {
    let mut rig = Ap2Rig::new(ptp_config(Vec::new()), ptp_fake());
    assert!(rig.ready());
    rig.fake.status_once.insert("SETPEERS".to_owned(), 501);
    set_peers(&mut rig, vec![FIRST]);
    rig.advance(Duration::from_millis(1200));
    assert_eq!(
        refusals(&rig),
        [(Some(501), true, true)],
        "an update that added a receiver, after a first SETPEERS answered 2xx"
    );
    assert_eq!(warnings(&rig), 1);
    assert!(rig.failure().is_none());

    // A refused update that only removed one: the same session plays on,
    // and a second refusal gives no second warning.
    rig.fake.status_once.insert("SETPEERS".to_owned(), 501);
    set_peers(&mut rig, Vec::new());
    rig.advance(Duration::from_millis(1200));
    assert_eq!(
        refusals(&rig),
        [(Some(501), true, true), (Some(501), false, true)]
    );
    assert_eq!(warnings(&rig), 1);
    assert!(rig.failure().is_none());
    assert!(rig.ready());
}
