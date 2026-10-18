//! A stream on a fake clock, a scripted host that writes blocks with
//! jitter, drift and late deliveries, and fake receivers that read what the
//! stream sends, for the audio, RTP and timing tests. Everything runs in
//! one thread, driven by the stream's own `poll_timeout` and the host's
//! next block, so send times are exact.

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use audiosync_airplay::audio::{FRAMES_PER_PACKET, Frame, read_pcm_payload};
use audiosync_airplay::clock::SenderClock;
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::rtp::{HEADER_BYTES, RtpHeader, SyncPacket};
use audiosync_airplay::sdp::{ALAC_FMTP, Codec};
use audiosync_airplay::session::{MediaDestinations, ReceiverPorts};
use audiosync_airplay::stream::{
    PacketKind, SpeakerConfig, SpeakerId, Stream, StreamConfig, StreamEvent, Transmit,
};

/// A small deterministic generator (xorshift64*), so the jitter of a test
/// is the same on every run.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [low, high).
    pub fn uniform(&mut self, low: f64, high: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        low + (high - low) * unit
    }
}

/// The value the host's frame `f` carries: never 0, so silence the sender
/// inserts is told apart, and different from its neighbours.
pub fn host_frame(f: u64) -> Frame {
    let value = (f % 20_000) as i16 + 1;
    [value, -value]
}

/// The host's audio, written block by block.
pub struct Host {
    /// Frames per block.
    pub block: usize,
    /// The host's true frame rate on the sender clock.
    pub rate: f64,
    /// Each block arrives up to this many seconds early or late.
    pub jitter: f64,
    /// Sender-clock seconds at which frame 0 started.
    pub start: f64,
    /// Blocks written so far.
    pub written: u64,
    /// No block from this index on.
    pub stop_at: Option<u64>,
    /// Blocks that arrive this many seconds late, with every later block
    /// held behind them as a stalled capture thread would.
    pub stalls: Vec<(u64, f64)>,
    /// From each listed block on, the host delivers this many seconds later
    /// and never catches up: a capture that paused and resumed in real time
    /// (a loopback that delivers nothing while the source is silent).
    pub pauses: Vec<(u64, f64)>,
    /// Write float samples instead of 16-bit ones.
    pub float: bool,
    rng: Rng,
    next_at: Option<f64>,
    held_until: f64,
}

impl Host {
    /// 10 ms blocks at exactly 44 100 Hz, no jitter, from `start`.
    pub fn new(start: f64) -> Host {
        Host {
            block: 441,
            rate: 44_100.0,
            jitter: 0.0,
            start,
            written: 0,
            stop_at: None,
            stalls: Vec::new(),
            pauses: Vec::new(),
            float: false,
            rng: Rng::new(0x5EED),
            next_at: None,
            held_until: f64::MIN,
        }
    }

    pub fn seed(mut self, seed: u64) -> Host {
        self.rng = Rng::new(seed);
        self
    }

    /// When the next block arrives, if one does.
    pub fn next_arrival(&mut self) -> Option<f64> {
        if self.stop_at.is_some_and(|stop| self.written >= stop) {
            return None;
        }
        if self.next_at.is_none() {
            let k = self.written;
            let paused: f64 = self
                .pauses
                .iter()
                .filter(|&&(block, _)| block <= k)
                .map(|&(_, seconds)| seconds)
                .sum();
            let produced = self.start + (k + 1) as f64 * self.block as f64 / self.rate + paused;

            let mut at = produced + self.rng.uniform(-self.jitter, self.jitter);
            for &(block, stall) in &self.stalls {
                if block == k {
                    self.held_until = self.held_until.max(produced + stall);
                }
            }
            at = at.max(self.held_until);
            self.next_at = Some(at);
        }
        self.next_at
    }

    /// The samples of block `k`, interleaved.
    pub fn samples(&self, k: u64) -> Vec<i16> {
        let first = k * self.block as u64;
        (first..first + self.block as u64)
            .flat_map(host_frame)
            .collect()
    }

    /// Writes every block that has arrived by `now`.
    pub fn write_due(&mut self, writer: &mut InputWriter, clock: &SenderClock, now: f64) {
        while let Some(at) = self.next_arrival() {
            if at > now + 1e-9 {
                break;
            }
            let samples = self.samples(self.written);
            let instant = clock.instant(at);
            if self.float {
                let floats: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32_767.0).collect();
                writer.write_f32(&floats, instant);
            } else {
                writer.write_i16(&samples, instant);
            }
            self.written += 1;
            self.next_at = None;
        }
    }
}

/// A receiver's documentation address (RFC 5737) and its ports.
pub fn destinations(k: u8) -> MediaDestinations {
    MediaDestinations::new(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20 + k)), 7000),
        ReceiverPorts {
            server_port: 50_000,
            control_port: 50_001,
            timing_port: 50_002,
        },
    )
}

/// Speaker `k` with its own SSRC and RECORD numbering.
pub fn speaker(k: u8, codec: Codec) -> SpeakerConfig {
    SpeakerConfig::new(
        codec,
        destinations(k),
        0x5EED_1000 + u32::from(k),
        1_000 * u16::from(k) + 7,
        1_000_003 * u32::from(k) + 99,
    )
}

/// A datagram the stream sent, when, and the margin M at that moment.
#[derive(Debug, Clone)]
pub struct Sent {
    pub at: f64,
    pub margin: f64,
    pub transmit: Transmit,
}

/// An audio packet as a receiver sees it.
#[derive(Debug, Clone)]
pub struct Audio {
    pub at: f64,
    pub margin: f64,
    pub header: RtpHeader,
    pub payload: Vec<u8>,
}

/// A stream wired to a host on a fake clock.
pub struct Rig {
    pub stream: Stream,
    pub writer: InputWriter,
    pub clock: SenderClock,
    pub start: Instant,
    pub now: Instant,
    pub host: Option<Host>,
    pub sent: Vec<Sent>,
    pub events: Vec<StreamEvent>,
    /// (time, M) after every step.
    pub margins: Vec<(f64, f64)>,
    /// (time, R̂) after every step.
    pub rates: Vec<(f64, f64)>,
}

impl Rig {
    pub fn new(config: StreamConfig) -> Rig {
        let start = Instant::now();
        let clock = SenderClock::new(start);
        let (stream, writer) = Stream::new(config, clock, start);
        Rig {
            stream,
            writer,
            clock,
            start,
            now: start,
            host: None,
            sent: Vec::new(),
            events: Vec::new(),
            margins: Vec::new(),
            rates: Vec::new(),
        }
    }

    /// A rig whose host writes 10 ms blocks from the start.
    pub fn with_host() -> Rig {
        let mut rig = Rig::new(StreamConfig::default());
        rig.host = Some(Host::new(0.0));
        rig
    }

    /// Seconds on the sender clock now.
    pub fn t(&self) -> f64 {
        self.clock.seconds(self.now)
    }

    pub fn instant(&self, seconds: f64) -> Instant {
        self.clock.instant(seconds)
    }

    pub fn add(&mut self, config: SpeakerConfig) -> SpeakerId {
        let id = self.stream.add_speaker(config, self.now);
        self.drain();
        id
    }

    /// Takes the stream's datagrams and events.
    pub fn drain(&mut self) {
        let at = self.t();
        let margin = self.stream.margin().as_secs_f64();
        while let Some(transmit) = self.stream.poll_transmit() {
            self.sent.push(Sent {
                at,
                margin,
                transmit,
            });
        }
        while let Some(event) = self.stream.poll_event() {
            self.events.push(event);
        }
    }

    /// One step at `now`: the host's due blocks, then the stream's timers.
    fn step(&mut self) {
        let t = self.t();
        if let Some(host) = self.host.as_mut() {
            host.write_due(&mut self.writer, &self.clock, t);
        }
        self.stream.handle_timeout(self.now);
        self.drain();
        self.margins.push((t, self.stream.margin().as_secs_f64()));
        self.rates.push((t, self.stream.rate()));
    }

    /// Runs for `duration`, waking exactly when the stream or the host
    /// needs it.
    pub fn run_for(&mut self, duration: Duration) {
        let end = self.now + duration;
        loop {
            let mut next = end;
            if let Some(at) = self.stream.poll_timeout() {
                next = next.min(at);
            }
            let host_next = self.host.as_mut().and_then(|host| host.next_arrival());
            if let Some(at) = host_next {
                next = next.min(self.clock.instant(at));
            }
            if next <= self.now {
                next = self.now + Duration::from_nanos(1_000);
            }
            self.now = next.min(end);
            self.step();
            if self.now >= end {
                break;
            }
        }
    }

    /// Moves the clock to `now` without running anything in between, then
    /// runs one step, as a driver that woke up late would.
    pub fn jump_to(&mut self, now: Instant) {
        self.now = now;
        self.step();
    }

    /// Every datagram to one speaker, in order.
    pub fn of(&self, id: SpeakerId) -> Vec<&Sent> {
        self.sent
            .iter()
            .filter(|sent| sent.transmit.speaker == id)
            .collect()
    }

    pub fn audio(&self, id: SpeakerId) -> Vec<Audio> {
        self.of(id)
            .into_iter()
            .filter(|sent| sent.transmit.kind == PacketKind::Audio)
            .map(|sent| Audio {
                at: sent.at,
                margin: sent.margin,
                header: RtpHeader::parse(&sent.transmit.bytes).expect("RTP header"),
                payload: sent.transmit.bytes[HEADER_BYTES..].to_vec(),
            })
            .collect()
    }

    pub fn syncs(&self, id: SpeakerId) -> Vec<(f64, SyncPacket)> {
        self.of(id)
            .into_iter()
            .filter(|sent| sent.transmit.kind == PacketKind::Sync)
            .map(|sent| {
                (
                    sent.at,
                    SyncPacket::parse(&sent.transmit.bytes).expect("sync packet"),
                )
            })
            .collect()
    }

    /// The speaker's first-sound reports, in order: (timeline packet,
    /// RTP timestamp, time).
    pub fn first_sounds(&self, id: SpeakerId) -> Vec<(u64, u32, f64)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::FirstSound {
                    speaker,
                    packet,
                    rtptime,
                    at,
                } if *speaker == id => Some((*packet, *rtptime, self.clock.seconds(*at))),
                _ => None,
            })
            .collect()
    }

    /// The timeline frame an RTP timestamp of a speaker stands for, by its
    /// last first-sound report (valid between flushes).
    pub fn frame_of(&self, id: SpeakerId, rtp: u32) -> i64 {
        let (packet, rtptime, _) = *self.first_sounds(id).last().expect("first sound");
        (packet * FRAMES_PER_PACKET as u64) as i64 + i64::from(rtp.wrapping_sub(rtptime) as i32)
    }

    /// c(n) in seconds.
    pub fn c(&self, n: i64) -> f64 {
        self.clock.seconds(self.stream.arrival_time(n))
    }

    /// L in seconds.
    pub fn latency(&self) -> f64 {
        self.stream.latency().as_secs_f64()
    }
}

/// The frames of a payload, decoded the receiver's way: big-endian PCM, or
/// ALAC through a reference decoder set up from the `a=fmtp` line.
pub fn decode(codec: Codec, payload: &[u8]) -> Vec<Frame> {
    match codec {
        Codec::Pcm => {
            let mut frames = vec![[0i16; 2]; payload.len() / 4];
            read_pcm_payload(payload, &mut frames);
            frames
        }
        Codec::Alac => {
            let info = alac::StreamInfo::from_sdp_format_parameters(ALAC_FMTP).expect("fmtp");
            let mut decoder = alac::Decoder::new(info);
            let mut samples = vec![0i16; FRAMES_PER_PACKET * 2];
            let decoded = decoder
                .decode_packet(payload, &mut samples)
                .expect("ALAC frame");
            decoded.chunks(2).map(|pair| [pair[0], pair[1]]).collect()
        }
    }
}
