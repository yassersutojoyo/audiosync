//! The bad-network switch for test runs (P253): the value's format, the
//! seeded drop and delay of datagrams, and a sender over loopback with the
//! switch on and off.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use audiosync_airplay::bad_network::{self, BadNetwork, Fate, Malformed, Shaper};
use audiosync_airplay::sender::{Sender, SenderConfig, SpeakerOptions, SpeakerState};
use audiosync_airplay::volume::Volume;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};

fn on(loss_percent: f64, delay_ms: u32, spread_ms: u32) -> BadNetwork {
    BadNetwork {
        loss_percent,
        delay_ms,
        spread_ms,
    }
}

// covers: P253
#[test]
fn the_value_is_read_whole_or_not_at_all() {
    let valid: &[(&str, Option<BadNetwork>)] = &[
        ("loss=3,delay=85,spread=65", Some(on(3.0, 85, 65))),
        (" spread=65 , loss=3 ,delay=85 ", Some(on(3.0, 85, 65))),
        ("loss=0.5", Some(on(0.5, 0, 0))),
        ("loss=50", Some(on(50.0, 0, 0))),
        ("delay=1000", Some(on(0.0, 1000, 0))),
        ("delay=500,spread=500", Some(on(0.0, 500, 500))),
        ("spread=20", Some(on(0.0, 0, 20))),
        ("", None),
        ("   ", None),
        ("loss=0,delay=0,spread=0", None),
        ("loss=0.0", None),
    ];
    for (value, wanted) in valid {
        assert_eq!(bad_network::parse(value), Ok(*wanted), "{value:?}");
    }
    let malformed: &[(&str, Malformed)] = &[
        ("loss=51", Malformed::OutOfRange("loss".into())),
        ("loss=50.5", Malformed::OutOfRange("loss".into())),
        ("delay=1001", Malformed::OutOfRange("delay".into())),
        (
            "delay=600,spread=401",
            Malformed::OutOfRange("spread".into()),
        ),
        ("loss=-1", Malformed::NotANumber("loss".into())),
        ("loss=+3", Malformed::NotANumber("loss".into())),
        ("loss=3%", Malformed::NotANumber("loss".into())),
        ("loss=nan", Malformed::NotANumber("loss".into())),
        ("loss=1e1", Malformed::NotANumber("loss".into())),
        ("loss=.", Malformed::NotANumber("loss".into())),
        ("loss=", Malformed::NotANumber("loss".into())),
        ("delay=8.5", Malformed::NotANumber("delay".into())),
        ("spread=x", Malformed::NotANumber("spread".into())),
        ("loss=3,loss=4", Malformed::Repeated("loss".into())),
        ("delay=1, delay=1", Malformed::Repeated("delay".into())),
        ("jitter=5", Malformed::UnknownKey("jitter".into())),
        ("Loss=3", Malformed::UnknownKey("Loss".into())),
        ("loss = 3", Malformed::UnknownKey("loss ".into())),
        ("loss= 3", Malformed::NotANumber("loss".into())),
        ("loss=3,", Malformed::NotAPair(String::new())),
        ("loss 3", Malformed::NotAPair("loss 3".into())),
        ("loss=3,,delay=1", Malformed::NotAPair(String::new())),
    ];
    for (value, wanted) in malformed {
        assert_eq!(bad_network::parse(value).as_ref(), Err(wanted), "{value:?}");
    }
}

// covers: P253
#[test]
fn the_host_logs_one_line_for_the_value() {
    let read = |value| bad_network::switch(value);
    assert_eq!(read(None), (None, "bad network: off".to_owned()));
    assert_eq!(read(Some("")), (None, "bad network: off".to_owned()));
    assert_eq!(
        read(Some("loss=3,delay=85,spread=65")),
        (
            Some(on(3.0, 85, 65)),
            "bad network: loss 3%, delay 85 ms, spread 65 ms".to_owned()
        )
    );
    assert_eq!(
        read(Some("loss=0.5")).1,
        "bad network: loss 0.5%, delay 0 ms, spread 0 ms"
    );
    let (config, line) = read(Some("loss=3,jitter=5"));
    assert_eq!(config, None);
    assert!(line.starts_with("bad network: off, "), "{line}");
    assert!(line.contains("jitter"), "{line}");
    assert!(line.contains(bad_network::BAD_NETWORK_ENV), "{line}");
}

// covers: P253
#[test]
fn a_seeded_switch_drops_the_share_it_states() {
    let start = Instant::now();
    let mut shaper: Shaper<u8, u32> = Shaper::new(on(3.0, 0, 0), 7);
    let mut dropped = 0;
    for n in 0..100_000u32 {
        match shaper.offer(0, n, start) {
            Fate::Dropped => dropped += 1,
            Fate::Send(item) => assert_eq!(item, n),
            Fate::Delayed => panic!("no delay was asked for"),
        }
    }
    assert!((2_700..=3_300).contains(&dropped), "{dropped}");
    assert_eq!(shaper.next_due(), None);
    // The same seed draws the same datagrams.
    let fates = |seed| {
        let mut shaper: Shaper<u8, u32> = Shaper::new(on(10.0, 0, 0), seed);
        (0..1000)
            .map(|n| matches!(shaper.offer(0, n, start), Fate::Dropped))
            .collect::<Vec<_>>()
    };
    assert_eq!(fates(11), fates(11));
    assert_ne!(fates(11), fates(12));
}

// covers: P253
#[test]
fn a_seeded_switch_delays_within_its_window_and_keeps_order() {
    let start = Instant::now();
    let tick = Duration::from_millis(8);
    let mut shaper: Shaper<u8, (u8, u32)> = Shaper::new(on(3.0, 85, 65), 99);
    let mut offered: BTreeMap<(u8, u32), Instant> = BTreeMap::new();
    let mut released: Vec<((u8, u32), Instant)> = Vec::new();
    let mut dropped = 0;
    let mut sent_at_once = 0;
    let mut now = start;
    for n in 0..20_000u32 {
        now = start + tick * n;
        while let Some(item) = shaper.pop_due(now) {
            released.push((item, now));
        }
        // Audio to one port and resends to another of the same speaker.
        let path = (n % 3 == 0) as u8;
        offered.insert((path, n), now);
        match shaper.offer(path, (path, n), now) {
            Fate::Dropped => dropped += 1,
            Fate::Send(item) => {
                sent_at_once += 1;
                released.push((item, now));
            }
            Fate::Delayed => {}
        }
    }
    while let Some(at) = shaper.next_due() {
        now = now.max(at);
        while let Some(item) = shaper.pop_due(now) {
            released.push((item, now));
        }
    }
    assert!((500..=700).contains(&dropped), "{dropped}");
    assert_eq!(sent_at_once, 0, "every delay is at least 20 ms");
    assert_eq!(released.len() + dropped, 20_000);
    for &(item, at) in &released {
        let waited = at - offered[&item];
        // The order rule waits only for one offered earlier, so never past
        // 150 ms; the pop runs once a tick.
        assert!(waited >= Duration::from_millis(20), "{waited:?}");
        assert!(waited <= Duration::from_millis(150) + tick, "{waited:?}");
    }
    // Datagrams far enough apart that the order rule never binds wait the
    // drawn delay, evenly spread around 85 ms.
    let mut sparse: Shaper<u8, u32> = Shaper::new(on(0.0, 85, 65), 5);
    let mut total = Duration::ZERO;
    let mut shortest = Duration::MAX;
    let mut longest = Duration::ZERO;
    for n in 0..2000u32 {
        let at = start + Duration::from_millis(300) * n;
        assert_eq!(sparse.offer(0, n, at), Fate::Delayed);
        let waited = sparse.next_due().expect("held") - at;
        assert_eq!(sparse.pop_due(at + waited), Some(n));
        total += waited;
        shortest = shortest.min(waited);
        longest = longest.max(waited);
    }
    let mean = total / 2000;
    assert!(
        mean >= Duration::from_millis(81) && mean <= Duration::from_millis(89),
        "{mean:?}"
    );
    assert!(shortest < Duration::from_millis(25), "{shortest:?}");
    assert!(longest > Duration::from_millis(145), "{longest:?}");
    for path in 0..2u8 {
        let order: Vec<u32> = released
            .iter()
            .filter(|((p, _), _)| *p == path)
            .map(|((_, n), _)| *n)
            .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "path {path} reordered"
        );
    }
}

const PCM_TXT: &[&str] = &[
    "txtvers=1",
    "ch=2",
    "cn=0",
    "et=0",
    "sr=44100",
    "ss=16",
    "tp=UDP",
    "md=0,1,2",
];

const LONG: Duration = Duration::from_secs(15);

/// Streams to a loopback receiver until it got `packets` audio packets, and
/// gives the receiver, the sender's counters and the sequence numbers seen.
fn stream(config: SenderConfig, packets: usize) -> (NetReceiver, Sender, Vec<u16>, u64, u64, u64) {
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    let host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    let counters = sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || receiver.log().audio.len() >= packets));
    let seqs: Vec<u16> = receiver
        .log()
        .audio
        .iter()
        .map(|packet| packet.header.seq)
        .collect();
    let sent = counters.packets_sent();
    let dropped = counters.bad_network_dropped();
    let delayed = counters.bad_network_delayed();
    drop(host);
    (receiver, sender, seqs, sent, dropped, delayed)
}

// covers: P253
#[test]
fn without_the_switch_every_packet_leaves_untouched() {
    let _serial = serial();
    assert_eq!(SenderConfig::default().bad_network, None);
    let (_receiver, sender, seqs, _, dropped, delayed) = stream(SenderConfig::default(), 300);
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)),
        "a packet went missing on loopback"
    );
    assert_eq!((dropped, delayed), (0, 0));
    sender.shutdown();
}

// covers: P253
#[test]
fn the_switch_drops_packets_that_stay_resendable() {
    let _serial = serial();
    let config = SenderConfig {
        bad_network: Some(on(30.0, 0, 0)),
        ..SenderConfig::default()
    };
    let (receiver, sender, seqs, sent, dropped, delayed) = stream(config, 300);
    assert_eq!(delayed, 0);
    // Dropped packets still count as sent.
    let share = dropped as f64 / sent as f64;
    assert!((0.15..=0.45).contains(&share), "{dropped} of {sent}");
    let missing: Vec<u16> = seqs
        .windows(2)
        .flat_map(|w| (1..w[1].wrapping_sub(w[0])).map(move |k| w[0].wrapping_add(k)))
        .collect();
    assert!(missing.len() >= 20, "{missing:?}");
    let before = receiver.log().resends.len();
    for &seq in &missing[missing.len() - 20..] {
        receiver.request_resend(seq, 1);
    }
    // Resend answers go through the switch too, so some are lost.
    assert!(
        wait_until(LONG, || receiver.log().resends.len() >= before + 5),
        "{} resends",
        receiver.log().resends.len() - before
    );
    sender.shutdown();
}

// covers: P253
#[test]
fn the_switch_delays_packets_in_order() {
    let _serial = serial();
    let config = SenderConfig {
        bad_network: Some(on(0.0, 60, 40)),
        ..SenderConfig::default()
    };
    let (_receiver, sender, seqs, _, dropped, delayed) = stream(config, 300);
    assert_eq!(dropped, 0);
    assert!(delayed >= 300, "{delayed}");
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)),
        "the delay reordered or lost packets"
    );
    sender.shutdown();
}
