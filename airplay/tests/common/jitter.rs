//! A television behind a busy 2.4 GHz network, for the stream rig. Every
//! datagram to it waits in one first-in, first-out queue whose one-way delay
//! is half a round trip that changes every 100 ms: 20 to 150 ms, and now and
//! then 280 to 315 ms. Its resend requests take the same one-way delay back.
//! It asks for a resend of every packet still missing 50 ms before its play
//! time, one request per run found in each 20 ms check, as a receiver that
//! checks its buffer does; a packet that arrives after its play time is a
//! gap.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use audiosync_airplay::rtp::{
    HEADER_BYTES, RESEND_PREFIX_BYTES, ResendRequest, RtpHeader, SyncPacket,
};
use audiosync_airplay::stream::{PacketKind, SpeakerId};

use super::rig::{Rig, Rng};

const SEGMENT: f64 = 0.1;
const LEAD: f64 = 0.05;
const CHECK: f64 = 0.02;

/// An audio packet as the television got it.
#[derive(Debug, Clone)]
pub struct Heard {
    pub seq: u16,
    pub timestamp: u32,
    pub sent: f64,
    /// When its sync says it sounds.
    pub play: f64,
    /// When it or its first resend arrived, whichever came first.
    pub arrival: f64,
    pub payload: Vec<u8>,
    pub requested: bool,
}

impl Heard {
    pub fn late(&self) -> bool {
        self.arrival > self.play
    }
}

pub struct JitteryTv {
    pub id: SpeakerId,
    rng: Rng,
    /// The chance that a 100 ms stretch is a spike.
    spike_chance: f64,
    segment_end: f64,
    one_way: f64,
    /// When the queue's newest datagram arrives.
    queue_free: f64,
    seen: usize,
    sync: Option<SyncPacket>,
    pub packets: Vec<Heard>,
    by_seq: HashMap<u16, usize>,
    unchecked: usize,
    next_check: f64,
    /// Requests on their way: when each reaches the sender.
    on_the_way: VecDeque<(f64, ResendRequest)>,
    /// When each request reached the sender.
    pub asked: Vec<f64>,
    request_seq: u16,
}

impl JitteryTv {
    pub fn new(id: SpeakerId, seed: u64, spike_chance: f64) -> JitteryTv {
        JitteryTv {
            id,
            rng: Rng::new(seed),
            spike_chance,
            segment_end: f64::MIN,
            one_way: 0.0,
            queue_free: f64::MIN,
            seen: 0,
            sync: None,
            packets: Vec::new(),
            by_seq: HashMap::new(),
            unchecked: 0,
            next_check: 0.0,
            on_the_way: VecDeque::new(),
            asked: Vec::new(),
            request_seq: 0,
        }
    }

    /// The one-way delay at `t`; times run forward only.
    fn one_way_at(&mut self, t: f64) -> f64 {
        while t >= self.segment_end {
            self.segment_end = if self.segment_end == f64::MIN {
                t + SEGMENT
            } else {
                self.segment_end + SEGMENT
            };
            let round_trip = if self.rng.uniform(0.0, 1.0) < self.spike_chance {
                self.rng.uniform(0.28, 0.315)
            } else {
                self.rng.uniform(0.02, 0.15)
            };
            self.one_way = round_trip / 2.0;
        }
        self.one_way
    }

    fn arrive(&mut self, sent: f64) -> f64 {
        let at = (sent + self.one_way_at(sent)).max(self.queue_free);
        self.queue_free = at;
        at
    }

    /// Takes in what the stream sent since the last call, asks for what is
    /// missing and hands the sender the requests that reached it.
    fn pump(&mut self, rig: &mut Rig) {
        while self.seen < rig.sent.len() {
            let sent = &rig.sent[self.seen];
            self.seen += 1;
            if sent.transmit.speaker != self.id {
                continue;
            }
            let (at, kind, bytes) = (sent.at, sent.transmit.kind, sent.transmit.bytes.clone());
            match kind {
                PacketKind::Sync => self.sync = SyncPacket::parse(&bytes),
                PacketKind::Audio => {
                    let (Some(header), Some(sync)) = (RtpHeader::parse(&bytes), self.sync) else {
                        continue;
                    };
                    let play = rig.clock.seconds_of(sync.time)
                        + f64::from(header.timestamp.wrapping_sub(sync.rtp_heard) as i32)
                            / 44_100.0;
                    let arrival = self.arrive(at);
                    self.by_seq.insert(header.seq, self.packets.len());
                    self.packets.push(Heard {
                        seq: header.seq,
                        timestamp: header.timestamp,
                        sent: at,
                        play,
                        arrival,
                        payload: bytes[HEADER_BYTES..].to_vec(),
                        requested: false,
                    });
                }
                PacketKind::Resend => {
                    let arrival = self.arrive(at);
                    let Some(header) = RtpHeader::parse(&bytes[RESEND_PREFIX_BYTES..]) else {
                        continue;
                    };
                    if let Some(&index) = self.by_seq.get(&header.seq) {
                        let packet = &mut self.packets[index];
                        packet.arrival = packet.arrival.min(arrival);
                    }
                }
                PacketKind::Timing => {}
            }
        }
        let now = rig.t();
        while self.next_check <= now {
            let check = self.next_check;
            self.next_check += CHECK;
            let mut runs: Vec<(u16, u16)> = Vec::new();
            while let Some(packet) = self.packets.get_mut(self.unchecked) {
                if packet.play - LEAD > check {
                    break;
                }
                self.unchecked += 1;
                if packet.arrival <= packet.play - LEAD || packet.requested {
                    continue;
                }
                packet.requested = true;
                match runs.last_mut() {
                    Some((first, count)) if first.wrapping_add(*count) == packet.seq => *count += 1,
                    _ => runs.push((packet.seq, 1)),
                }
            }
            for (first, count) in runs {
                self.request_seq = self.request_seq.wrapping_add(1);
                let reaches = check + self.one_way_at(check);
                self.on_the_way.push_back((
                    reaches,
                    ResendRequest {
                        seq: self.request_seq,
                        first,
                        count,
                    },
                ));
            }
        }
        while self.on_the_way.front().is_some_and(|(at, _)| *at <= now) {
            let (_, request) = self.on_the_way.pop_front().expect("a request");
            rig.stream
                .handle_control_packet(self.id, &request.encode(), rig.now);
            rig.drain();
            self.asked.push(now);
        }
    }

    /// Runs the rig to `seconds` in 5 ms steps, answering every FLUSH at
    /// once and passing every datagram through the network.
    pub fn run_to(&mut self, rig: &mut Rig, seconds: f64) {
        while rig.t() < seconds - 1e-9 {
            let step = (seconds - rig.t()).min(0.005);
            rig.run_for(Duration::from_secs_f64(step));
            rig.answer_flushes();
            self.pump(rig);
        }
    }

    /// The requests that reached the sender from `from` to `to` seconds.
    pub fn asked_between(&self, from: f64, to: f64) -> usize {
        self.asked
            .iter()
            .filter(|&&at| at >= from && at < to)
            .count()
    }
}
