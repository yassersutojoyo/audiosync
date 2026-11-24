//! One audio stream played on many speakers in step (sections 5 to 7).
//!
//! [`Stream`] is sans-I/O like the session. The host writes audio through
//! the [`InputWriter`] it gets with the stream (P6); the driver adds a
//! speaker once its session's RECORD succeeded
//! ([`SessionEvent::Recording`](crate::session::SessionEvent)) and removes
//! it when the session stops playing, runs [`Stream::handle_timeout`] at
//! [`Stream::poll_timeout`], sends every [`Transmit`] from that speaker's
//! control socket (P83), hands control packets and timing requests in, and
//! turns every [`StreamEvent::Flush`] into
//! [`Session::flush`](crate::session::Session::flush).
//!
//! The timeline: frames from the host, plus silence the crate inserts, are
//! numbered n = 0, 1, 2, … (P96) and cut into packets at multiples of 352
//! (P80). Packet j is cut and encoded once, at its send time for a speaker
//! without delay, c(352·j + 351) + M (P84), into one store per codec (P80,
//! P111); each speaker s sends it d_s later from the store with its own
//! header (P81, P99). Speaker s hears frame n at P_s(n) = c(n) + L + d_s,
//! where c(n) is the shared arrival model (P96, P101) and L = Lf / 44 100 s
//! (P98); its sync packets state exactly that (P97), so all speakers stay in
//! step whatever their delay (P102).

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::alac;
use crate::ap2::audio::{PacketSealer, SEALED_OVERHEAD, StreamKey};
use crate::audio::{self, FRAMES_PER_PACKET, PACKET_FRAMES, PCM_PAYLOAD_BYTES, PacketFrames};
use crate::clock::SenderClock;
use crate::input::{self, InputReader, InputWriter, TimelineMove};
use crate::latency::{self, LatencySources, ReportedLatency};
use crate::rate::{MAX_MARGIN, Margin, NOMINAL_RATE, RateModel};

use crate::rtp::{
    self, HEADER_BYTES, RESEND_PREFIX_BYTES, ResendRequest, RtpHeader, SyncPacket, TimeAnnounce,
};
use crate::sdp::Codec;
use crate::session::MediaDestinations;
use crate::store::{DELAY_PACKETS, HISTORY_PACKETS, MARGIN_PACKETS, PayloadStore, STORE_PACKETS};
use crate::volume::Volume;

/// The base latency Lf by default: 4410 frames, 0.1 s (P98).
pub const DEFAULT_LATENCY_FRAMES: u32 = 4_410;
/// The smallest Lf the host may set: 0.1 s (P98).
pub const MIN_LATENCY_FRAMES: u32 = 4_410;
/// The largest Lf the host may set: 2.0 s (P98).
pub const MAX_LATENCY_FRAMES: u32 = 88_200;
/// The largest per-speaker delay (P99): the host's own 20 s delay range, so
/// a Calibrate All result or a Cast hold-back up to it is played in full.
pub const MAX_DELAY: Duration = Duration::from_millis(20_000);
/// Delay changes closer together than this become one flush (P100).
pub const DELAY_MERGE: Duration = Duration::from_millis(250);
/// A retransmission request asks for at most this many packets (P88).
pub const MAX_RESEND_REQUEST: u16 = 128;
/// A packet is resent at most this many times (P88).
pub const MAX_RESENDS: u8 = 3;
/// A resend's second copy leaves this long after the first (P252, G22).
pub const SECOND_COPY_SPACING: Duration = Duration::from_millis(20);
/// Second copies a session holds waiting at most (P252).
pub const MAX_SECOND_COPIES: usize = 128;
/// A packet sent later than this after its time counts as late (P84, P144).
pub const LATE_SEND: Duration = Duration::from_millis(5);
/// A packet already cut leaves this long before its time, so a wake-up
/// that much late still sends it on time (P84).
pub const SEND_LEAD: Duration = Duration::from_millis(4);
/// A late request moves AudioSync's own margin by this much (P200); a
/// resend rate goes straight to the ceiling below (P254).
const STEP_FRAMES: u32 = 4_410;
/// A resend rate raises AudioSync's own margin to this on top of the
/// host's R_max + 4410, 0.4 s in all: the ceiling of every step (P254).
const RATE_STEP_FRAMES: u32 = 13_230;
/// After a step up, this many seconds without a gap step back down (P200).
const CLEAN_WINDOW: f64 = 60.0;
/// During a watch, this many requests within [`FAST_SPAN`] seconds, in
/// [`FAST_SECONDS`] distinct seconds, fail the try (P258, G24).
const FAST_REQUESTS: u32 = 6;
const FAST_SPAN: i64 = 10;
const FAST_SECONDS: usize = 3;
/// Quiet packets a try waits for before its own when it drops more than
/// 0.1 s: 0.3 s and P200's spare of about 0.1 s (P257).
const TRY_QUIET_PACKETS: u64 = 52;
/// A floor's first wait under the line before a try, in seconds (P256,
/// G24).
const FIRST_WAIT: f64 = 600.0;
/// Each failed try doubles the wait up to this (P256, G24).
const WAIT_CEILING: f64 = 3_600.0;
/// After a try, this long the fast way back up applies (P258, G24).
const WATCH: f64 = 600.0;
/// With no input since the last hole's restart, a hole up to this many
/// seconds is caught up with late silence; a longer one restarts (P143).
const IDLE_HOLE_CEILING: f64 = 1.0;
/// A packet whose samples all stay below this, -60 dBFS, is quiet (P200).
const QUIET_PEAK: u16 = 33;
/// A step waits for this many quiet packets before its own, about 207 ms
/// (P200).
const QUIET_PACKETS: u64 = 26;
/// The step's own packet and the 5 cut after it, all quiet: the step falls
/// at the end of the first, and the sync stating it reaches each receiver
/// within the other 5, about 40 ms (P200).
const STEP_PACKETS: u64 = 6;
/// Silent packets after the fade-out of a forced step, which only the
/// test-only [`Stream::step_up`] makes, about 32 ms: the sync stating the
/// step reaches each receiver within them (P200).
const STEP_SILENCE_PACKETS: u64 = 4;
/// More resend requests than this from one speaker within
/// [`RATE_WINDOW`] count as a gap (P200).
const RATE_GAP_REQUESTS: usize = 20;
/// The window of a speaker's resend rate and of its debug line, in seconds
/// (P200).
const RATE_WINDOW: f64 = 60.0;
/// Those requests span at least this many seconds: one burst re-asking for
/// a lost run is no rate (P200).
const RATE_SPAN: f64 = 10.0;
/// And fall in at least this many distinct seconds, so a burst and one
/// stray request after it are no rate (P200).
const RATE_SECONDS: usize = 5;
/// Tight requests within the rate's minute that make a receiver starved,
/// in at least this many distinct seconds (P260).
const STARVED_REQUESTS: u32 = 3;
const STARVED_SECONDS: usize = 2;
/// The input ring by default: 2 s (at least 1 s, P111).
pub const DEFAULT_INPUT_FRAMES: usize = 2 * audio::SAMPLE_RATE as usize;
/// Times within this many seconds of being due count as due, so a wake-up
/// at an instant rounded to the clock's resolution finds its work.
const DUE_TOLERANCE: f64 = 1e-6;

/// How a stream starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Lf, clamped to 4410 to 88 200 frames (P98).
    pub latency_frames: u32,
    /// The input ring's size in frames, at least 1 s (P111).
    pub input_frames: usize,
    /// The host answers every latency event with [`Stream::set_latency`],
    /// so a speaker joining a stream that sent no audio yet waits for it
    /// (P201).
    pub host_answers_latency: bool,
}

impl Default for StreamConfig {
    fn default() -> StreamConfig {
        StreamConfig {
            latency_frames: DEFAULT_LATENCY_FRAMES,
            input_frames: DEFAULT_INPUT_FRAMES,
            host_answers_latency: false,
        }
    }
}

/// A speaker of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpeakerId(u64);

/// What the stream needs to know about a speaker whose RECORD succeeded.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerConfig {
    /// The session's codec (P74).
    pub codec: Codec,
    /// Where its packets go, from the session (P17, P83).
    pub destinations: MediaDestinations,
    /// Its SSRC, random per session (P78).
    pub ssrc: u32,
    /// The sequence number and timestamp its RECORD announced (P32, P79).
    pub first_seq: u16,
    pub first_rtptime: u32,
    /// d_s, clamped to 0 to 20 000 ms (P99).
    pub delay: Duration,
    /// `Some` for a receiver with `sv=true`: the samples are scaled by this
    /// volume before encoding, in an encoding of its own (P121).
    pub software_volume: Option<Volume>,
    /// A receiver with `sm=true` (and not `sv`): the samples are zeroed while
    /// it is muted, in an encoding of its own, and left as they are
    /// otherwise (P121).
    pub software_mute: bool,
    /// With `software_mute`, whether it starts muted, so the packets cut
    /// before it joined are zeroed too.
    pub muted: bool,
    /// What the receiver reported of its own latency, reported back and
    /// never folded into Lf or the timeline (P110, P199).
    pub reported_latency: ReportedLatency,
    /// `Some` for an AirPlay 2 session: its packets are sealed (P155).
    pub airplay2: Option<Ap2Audio>,
}

/// How an AirPlay 2 speaker's packets differ (P155, P166).
#[derive(Debug, Clone, PartialEq)]
pub struct Ap2Audio {
    /// The `shk` of the stream SETUP (P156).
    pub key: StreamKey,
    /// Under PTP timing, the clock identity: time-announce packets take the
    /// place of sync packets (P166).
    pub clock_identity: Option<[u8; 8]>,
}

impl SpeakerConfig {
    /// A speaker without delay or software volume.
    pub fn new(
        codec: Codec,
        destinations: MediaDestinations,
        ssrc: u32,
        first_seq: u16,
        first_rtptime: u32,
    ) -> SpeakerConfig {
        SpeakerConfig {
            codec,
            destinations,
            ssrc,
            first_seq,
            first_rtptime,
            delay: Duration::ZERO,
            software_volume: None,
            software_mute: false,
            muted: false,
            reported_latency: ReportedLatency::default(),
            airplay2: None,
        }
    }
}

/// What a datagram carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// An RTP audio packet, to the receiver's `server_port`.
    Audio,
    /// A sync packet, to its `control_port`.
    Sync,
    /// A retransmission reply, to its `control_port`.
    Resend,
    /// A timing reply, to the source of the request (P91).
    Timing,
}

/// The session socket a datagram leaves from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSocket {
    /// The control socket: audio, sync and retransmission replies (P83).
    Control,
    /// The timing socket: timing replies (P91).
    Timing,
}

/// A datagram to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    pub speaker: SpeakerId,
    pub kind: PacketKind,
    pub from: LocalSocket,
    pub to: SocketAddr,
    pub bytes: Vec<u8>,
}

/// Something the stream reports.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Send FLUSH on this speaker's session, naming the first packet that
    /// follows it (P34): its delay changed (P100), Lf changed (P98) or the
    /// timeline restarted (P143).
    Flush {
        speaker: SpeakerId,
        seq: u16,
        rtptime: u32,
    },
    /// When a speaker's first packet after RECORD or a flush is due to be
    /// heard, and which timeline packet that is (P103, P115).
    FirstSound {
        speaker: SpeakerId,
        packet: u64,
        rtptime: u32,
        at: Instant,
    },
    /// The host set an Lf while a speaker sounds: it is held and applies
    /// when the stream next has no speaker or the timeline restarts
    /// (P201).
    LatencyHeld { frames: u32 },
    /// A wanted step waits: the send margin M leaves its quiet packets no
    /// time ahead of their play time. Once per wanted step (P200).
    StepWaiting { margin: Duration },
    /// A speaker's resend requests in the last minute, once a minute; more
    /// than 20 count as a gap (P200).
    ResendRate { speaker: SpeakerId, requests: u32 },
    /// A speaker whose R_s needs more than the stream's Lf joined a
    /// sounding stream and plays at that Lf; once per session (P201).
    LatencyShortfall {
        speaker: SpeakerId,
        reported_us: u64,
        latency_frames: u32,
    },
    /// L or the timeline's mapping changed: local outputs re-anchor (P98,
    /// P114, P143). Frame n of the timeline is the host's frame at input
    /// ring position n + `origin` (`InputWriter::position`). Sent when Lf
    /// changes, when the timeline restarts (`restarted`), and when a paused
    /// capture resumes and its frames are placed anew (P112).
    Timeline {
        latency: Duration,
        origin: i64,
        restarted: bool,
    },
    /// A receiver gave up its floor through a try, from Lf `from` to `to`
    /// frames, after `wait` under the line (P258).
    MarginTry {
        receiver: IpAddr,
        from: u32,
        to: u32,
        wait: Duration,
    },
    /// A try failed after `after`: the receiver's floor is back at `back_to`
    /// frames, and its next wait is `next_wait` (P258).
    MarginTryFailed {
        receiver: IpAddr,
        back_to: u32,
        after: Duration,
        requests: u32,
        late: u32,
        next_wait: Duration,
    },
    /// A try's watch ended without a failure (P258).
    MarginTryHeld { receiver: IpAddr, wait: Duration },
}

impl StreamEvent {
    /// The debug log line of a try, a failed try or a held one (P258).
    pub fn margin_line(&self) -> Option<String> {
        let ms = |frames: u32| (f64::from(frames) * 1000.0 / NOMINAL_RATE).round();
        match self {
            StreamEvent::MarginTry {
                receiver,
                from,
                to,
                wait,
            } => Some(format!(
                "margin try: {receiver} {} ms to {} ms after {} under the line",
                ms(*from),
                ms(*to),
                spoken(*wait)
            )),
            StreamEvent::MarginTryFailed {
                receiver,
                back_to,
                after,
                requests,
                late,
                next_wait,
            } => Some(format!(
                "margin try failed: {receiver} back to {} ms after {} s, {requests} requests, {late} late; next wait {}",
                ms(*back_to),
                after.as_secs(),
                spoken(*next_wait)
            )),
            StreamEvent::MarginTryHeld { receiver, wait } => Some(format!(
                "margin try held: {receiver} wait back to {}",
                spoken(*wait)
            )),
            _ => None,
        }
    }
}

/// A wait as the log says it: whole minutes, or seconds for a test's
/// shortened one.
fn spoken(wait: Duration) -> String {
    match wait.as_secs() {
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

/// A speaker's latencies (P110, P113, P115, P199).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeakerLatency {
    /// From a frame's arrival at c(n) to its sound: L + d_s (P113).
    pub arrival_to_sound: Duration,
    /// The receiver's own `Audio-Latency`, in frames, as it reported it
    /// (P110).
    pub receiver_latency_frames: Option<u32>,
    /// R_s in microseconds, 0 when nothing was reported (P199).
    pub receiver_latency_us: u64,
    /// The reports that gave R_s (P199).
    pub receiver_latency_sources: LatencySources,
    /// When its first packet after RECORD or the last flush is due to be
    /// heard (P115).
    pub first_sound: Instant,
}

/// Per-speaker counters the host reads without locks (P144).
#[derive(Debug, Default)]
pub struct SpeakerCounters {
    packets_sent: AtomicU64,
    resend_requests: AtomicU64,
    packets_resent: AtomicU64,
    not_held: AtomicU64,
    second_copies_sent: AtomicU64,
    second_copies_skipped: AtomicU64,
    timing_replies: AtomicU64,
    late_packets: AtomicU64,
    silent_frames: AtomicU64,
    foreign_datagrams: AtomicU64,
    bad_network_dropped: AtomicU64,
    bad_network_delayed: AtomicU64,
    connects: ConnectStarts,
    stream: Arc<StreamCounters>,
}

/// The connects the queue let start for a speaker (P222), the last one as
/// nanoseconds after `base` so it fits an atomic.
#[derive(Debug)]
struct ConnectStarts {
    base: Instant,
    count: AtomicU64,
    last: AtomicU64,
}

impl Default for ConnectStarts {
    fn default() -> ConnectStarts {
        ConnectStarts {
            base: Instant::now(),
            count: AtomicU64::new(0),
            last: AtomicU64::new(0),
        }
    }
}

impl SpeakerCounters {
    /// Counters at zero that report `stream`'s input drops.
    pub(crate) fn for_stream(stream: Arc<StreamCounters>) -> SpeakerCounters {
        SpeakerCounters {
            stream,
            ..SpeakerCounters::default()
        }
    }

    /// One more timing request answered, by whichever thread answered it
    /// (P91, P144).
    pub(crate) fn count_timing_reply(&self) {
        self.timing_replies.fetch_add(1, Ordering::Relaxed);
    }

    /// One more datagram dropped on the session's control or timing socket
    /// because it came from an address other than the receiver's (P86,
    /// P91, P144).
    pub(crate) fn count_foreign_datagram(&self) {
        self.foreign_datagrams.fetch_add(1, Ordering::Relaxed);
    }

    /// The connect queue let a connect of this speaker start at `at`
    /// (P222).
    pub(crate) fn count_connect_start(&self, at: Instant) {
        let nanos = at.saturating_duration_since(self.connects.base).as_nanos();
        self.connects
            .last
            .store(u64::try_from(nanos).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.connects.count.fetch_add(1, Ordering::Release);
    }

    /// Connects the queue let start, the sender's own record of its turns
    /// (P222).
    pub fn connect_starts(&self) -> u64 {
        self.connects.count.load(Ordering::Acquire)
    }

    /// When the last of [`SpeakerCounters::connect_starts`] started.
    pub fn last_connect_start(&self) -> Option<Instant> {
        if self.connect_starts() == 0 {
            return None;
        }
        let nanos = self.connects.last.load(Ordering::Relaxed);
        Some(self.connects.base + Duration::from_nanos(nanos))
    }

    /// Audio packets sent (resends not included).
    pub fn packets_sent(&self) -> u64 {
        self.packets_sent.load(Ordering::Relaxed)
    }

    /// Retransmission requests received.
    pub fn resend_requests(&self) -> u64 {
        self.resend_requests.load(Ordering::Relaxed)
    }

    /// Packets resent.
    pub fn packets_resent(&self) -> u64 {
        self.packets_resent.load(Ordering::Relaxed)
    }

    /// Requested packets not resent: no longer held, or already resent
    /// three times.
    pub fn not_held(&self) -> u64 {
        self.not_held.load(Ordering::Relaxed)
    }

    /// Second copies of resends sent (P252).
    pub fn second_copies_sent(&self) -> u64 {
        self.second_copies_sent.load(Ordering::Relaxed)
    }

    /// Second copies of resends skipped: too late, or the list was full
    /// (P252).
    pub fn second_copies_skipped(&self) -> u64 {
        self.second_copies_skipped.load(Ordering::Relaxed)
    }

    /// Timing requests answered.
    pub fn timing_replies(&self) -> u64 {
        self.timing_replies.load(Ordering::Relaxed)
    }

    /// Packets sent more than 5 ms after their time.
    pub fn late_packets(&self) -> u64 {
        self.late_packets.load(Ordering::Relaxed)
    }

    /// Silent frames the sender inserted into the packets sent to this
    /// speaker (P112).
    pub fn silent_frames(&self) -> u64 {
        self.silent_frames.load(Ordering::Relaxed)
    }

    /// Audio and resend datagrams the bad-network switch dropped (P253).
    pub fn bad_network_dropped(&self) -> u64 {
        self.bad_network_dropped.load(Ordering::Relaxed)
    }

    /// Audio and resend datagrams the bad-network switch delayed (P253).
    pub fn bad_network_delayed(&self) -> u64 {
        self.bad_network_delayed.load(Ordering::Relaxed)
    }

    pub(crate) fn note_bad_network(&self, dropped: bool) {
        let counter = if dropped {
            &self.bad_network_dropped
        } else {
            &self.bad_network_delayed
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Datagrams dropped on the session's control and timing sockets
    /// because they came from an address other than the receiver's (P86,
    /// P91).
    pub fn foreign_datagrams(&self) -> u64 {
        self.foreign_datagrams.load(Ordering::Relaxed)
    }

    /// Input frames dropped: the stream's count, since every speaker plays
    /// the one input (P112).
    pub fn input_frames_dropped(&self) -> u64 {
        self.stream.input_frames_dropped()
    }
}

/// Stream counters the host reads without locks (P144).
#[derive(Debug, Default)]
pub struct StreamCounters {
    rate_bits: AtomicU64,
    packets_encoded: AtomicU64,
    silent_frames: AtomicU64,
    input_frames_dropped: AtomicU64,
}

impl StreamCounters {
    /// The current R̂ in Hz (P101).
    pub fn rate(&self) -> f64 {
        f64::from_bits(self.rate_bits.load(Ordering::Relaxed))
    }

    /// Packets encoded: one per packet of the timeline, however many
    /// speakers play it (P80), and none while the stream has none (P251).
    pub fn packets_encoded(&self) -> u64 {
        self.packets_encoded.load(Ordering::Relaxed)
    }

    /// Silent frames inserted where the host's frames hadn't arrived in
    /// time (P112).
    pub fn silent_frames(&self) -> u64 {
        self.silent_frames.load(Ordering::Relaxed)
    }

    /// Input frames dropped: late, beyond a full ring, or discarded by a
    /// restart (P112).
    pub fn input_frames_dropped(&self) -> u64 {
        self.input_frames_dropped.load(Ordering::Relaxed)
    }
}

/// The local delay a host gives an output that used the engine's stored
/// delay setting, migrated once (P114): clamp(configured − L, 0, 20 000 ms).
pub fn migrate_local_delay(configured: Duration, latency: Duration) -> Duration {
    configured.saturating_sub(latency).min(MAX_DELAY)
}

/// The last frame of timeline packet `j`.
fn last_frame(j: u64) -> i64 {
    (j * PACKET_FRAMES + PACKET_FRAMES - 1) as i64
}

/// Whether timeline packet `j` is the first that starts at or after a
/// multiple of 44 100 frames (P94).
fn starts_sync_mark(j: u64) -> bool {
    let interval = audio::SAMPLE_RATE as u64;
    j == 0 || (j * PACKET_FRAMES) / interval != ((j - 1) * PACKET_FRAMES) / interval
}

fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

fn clamp_delay(delay: Duration) -> f64 {
    seconds(delay.min(MAX_DELAY))
}

/// Bytes of a payload in a codec.
fn payload_bytes(codec: Codec) -> usize {
    match codec {
        Codec::Alac => alac::FRAME_BYTES,
        Codec::Pcm => PCM_PAYLOAD_BYTES,
    }
}

fn encode(codec: Codec, frames: &PacketFrames, store: &mut PayloadStore, number: u64, silent: u16) {
    match codec {
        Codec::Alac => {
            let mut frame = [0u8; alac::FRAME_BYTES];
            alac::write_frame(frames, &mut frame);
            store.put(number, &frame, silent);
        }
        Codec::Pcm => {
            let mut payload = [0u8; PCM_PAYLOAD_BYTES];
            audio::write_pcm_payload(frames, &mut payload);
            store.put(number, &payload, silent);
        }
    }
}

/// A packet a speaker sent, for resends (P87, P88).
#[derive(Debug, Clone, Copy, Default)]
struct Sent {
    valid: bool,
    seq: u16,
    /// The absolute packet number in the stores.
    number: u64,
    timestamp: u32,
    marker: bool,
    resends: u8,
}

/// A speaker's own encoding (P121).
#[derive(Debug)]
struct OwnEncoding {
    gain: f64,
    /// `sm=true` without `sv`: the gain is 0 when muted and 1 otherwise.
    mute_only: bool,
    store: PayloadStore,
}

impl OwnEncoding {
    fn set(&mut self, volume: Volume) {
        self.gain = match (self.mute_only, volume) {
            (true, Volume::Muted) => 0.0,
            (true, Volume::Db(_)) => 1.0,
            (false, volume) => audio::software_gain(volume),
        };
    }
}

/// A resend's second copy waiting for its time (P252).
#[derive(Debug)]
struct SecondCopy {
    seq: u16,
    /// When it goes, and when its packet plays on the speaker, in seconds.
    due: f64,
    play: f64,
    bytes: Vec<u8>,
}

/// One second of a speaker's resend requests (P200).
#[derive(Clone, Copy, Debug)]
struct RateSecond {
    second: i64,
    /// When its first request came, in seconds.
    first: f64,
    requests: u32,
    /// Its requests that came past their packet's midpoint (P260).
    tight: u32,
}

#[derive(Debug)]
struct Speaker {
    id: SpeakerId,
    codec: Codec,
    destinations: MediaDestinations,
    ssrc: u32,
    /// The timestamp RECORD announced, for a speaker that has sent nothing
    /// yet.
    first_rtptime: u32,
    /// d_s in seconds.
    delay: f64,
    /// A delay change waiting for its flush: the delay and when.
    pending_delay: Option<(f64, f64)>,
    /// The merge window of `pending_delay` ended; it waits for its session
    /// to be able to send the FLUSH at once (P100).
    delay_due: bool,
    /// The next timeline packet to send.
    next_packet: u64,
    next_seq: u16,
    /// k_s: frame n has RTP timestamp k_s + n (P81).
    rtp_offset: u32,
    /// The next audio packet carries the marker and the next sync the
    /// first flag (P78, P93).
    first_pending: bool,
    /// The Lf its syncs still state while a step is on its way, and when
    /// the sync stating the step goes: as the end of the step's packet
    /// sounds on this speaker (P200).
    step_sync: Option<(u32, f64)>,
    /// Its resend requests of the last [`RATE_WINDOW`], one entry per
    /// second that had any, oldest first; never more than 60 (P200).
    recent_requests: VecDeque<RateSecond>,
    /// The start of the minute its resend rate line counts, and the
    /// requests in it (P200).
    rate_since: f64,
    rate_requests: u32,
    /// Anything sent since RECORD.
    started: bool,
    /// The FLUSH of an explicit pause or a timeline restart is not answered
    /// yet; a resume or the new timeline waits for it (P143, P213).
    awaiting_flush: bool,
    /// Joined a stream that had sent no audio: it sends none until the
    /// host answers its latency event (P201).
    awaiting_latency: bool,
    sync_seq: u16,
    history: Box<[Sent]>,
    own: Option<OwnEncoding>,
    reported: ReportedLatency,
    /// The shortfall of P201 was told for this session.
    shortfall_told: bool,
    first_sound: f64,
    counters: Arc<SpeakerCounters>,
    /// An AirPlay 2 session's sealed packets (P155).
    sealed: Option<SealedStore>,
    /// Under PTP timing: time-announce packets in place of sync (P166).
    clock_identity: Option<[u8; 8]>,
    /// Resend second copies waiting, oldest first (P252).
    second_copies: VecDeque<SecondCopy>,
}

/// An AirPlay 2 speaker's sealer and its last 1024 sealed packets, kept as
/// sent so a resend is byte-identical (P87, P155). Allocated whole when the
/// speaker joins (P111).
struct SealedStore {
    sealer: PacketSealer,
    /// One slot per history entry, `slot_bytes` each.
    slots: Box<[u8]>,
    lengths: Box<[usize]>,
    slot_bytes: usize,
}

impl std::fmt::Debug for SealedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedStore")
            .field("sealer", &self.sealer)
            .finish_non_exhaustive()
    }
}

impl SealedStore {
    fn new(key: &StreamKey, codec: Codec) -> SealedStore {
        let slot_bytes = payload_bytes(codec) + SEALED_OVERHEAD;
        SealedStore {
            sealer: PacketSealer::new(key),
            slots: vec![0u8; slot_bytes * HISTORY_PACKETS].into_boxed_slice(),
            lengths: vec![0usize; HISTORY_PACKETS].into_boxed_slice(),
            slot_bytes,
        }
    }

    /// Seals a packet into history slot `slot` and gives its bytes.
    fn seal(&mut self, slot: usize, header: &[u8; HEADER_BYTES], payload: &[u8]) -> &[u8] {
        let length = (payload.len() + SEALED_OVERHEAD).min(self.slot_bytes);
        let start = slot * self.slot_bytes;
        let out = &mut self.slots[start..start + length];
        self.sealer
            .seal_into(header, &payload[..length - SEALED_OVERHEAD], out);
        self.lengths[slot] = length;
        &self.slots[start..start + length]
    }

    /// The packet sealed into history slot `slot`, as sent.
    fn get(&self, slot: usize) -> Option<&[u8]> {
        let length = *self.lengths.get(slot)?;
        let start = slot * self.slot_bytes;
        (length > 0).then(|| &self.slots[start..start + length])
    }
}

impl Speaker {
    /// Starts sending at timeline packet `packet` with RTP timestamp
    /// `rtptime`, flagged as the first after RECORD or FLUSH.
    fn start_at(&mut self, packet: u64, rtptime: u32) {
        self.recent_requests.clear();
        self.next_packet = packet;
        self.rtp_offset = rtptime.wrapping_sub((packet * PACKET_FRAMES) as u32);
        self.first_pending = true;
    }

    /// The timestamp the next packet would carry (P79).
    fn next_rtptime(&self) -> u32 {
        self.rtp_offset
            .wrapping_add((self.next_packet * PACKET_FRAMES) as u32)
    }
}

/// The parts of the stream a speaker reads while it sends, borrowed field
/// by field so the speakers and the output queues stay mutable.
macro_rules! context {
    ($stream:expr) => {
        Context {
            clock: $stream.clock,
            model: &$stream.model,
            margin: $stream.margin.value(),
            latency_frames: $stream.latency_frames,
            packet_base: $stream.packet_base,
            next_cut: $stream.next_cut,
            pcm: &$stream.pcm,
            alac: &$stream.alac,
        }
    };
}

/// Sends a speaker's next packet (P78, P79, P81, P83): a sync packet first
/// when one is due (P93, P94, P97), then the audio packet with the shared
/// payload and the speaker's own header. `lateness` is how long after its
/// time it goes (P144).
fn send(
    speaker: &mut Speaker,
    context: &Context<'_>,
    silence: (&[u8], &[u8]),
    lateness: f64,
    transmits: &mut VecDeque<Transmit>,
) {
    let packet = speaker.next_packet;
    let rtptime = speaker.next_rtptime();
    let marker = speaker.first_pending;
    if marker || starts_sync_mark(packet) {
        send_sync(speaker, context, marker, transmits);
    }
    let number = context.packet_base + packet;
    let seq = speaker.next_seq;
    let header = RtpHeader {
        marker,
        seq,
        timestamp: rtptime,
        ssrc: speaker.ssrc,
    };
    let (bytes, silent) = {
        let fallback = match speaker.codec {
            Codec::Alac => silence.1,
            Codec::Pcm => silence.0,
        };
        let (payload, silent) = context
            .payload(speaker.own.as_ref(), speaker.codec, number)
            .unwrap_or((fallback, FRAMES_PER_PACKET as u16));
        let header = header.encode();
        let bytes = match &mut speaker.sealed {
            Some(sealed) => sealed
                .seal(usize::from(seq) % HISTORY_PACKETS, &header, payload)
                .to_vec(),
            None => {
                let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
                bytes.extend_from_slice(&header);
                bytes.extend_from_slice(payload);
                bytes
            }
        };
        (bytes, silent)
    };
    transmits.push_back(Transmit {
        speaker: speaker.id,
        kind: PacketKind::Audio,
        from: LocalSocket::Control,
        to: speaker.destinations.audio,
        bytes,
    });
    speaker.history[usize::from(seq) % HISTORY_PACKETS] = Sent {
        valid: true,
        seq,
        number,
        timestamp: rtptime,
        marker,
        resends: 0,
    };
    let counters = &speaker.counters;
    counters.packets_sent.fetch_add(1, Ordering::Relaxed);
    counters
        .silent_frames
        .fetch_add(u64::from(silent), Ordering::Relaxed);
    if lateness > seconds(LATE_SEND) {
        counters.late_packets.fetch_add(1, Ordering::Relaxed);
    }
    speaker.next_seq = seq.wrapping_add(1);
    speaker.next_packet += 1;
    speaker.first_pending = false;
    speaker.started = true;
}

/// Restarts a playing speaker through a FLUSH (P34, P100, P143): the
/// numbering goes on from the values the next packet would have had, the
/// next packet in its schedule at `now` follows, flagged as the first.
fn flush(
    speaker: &mut Speaker,
    context: &Context<'_>,
    now: f64,
    events: &mut VecDeque<StreamEvent>,
) {
    let rtptime = speaker.next_rtptime();
    let packet = context.first_due(speaker.delay, now);
    speaker.start_at(packet, rtptime);
    speaker.second_copies.clear();
    events.push_back(StreamEvent::Flush {
        speaker: speaker.id,
        seq: speaker.next_seq,
        rtptime,
    });
    report_first_sound(speaker, context, events);
}

/// Reports when the speaker's next packet, the first after RECORD or a
/// flush, is due to be heard: P_s(352·j) (P115).
fn report_first_sound(
    speaker: &mut Speaker,
    context: &Context<'_>,
    events: &mut VecDeque<StreamEvent>,
) {
    let packet = speaker.next_packet;
    speaker.first_sound = context.heard((packet * PACKET_FRAMES) as i64, speaker.delay);
    events.push_back(StreamEvent::FirstSound {
        speaker: speaker.id,
        packet,
        rtptime: speaker.next_rtptime(),
        at: context.clock.instant(speaker.first_sound),
    });
}

/// Sends a speaker's sync packet for its next packet (P93, P94, P97),
/// stating the Lf it keeps while a step is on its way (P200).
fn send_sync(
    speaker: &mut Speaker,
    context: &Context<'_>,
    marker: bool,
    transmits: &mut VecDeque<Transmit>,
) {
    if marker {
        speaker.step_sync = None;
    }
    let packet = speaker.next_packet;
    let rtptime = speaker.next_rtptime();
    let latency = speaker
        .step_sync
        .map_or(context.latency_frames, |(frames, _)| frames);
    let heard = (packet * PACKET_FRAMES) as i64 - i64::from(latency);
    // Until its step sounds, a speaker plays at the Lf this sync states.
    let at = context.model.time(heard) + f64::from(latency) / NOMINAL_RATE + speaker.delay;
    let bytes = match speaker.clock_identity {
        Some(clock_identity) => TimeAnnounce {
            first: marker,
            seq: speaker.sync_seq,
            rtp_heard: rtptime.wrapping_sub(latency),
            nanos: context.clock.ptp_at(at).total_nanos(),
            next_rtp: rtptime,
            clock_identity,
        }
        .encode()
        .to_vec(),
        None => SyncPacket {
            first: marker,
            seq: speaker.sync_seq,
            rtp_heard: rtptime.wrapping_sub(latency),
            time: context.clock.ntp_at(at),
            next_rtp: rtptime,
        }
        .encode()
        .to_vec(),
    };
    speaker.sync_seq = speaker.sync_seq.wrapping_add(1);
    transmits.push_back(Transmit {
        speaker: speaker.id,
        kind: PacketKind::Sync,
        from: LocalSocket::Control,
        to: speaker.destinations.control,
        bytes,
    });
}

struct Context<'a> {
    clock: SenderClock,
    model: &'a RateModel,
    margin: f64,
    latency_frames: u32,
    packet_base: u64,
    next_cut: u64,
    pcm: &'a PayloadStore,
    alac: &'a PayloadStore,
}

impl Context<'_> {
    /// When packet j is cut, and sent to a speaker without delay:
    /// c(352·j + 351) + M (P84).
    fn cut_due(&self, j: u64) -> f64 {
        self.model.time(last_frame(j)) + self.margin
    }

    /// L in seconds.
    fn latency(&self) -> f64 {
        f64::from(self.latency_frames) / NOMINAL_RATE
    }

    /// P_s(n) = c(n) + L + d_s (P96).
    fn heard(&self, n: i64, delay: f64) -> f64 {
        self.model.time(n) + self.latency() + delay
    }

    /// The next packet in a speaker's regular schedule at `now`: the first
    /// whose send time hasn't passed (P84, P103), among the packets the
    /// stores can still hold for it.
    fn first_due(&self, delay: f64, now: f64) -> u64 {
        let oldest = self
            .next_cut
            .saturating_sub((DELAY_PACKETS + MARGIN_PACKETS) as u64);
        let mut packet = self.next_cut;
        while packet > oldest && self.cut_due(packet - 1) + delay >= now {
            packet -= 1;
        }
        packet
    }

    /// A speaker's payload for absolute packet `number`, and its silent
    /// frames.
    fn payload<'b>(
        &'b self,
        own: Option<&'b OwnEncoding>,
        codec: Codec,
        number: u64,
    ) -> Option<(&'b [u8], u16)> {
        match (own, codec) {
            (Some(own), _) => own.store.get(number),
            (None, Codec::Alac) => self.alac.get(number),
            (None, Codec::Pcm) => self.pcm.get(number),
        }
    }
}

/// The Lf a receiver's late request or resend rate stepped the stream to,
/// by its address, and its wait before a try (P200, P255, P256).
#[derive(Debug)]
struct Floor {
    ip: IpAddr,
    /// 0 once a try gave it up.
    frames: u32,
    /// The Lf before the step, where a step down after it left returns.
    before: u32,
    /// W in force and the time served under the line, in seconds.
    wait: f64,
    served: f64,
    watch: Option<Watch>,
}

/// The watch after a try (P258).
#[derive(Debug)]
struct Watch {
    since: f64,
    /// The level the try left, where a failure returns.
    left: u32,
    requests: u32,
    late: u32,
}

/// One stream to many speakers.
#[derive(Debug)]
pub struct Stream {
    clock: SenderClock,
    latency_frames: u32,
    /// An Lf the host set while a speaker sounds (P201).
    pending_latency: Option<u32>,
    /// A step waiting for a quiet window: the Lf it goes to (P200).
    step_wanted: Option<u32>,
    /// The session stepped up already (P200).
    stepped_up: bool,
    /// Since a step up applied, the last gap or the step: the start of the
    /// clean window that steps back down (P200).
    clean_since: Option<f64>,
    /// Quiet packets cut in a row (P200).
    quiet_packets: u64,
    /// The wanted step's wait for a smaller M was reported (P200).
    step_wait_told: bool,
    /// The stepped Lf: no lower one applies until the stream has no
    /// speaker (P200, P201).
    step_floor: u32,
    /// The Lf before the step up and the host's latest Lf: a step down
    /// returns to the larger (P200).
    before_step: u32,
    host_latency: u32,
    /// A step on its way: when the end of its packet sounds, and the new Lf
    /// (P200).
    step_due: Option<(f64, u32)>,
    /// A fade-out step came while `step_due` was on its way: it starts as
    /// soon as that step applies (P258).
    fade_after_step: bool,
    /// The Lf a step up took the stream to, by the address of the receiver
    /// whose gap or resend rate asked for it, for the stream's life: no step
    /// down while that receiver is in the stream but through a try, and its
    /// floor when it joins again (P200, P255).
    rate_levels: Vec<Floor>,
    /// When the time under the line was last counted (P256).
    served_at: f64,
    /// The first wait before a try, its ceiling and the watch after it, in
    /// seconds (P256, P258).
    first_wait: f64,
    wait_ceiling: f64,
    watch: f64,
    /// Every receiver whose gap or resend rate asked for the step on its way.
    askers: Vec<IpAddr>,
    /// The first packet cut at full gain again after a forced step's
    /// fade-out and silence; only the test-only `step_up` sets it (P200).
    silence_until: u64,
    input: InputReader,
    model: RateModel,
    margin: Margin,
    /// Frame n of the timeline is at input ring position n + `origin`.
    origin: i64,
    /// The absolute packet number of timeline packet 0; the stores are
    /// keyed by absolute numbers, which never repeat across restarts.
    packet_base: u64,
    /// The next timeline packet to cut.
    next_cut: u64,
    frames: PacketFrames,
    /// The host's gain on every packet cut, before any encoding (P80).
    master_gain: f64,
    /// The gain the last packet cut ended at, so a change fades in (P80).
    cut_gain: f64,
    /// PCM payloads, which are also the source of own encodings (P121).
    pcm: PayloadStore,
    alac: PayloadStore,
    silence_pcm: Box<[u8]>,
    silence_alac: Box<[u8]>,
    speakers: Vec<Speaker>,
    /// The host paused the stream (P213): packets are cut but not sent.
    paused: bool,
    /// A hole restarted the timeline and no frame has arrived since: the
    /// next hole waits for input, so an idle stream restarts once (P143).
    hole_idle: bool,
    host_answers_latency: bool,
    next_speaker: u64,
    transmits: VecDeque<Transmit>,
    events: VecDeque<StreamEvent>,
    counters: Arc<StreamCounters>,
}

impl Stream {
    /// A stream whose timeline starts at `now`, and the host's input. All
    /// buffers are allocated here (P111).
    pub fn new(config: StreamConfig, clock: SenderClock, now: Instant) -> (Stream, InputWriter) {
        let (writer, reader) = input::ring(config.input_frames, clock.origin());
        let start = clock.seconds(now);
        let silence = [[0i16; 2]; FRAMES_PER_PACKET];
        let mut silence_pcm = vec![0u8; PCM_PAYLOAD_BYTES].into_boxed_slice();
        audio::write_pcm_payload(&silence, &mut silence_pcm);
        let mut silence_alac = [0u8; alac::FRAME_BYTES];
        alac::write_frame(&silence, &mut silence_alac);
        let mut stream = Stream {
            clock,
            latency_frames: config
                .latency_frames
                .clamp(MIN_LATENCY_FRAMES, MAX_LATENCY_FRAMES),
            pending_latency: None,
            step_wanted: None,
            stepped_up: false,
            clean_since: None,
            quiet_packets: 0,
            step_wait_told: false,
            step_floor: 0,
            before_step: 0,
            host_latency: config
                .latency_frames
                .clamp(MIN_LATENCY_FRAMES, MAX_LATENCY_FRAMES),
            step_due: None,
            fade_after_step: false,
            rate_levels: Vec::new(),
            served_at: start,
            first_wait: FIRST_WAIT,
            wait_ceiling: WAIT_CEILING,
            watch: WATCH,
            askers: Vec::new(),
            silence_until: 0,
            input: reader,
            model: RateModel::new(start),
            margin: Margin::new(start),
            origin: 0,
            packet_base: 0,
            next_cut: 0,
            frames: silence,
            master_gain: 1.0,
            cut_gain: 1.0,
            pcm: PayloadStore::new(STORE_PACKETS, PCM_PAYLOAD_BYTES),
            alac: PayloadStore::new(STORE_PACKETS, alac::FRAME_BYTES),
            silence_pcm,
            silence_alac: Box::new(silence_alac),
            speakers: Vec::new(),
            paused: false,
            hole_idle: false,
            host_answers_latency: config.host_answers_latency,
            next_speaker: 0,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
            counters: Arc::new(StreamCounters::default()),
        };
        stream.model.ensure(last_frame(0));
        stream.publish();
        (stream, writer)
    }

    fn context(&self) -> Context<'_> {
        context!(self)
    }

    pub fn clock(&self) -> SenderClock {
        self.clock
    }

    /// L, the base latency (P98, P113).
    pub fn latency(&self) -> Duration {
        Duration::from_secs_f64(f64::from(self.latency_frames) / NOMINAL_RATE)
    }

    /// Lf in frames (P98).
    pub fn latency_frames(&self) -> u32 {
        self.latency_frames
    }

    /// The speakers whose receiver is at `ip`.
    pub fn speakers_at(&self, ip: IpAddr) -> Vec<SpeakerId> {
        self.speakers
            .iter()
            .filter(|speaker| speaker.destinations.control.ip() == ip)
            .map(|speaker| speaker.id)
            .collect()
    }

    /// Shortens the first wait, its ceiling and the watch of P256 and
    /// P258, for tests and the rig's scaled run (P259).
    #[doc(hidden)]
    pub fn set_step_down_times(
        &mut self,
        first_wait: Duration,
        ceiling: Duration,
        watch: Duration,
    ) {
        self.first_wait = first_wait.as_secs_f64();
        self.wait_ceiling = ceiling.as_secs_f64();
        self.watch = watch.as_secs_f64();
    }

    /// An Lf held until the stream next has no speaker or restarts (P201).
    pub fn pending_latency_frames(&self) -> Option<u32> {
        self.pending_latency
    }

    /// True once any speaker reached its first-sound time at `now` (P115,
    /// P201): an Lf set now would be held.
    pub fn is_sounding(&self, now: Instant) -> bool {
        self.sounding(self.clock.seconds(now))
    }

    fn sounding(&self, t: f64) -> bool {
        self.speakers
            .iter()
            .any(|speaker| speaker.started && t >= speaker.first_sound)
    }

    /// Tells the shortfall of P201 for every speaker whose R_s needs more
    /// than the stream's Lf, once per session.
    fn tell_shortfalls(&mut self) {
        for speaker in &mut self.speakers {
            let reported_us = speaker.reported.micros();
            if !speaker.shortfall_told
                && reported_us > 0
                && latency::latency_needed_frames(reported_us) > self.latency_frames
            {
                speaker.shortfall_told = true;
                self.events.push_back(StreamEvent::LatencyShortfall {
                    speaker: speaker.id,
                    reported_us,
                    latency_frames: self.latency_frames,
                });
            }
        }
    }

    /// c(n): when timeline frame `n` arrived from the host, as the model
    /// gives it (P96, P113). Frames past the next sync point are a
    /// prediction.
    pub fn arrival_time(&self, n: i64) -> Instant {
        self.clock.instant(self.model.time(n))
    }

    /// Frame n of the timeline is the host's frame at input ring position
    /// n + `origin` (P113, P114).
    pub fn origin(&self) -> i64 {
        self.origin
    }

    /// The send margin M (P84).
    pub fn margin(&self) -> Duration {
        Duration::from_secs_f64(self.margin.value())
    }

    /// R̂ in Hz (P101).
    pub fn rate(&self) -> f64 {
        self.model.rate()
    }

    /// The next timeline packet to be cut.
    pub fn next_packet(&self) -> u64 {
        self.next_cut
    }

    /// Packets each store holds (P111).
    pub fn store_packets(&self) -> usize {
        self.pcm.capacity()
    }

    /// The input ring's size in frames (P111).
    pub fn input_frames(&self) -> usize {
        self.input.capacity()
    }

    /// Packets a speaker's own store holds, for a speaker with software
    /// volume (P111, P121).
    pub fn own_store_packets(&self, id: SpeakerId) -> Option<usize> {
        let speaker = self.speakers.iter().find(|s| s.id == id)?;
        speaker.own.as_ref().map(|own| own.store.capacity())
    }

    pub fn counters(&self) -> Arc<StreamCounters> {
        self.counters.clone()
    }

    pub fn speaker_counters(&self, id: SpeakerId) -> Option<Arc<SpeakerCounters>> {
        let speaker = self.speakers.iter().find(|s| s.id == id)?;
        Some(speaker.counters.clone())
    }

    /// The delay a host gives a local output with trim `local` so that it
    /// plays in step with the speakers: L + d_local, counted from c(n)
    /// (P114).
    pub fn local_output_delay(&self, local: Duration) -> Duration {
        self.latency() + local.min(MAX_DELAY)
    }

    /// The RTP timestamp, in a speaker's numbering, of the frame the host
    /// delivers now: the first frame of the next packet to be cut. Metadata
    /// that describes the audio arriving now applies from it (P33, P124).
    pub fn rtptime_now(&self, id: SpeakerId) -> Option<u32> {
        let speaker = self.speakers.iter().find(|s| s.id == id)?;
        Some(
            speaker
                .rtp_offset
                .wrapping_add((self.next_cut * PACKET_FRAMES) as u32),
        )
    }

    /// A speaker's latencies (P110, P113, P115).
    pub fn speaker_latency(&self, id: SpeakerId) -> Option<SpeakerLatency> {
        let speaker = self.speakers.iter().find(|s| s.id == id)?;
        Some(SpeakerLatency {
            arrival_to_sound: self.latency() + Duration::from_secs_f64(speaker.delay),
            receiver_latency_frames: speaker.reported.audio_latency_frames,
            receiver_latency_us: speaker.reported.micros(),
            receiver_latency_sources: speaker.reported.sources(),
            first_sound: self.clock.instant(speaker.first_sound),
        })
    }

    /// The next datagram to send.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<StreamEvent> {
        self.events.pop_front()
    }

    /// When [`Stream::handle_timeout`] must run next: the next cut, the next
    /// packet any speaker sends, or a pending delay flush.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let context = self.context();
        let lead = seconds(SEND_LEAD);
        let mut next = context.cut_due(self.next_cut);
        if self.frames_in() {
            next -= lead;
        }
        if let Some((at, _)) = self.step_due {
            next = next.min(at);
        }
        for copy in self.speakers.iter().filter_map(|s| s.second_copies.front()) {
            next = next.min(copy.due);
        }
        for speaker in self.speakers.iter().filter(|s| self.sending(s)) {
            if let Some((_, at)) = speaker.pending_delay
                && !speaker.delay_due
            {
                next = next.min(at);
            }
            if speaker.next_packet < self.next_cut {
                next = next.min(context.cut_due(speaker.next_packet) + speaker.delay - lead);
            }
            if let Some((_, at)) = speaker.step_sync {
                next = next.min(at);
            }
        }
        Some(self.clock.instant(next))
    }

    /// Runs everything due at `now`: takes the host's arrivals into the
    /// model, cuts the packets whose time came, marks delay changes whose
    /// merge window ended as due (P100), and sends every packet whose time came to
    /// each speaker, a packet already past its time at once (P84, P112).
    pub fn handle_timeout(&mut self, now: Instant) {
        let t = self.clock.seconds(now);
        self.take_arrivals();
        self.margin.advance(t);
        // Packets more than L past their time would be heard after their
        // own play time: rather than send them all, the timeline starts
        // over (P143).
        self.model.ensure(last_frame(self.next_cut));
        let overdue = t - (self.model.time(last_frame(self.next_cut)) + self.margin.value());
        if overdue > f64::from(self.latency_frames) / NOMINAL_RATE
            && (!self.hole_idle || overdue > IDLE_HOLE_CEILING)
        {
            self.restart_timeline(t);
            self.hole_idle = true;
        }
        if let Some((at, frames)) = self.step_due
            && at <= t
        {
            let fade = self.fade_after_step;
            self.apply_step(frames, at, true);
            if fade {
                self.fade_step(t);
            }
        }
        self.serve(t);
        if self.step_wanted.is_none()
            && self
                .clean_since
                .is_some_and(|since| t - since >= CLEAN_WINDOW)
            && !self.held()
        {
            self.step_wait_told = false;
            self.step_wanted = Some(
                self.before_step
                    .max(self.host_latency)
                    .min(self.latency_frames),
            );
        }
        let lead = seconds(SEND_LEAD);
        loop {
            self.model.ensure(last_frame(self.next_cut));
            let due = self.model.time(last_frame(self.next_cut)) + self.margin.value();
            if due > t + DUE_TOLERANCE && (due > t + lead + DUE_TOLERANCE || !self.frames_in()) {
                break;
            }
            self.cut();
            if let Some(frames) = self.step_wanted {
                if self.quiet_packets >= QUIET_PACKETS + STEP_PACKETS {
                    self.start_step(t, frames, self.next_cut - STEP_PACKETS);
                }
            } else if self.step_due.is_none()
                && let Some(to) = self.try_target(None)
            {
                // A try drops its whole amount inside the quiet run (P257 d).
                let before = if self.latency_frames.saturating_sub(to) > STEP_FRAMES {
                    TRY_QUIET_PACKETS
                } else {
                    QUIET_PACKETS
                };
                if self.quiet_packets >= before + STEP_PACKETS
                    && (to >= self.latency_frames
                        || self.start_step(t, to, self.next_cut - STEP_PACKETS))
                {
                    self.commit_tries(t, None, to);
                }
            }
        }
        let context = context!(self);
        let silence = (&*self.silence_pcm, &*self.silence_alac);
        let paused = self.paused;
        for speaker in &mut self.speakers {
            if t - speaker.rate_since >= RATE_WINDOW {
                self.events.push_back(StreamEvent::ResendRate {
                    speaker: speaker.id,
                    requests: speaker.rate_requests,
                });
                if speaker.rate_requests as usize > RATE_GAP_REQUESTS {
                    let ip = speaker.destinations.control.ip();
                    for floor in self.rate_levels.iter_mut().filter(|floor| floor.ip == ip) {
                        floor.served = 0.0;
                    }
                }
                speaker.rate_since = t;
                speaker.rate_requests = 0;
            }
            if let Some((_, at)) = speaker.step_sync
                && at <= t
            {
                speaker.step_sync = None;
                if !paused && !speaker.awaiting_flush && !speaker.first_pending {
                    send_sync(speaker, &context, false, &mut self.transmits);
                }
            }
            while let Some(copy) = speaker
                .second_copies
                .pop_front_if(|copy| copy.due <= t + DUE_TOLERANCE)
            {
                if copy.play > t {
                    self.transmits.push_back(Transmit {
                        speaker: speaker.id,
                        kind: PacketKind::Resend,
                        from: LocalSocket::Control,
                        to: speaker.destinations.control,
                        bytes: copy.bytes,
                    });
                    speaker
                        .counters
                        .second_copies_sent
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    speaker
                        .counters
                        .second_copies_skipped
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            if paused || speaker.awaiting_flush || speaker.awaiting_latency {
                continue;
            }
            if let Some((_, at)) = speaker.pending_delay
                && at <= t + DUE_TOLERANCE
            {
                speaker.delay_due = true;
            }
            while speaker.next_packet < context.next_cut {
                let due = context.cut_due(speaker.next_packet) + speaker.delay;
                if due > t + lead + DUE_TOLERANCE {
                    break;
                }
                send(speaker, &context, silence, t - due, &mut self.transmits);
            }
        }
        self.publish();
    }

    /// Whether every frame of the next packet to cut has come from the host.
    fn frames_in(&self) -> bool {
        let end = ((self.next_cut + 1) * PACKET_FRAMES) as i64 + self.origin;
        u64::try_from(end).is_ok_and(|end| self.input.written() >= end)
    }

    /// What a gap does, without waiting for a quiet window: the next packet
    /// cut fades out, the 4 after it are silent, and the step falls at the
    /// faded packet's end. Hosts never call it; tests use it to step on a
    /// programme that never goes quiet.
    #[doc(hidden)]
    pub fn step_up(&mut self, now: Instant) {
        let t = self.clock.seconds(now);
        self.note_gap(t, false);
        self.fade_step(t);
    }

    /// Starts a wanted step without a quiet window: the next packet cut
    /// fades out and the 4 after it are silent (P200).
    fn fade_step(&mut self, t: f64) {
        let fade = self.next_cut;
        self.fade_after_step = self.step_due.is_some();
        if let Some(frames) = self.step_wanted
            && self.start_step(t, frames, fade)
        {
            self.silence_until = fade + 1 + STEP_SILENCE_PACKETS;
        }
    }

    /// A speaker played without a packet (P200): the session's one step up
    /// is wanted, or after it the clean window starts again. A resend rate
    /// (`rate`) wants the step to the ceiling while Lf is below it (P254).
    fn note_gap(&mut self, t: f64, rate: bool) {
        let ceiling = (self.host_latency + RATE_STEP_FRAMES).min(MAX_LATENCY_FRAMES);
        // A step down already on its way counts; a step up on its way still
        // takes the receiver among those that asked.
        let to = self
            .step_due
            .map_or(self.latency_frames, |(_, frames)| frames);
        if rate && self.latency_frames.min(to) < ceiling {
            self.stepped_up = true;
            self.step_wait_told = false;
            self.step_wanted = Some(ceiling);
        } else if self.clean_since.is_some() {
            self.clean_since = Some(t);
            self.step_wanted = None;
        } else if !self.stepped_up {
            self.stepped_up = true;
            self.step_wait_told = false;
            self.step_wanted = Some((self.latency_frames + STEP_FRAMES).min(MAX_LATENCY_FRAMES));
        }
    }

    /// Starts a step to `frames` while a speaker sounds (P200), true when
    /// it did. The step applies on the same timeline, with no flush, when
    /// the end of `packet` sounds at the lower of the two Lf.
    fn start_step(&mut self, t: f64, frames: u32, packet: u64) -> bool {
        if self.step_due.is_some() || !self.sounding(t) {
            return false;
        }
        if frames == self.latency_frames {
            self.step_wanted = None;
            return false;
        }
        self.model.ensure(last_frame(packet));
        // Down, the speakers skip the 0.1 s before the packet's end, frames
        // of the quiet run already cut, rather than audio to come.
        let at = self.model.time(last_frame(packet))
            + f64::from(frames.min(self.latency_frames)) / NOMINAL_RATE;
        // The packet's end is 5 packets (40 ms) before the newest cut, which
        // left M less SEND_LEAD before its time: the step only fits while M
        // stays under the lower Lf less about 40 ms, 60 ms at 0.1 s.
        if at <= t {
            if !self.step_wait_told {
                self.step_wait_told = true;
                self.events.push_back(StreamEvent::StepWaiting {
                    margin: Duration::from_secs_f64(self.margin.value()),
                });
            }
            return false;
        }
        self.step_wanted = None;
        if frames < self.latency_frames {
            self.clean_since = None;
        }
        self.step_due = Some((at, frames));
        self.input.announce_move(TimelineMove {
            position: ((packet * PACKET_FRAMES) as i64 + self.origin).max(0) as u64,
            frames: i64::from(frames) - i64::from(self.latency_frames),
        });
        true
    }

    /// Sets the stepped Lf at `at`, which the local outputs follow with the
    /// timeline event; with `sync`, each speaker gets its sync stating it
    /// when the end of the step's packet sounds there, at `at` plus its
    /// delay (P200).
    fn apply_step(&mut self, frames: u32, at: f64, sync: bool) {
        self.step_due = None;
        self.fade_after_step = false;
        let old = self.latency_frames;
        self.latency_frames = frames;
        if frames > old {
            for ip in std::mem::take(&mut self.askers) {
                match self.rate_levels.iter_mut().find(|floor| floor.ip == ip) {
                    Some(floor) => {
                        floor.frames = frames;
                        floor.before = old;
                        floor.served = 0.0;
                    }
                    None => self.rate_levels.push(Floor {
                        ip,
                        frames,
                        before: old,
                        wait: self.first_wait,
                        served: 0.0,
                        watch: None,
                    }),
                }
            }
            self.step_floor = frames;
            self.before_step = old;
            self.clean_since = Some(at);
            if self.pending_latency.is_some_and(|held| held <= frames) {
                self.pending_latency = None;
            }
        } else {
            self.step_floor = self.floor_in_stream();
        }
        for speaker in &mut self.speakers {
            speaker.step_sync = sync.then_some((old, at + speaker.delay));
            speaker.second_copies.clear();
        }
        self.events.push_back(StreamEvent::Timeline {
            latency: self.latency(),
            origin: self.origin,
            restarted: false,
        });
    }

    /// Adds a speaker whose RECORD succeeded (P103): it starts with the
    /// next packet in its regular schedule, preceded by a first-flagged
    /// sync, and its first sound time is reported (P115).
    pub fn add_speaker(&mut self, config: SpeakerConfig, now: Instant) -> SpeakerId {
        let counters = Arc::new(self.new_speaker_counters());
        self.add_speaker_with_counters(config, counters, now)
    }

    /// Counters for a speaker of this stream, all at zero, to pass to
    /// [`Stream::add_speaker_with_counters`].
    pub fn new_speaker_counters(&self) -> SpeakerCounters {
        SpeakerCounters::for_stream(self.counters.clone())
    }

    /// Adds a speaker like [`Stream::add_speaker`], counting into
    /// `counters`. A host that reconnects a speaker passes the same counters
    /// to every session, so its handle stays valid and the counts run on
    /// (P144).
    pub fn add_speaker_with_counters(
        &mut self,
        config: SpeakerConfig,
        counters: Arc<SpeakerCounters>,
        now: Instant,
    ) -> SpeakerId {
        self.handle_timeout(now);
        let t = self.clock.seconds(now);
        // A receiver whose late request or resend rate stepped this stream
        // joins at that level again while nothing sounds; next to a sounding
        // speaker its rate steps again instead (P200, P201).
        let ip = config.destinations.control.ip();
        // A due try falls here, before the stream's first sound (P257 a).
        if !self.sounding(t)
            && let Some(to) = self.try_target(Some(ip))
        {
            self.commit_tries(t, Some(ip), to);
            if self.speakers.is_empty() {
                self.set_latency(self.host_latency, now);
            }
        } else if let Some(&Floor { frames, before, .. }) =
            self.rate_levels.iter().find(|floor| floor.ip == ip)
            && frames > self.step_floor
            && !self.sounding(t)
        {
            self.step_floor = frames;
            self.stepped_up = true;
            self.before_step = before;
            self.clean_since = Some(t);
            // A new stream starts there, before its first sound.
            if self.speakers.is_empty() {
                self.set_latency(self.host_latency, now);
            }
        }
        let id = SpeakerId(self.next_speaker);
        self.next_speaker += 1;
        let codec = config.codec;
        let own = match (config.software_volume, config.software_mute) {
            (Some(volume), _) => Some(OwnEncoding {
                gain: audio::software_gain(volume),
                mute_only: false,
                store: PayloadStore::new(STORE_PACKETS, payload_bytes(codec)),
            }),
            (None, true) => Some(OwnEncoding {
                gain: if config.muted { 0.0 } else { 1.0 },
                mute_only: true,
                store: PayloadStore::new(STORE_PACKETS, payload_bytes(codec)),
            }),
            (None, false) => None,
        };
        let mut speaker = Speaker {
            id,
            codec,
            destinations: config.destinations,
            ssrc: config.ssrc,
            first_rtptime: config.first_rtptime,
            delay: clamp_delay(config.delay),
            pending_delay: None,
            delay_due: false,
            next_packet: 0,
            next_seq: config.first_seq,
            rtp_offset: 0,
            first_pending: true,
            step_sync: None,
            recent_requests: VecDeque::with_capacity(RATE_WINDOW as usize),
            rate_since: t,
            rate_requests: 0,
            started: false,
            awaiting_flush: false,
            awaiting_latency: self.host_answers_latency
                && !self.speakers.iter().any(|speaker| speaker.started),
            sync_seq: 0,
            history: vec![Sent::default(); HISTORY_PACKETS].into_boxed_slice(),
            own,
            reported: config.reported_latency,
            shortfall_told: false,
            first_sound: t,
            counters,
            sealed: config
                .airplay2
                .as_ref()
                .map(|ap2| SealedStore::new(&ap2.key, codec)),
            clock_identity: config.airplay2.as_ref().and_then(|ap2| ap2.clock_identity),
            second_copies: VecDeque::with_capacity(MAX_SECOND_COPIES),
        };
        if self.speakers.is_empty() {
            // Nothing was encoded while the stream had no speaker (P251).
            let mut frames = [[0i16; 2]; FRAMES_PER_PACKET];
            let oldest = self
                .next_cut
                .saturating_sub((DELAY_PACKETS + MARGIN_PACKETS) as u64);
            for packet in oldest..self.next_cut {
                let number = self.packet_base + packet;
                if let Some((payload, silent)) = self.pcm.get(number) {
                    audio::read_pcm_payload(payload, &mut frames);
                    encode(Codec::Alac, &frames, &mut self.alac, number, silent);
                    self.counters
                        .packets_encoded
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let context = context!(self);
        let first = context.first_due(speaker.delay, t);
        speaker.start_at(first, speaker.first_rtptime);
        if let Some(own) = &mut speaker.own {
            // Packets cut before the speaker joined get its encoding too, as
            // far back as a delay can reach, so a later delay increase finds
            // them (P100, P121).
            let mut frames = [[0i16; 2]; FRAMES_PER_PACKET];
            let oldest = self
                .next_cut
                .saturating_sub((DELAY_PACKETS + MARGIN_PACKETS) as u64);
            for packet in oldest..self.next_cut {
                let number = self.packet_base + packet;
                if let Some((payload, silent)) = self.pcm.get(number) {
                    audio::read_pcm_payload(payload, &mut frames);
                    audio::apply_gain(&mut frames, own.gain);
                    encode(codec, &frames, &mut own.store, number, silent);
                }
            }
        }
        if !self.paused {
            report_first_sound(&mut speaker, &context, &mut self.events);
        }
        let sounding = self.sounding(t);
        self.speakers.push(speaker);
        if sounding {
            self.tell_shortfalls();
        }
        id
    }

    /// Removes a speaker, as its session stops playing. The last one to
    /// leave lets a held Lf apply (P201).
    pub fn remove_speaker(&mut self, id: SpeakerId) -> bool {
        let before = self.speakers.len();
        self.speakers.retain(|speaker| speaker.id != id);
        if self.speakers.is_empty() {
            self.step_floor = 0;
            self.step_wanted = None;
            self.stepped_up = false;
            self.clean_since = None;
        }
        if self.speakers.is_empty()
            && let Some(frames) = self.pending_latency.take()
            && frames != self.latency_frames
        {
            self.latency_frames = frames;
            self.events.push_back(StreamEvent::Timeline {
                latency: self.latency(),
                origin: self.origin,
                restarted: false,
            });
        }
        self.speakers.len() != before
    }

    /// Sets a speaker's delay d_s, clamped to 0 to 20 000 ms (P99). A speaker
    /// that has sent nothing yet just starts later or sooner; a playing one
    /// changes through a flush (P100) 250 ms after the last of a burst of
    /// changes, and plays on under its old delay until its session can send
    /// that FLUSH; a change made while it waits replaces the waiting one.
    pub fn set_delay(&mut self, id: SpeakerId, delay: Duration, now: Instant) -> bool {
        self.handle_timeout(now);
        let t = self.clock.seconds(now);
        let delay = clamp_delay(delay);
        let Some(index) = self.speakers.iter().position(|s| s.id == id) else {
            return false;
        };
        let context = context!(self);
        let speaker = &mut self.speakers[index];
        if self.paused && !speaker.started {
            // The resume places it (P213).
            speaker.delay = delay;
            speaker.pending_delay = None;
        } else if !speaker.started {
            speaker.delay = delay;
            speaker.pending_delay = None;
            let first = context.first_due(delay, t);
            speaker.start_at(first, speaker.first_rtptime);
            report_first_sound(speaker, &context, &mut self.events);
        } else if speaker.delay_due {
            speaker.pending_delay = Some((delay, t));
        } else if delay == speaker.delay {
            speaker.pending_delay = None;
        } else {
            speaker.pending_delay = Some((delay, t + seconds(DELAY_MERGE)));
        }
        true
    }

    /// The speakers whose delay change waits for its FLUSH (P100): the
    /// driver calls [`Stream::apply_delay`] once their session can send it
    /// at once.
    pub fn delays_due(&self) -> Vec<SpeakerId> {
        self.speakers
            .iter()
            .filter(|speaker| speaker.delay_due && self.sending(speaker))
            .map(|speaker| speaker.id)
            .collect()
    }

    /// Applies a due delay change as its FLUSH goes out (P100): the FLUSH
    /// names the first packet under the new delay, which waits for the
    /// answer ([`Stream::flush_answered`]).
    pub fn apply_delay(&mut self, id: SpeakerId, now: Instant) {
        let t = self.clock.seconds(now);
        let context = context!(self);
        let paused = self.paused;
        let Some(speaker) = self.speakers.iter_mut().find(|speaker| {
            speaker.id == id && speaker.delay_due && !paused && !speaker.awaiting_flush
        }) else {
            return;
        };
        speaker.delay_due = false;
        if let Some((delay, _)) = speaker.pending_delay.take() {
            speaker.delay = delay;
            speaker.awaiting_flush = true;
            flush(speaker, &context, t, &mut self.events);
        }
    }

    /// Sets the volume a speaker with `sv=true` or `sm=true` applies to its
    /// samples (P121), for the packets cut from now on: the level and mute
    /// with `sv`, mute alone with `sm`. False for any other speaker.
    pub fn set_software_volume(&mut self, id: SpeakerId, volume: Volume) -> bool {
        let own = self
            .speakers
            .iter_mut()
            .find(|s| s.id == id)
            .and_then(|s| s.own.as_mut());
        match own {
            Some(own) => {
                own.set(volume);
                true
            }
            None => false,
        }
    }

    /// Sets the linear gain every packet cut from now on is scaled by,
    /// once and before any encoding, so it reaches every speaker (P80).
    pub fn set_master_gain(&mut self, gain: f64) {
        self.master_gain = gain;
        // Nothing was played at the old gain, so there is nothing to fade from.
        if self.packet_base == 0 && self.next_cut == 0 {
            self.cut_gain = gain;
        }
    }

    /// Sets Lf, clamped to 4410 to 88 200 frames (P98). While no speaker
    /// has reached its first-sound time a change flushes every speaker:
    /// each goes on with its next packet, flagged as the first, and states
    /// the new latency from then on. Once one sounds the value is held
    /// until the stream has no speaker or restarts (P201).
    pub fn set_latency(&mut self, frames: u32, now: Instant) {
        let frames = frames.clamp(MIN_LATENCY_FRAMES, MAX_LATENCY_FRAMES);
        self.host_latency = frames;
        let frames = frames.max(self.step_floor);
        self.handle_timeout(now);
        let t = self.clock.seconds(now);
        let mut answered = Vec::new();
        for speaker in &mut self.speakers {
            if std::mem::take(&mut speaker.awaiting_latency) {
                answered.push(speaker.id);
            }
        }
        if self.paused || self.sounding(t) {
            if frames == self.latency_frames {
                self.pending_latency = None;
            } else if self.pending_latency != Some(frames) {
                self.pending_latency = Some(frames);
                self.events.push_back(StreamEvent::LatencyHeld { frames });
                self.tell_shortfalls();
            }
            return;
        }
        self.pending_latency = None;
        if frames == self.latency_frames {
            let context = context!(self);
            for speaker in &mut self.speakers {
                if answered.contains(&speaker.id) {
                    let first = context.first_due(speaker.delay, t);
                    speaker.start_at(first, speaker.first_rtptime);
                    report_first_sound(speaker, &context, &mut self.events);
                }
            }
            return;
        }
        self.latency_frames = frames;
        let latency = self.latency();
        let context = context!(self);
        for speaker in &mut self.speakers {
            if speaker.started {
                let rtptime = speaker.next_rtptime();
                let packet = speaker.next_packet;
                speaker.start_at(packet, rtptime);
                speaker.second_copies.clear();
                self.events.push_back(StreamEvent::Flush {
                    speaker: speaker.id,
                    seq: speaker.next_seq,
                    rtptime,
                });
            } else {
                let first = context.first_due(speaker.delay, t);
                speaker.start_at(first, speaker.first_rtptime);
            }
            report_first_sound(speaker, &context, &mut self.events);
        }
        self.events.push_back(StreamEvent::Timeline {
            latency,
            origin: self.origin,
            restarted: false,
        });
    }

    /// Starts the timeline over at `now`, as a hole longer than L does
    /// (P143): every speaker that has sent audio is flushed and waits for
    /// the FLUSH answer, the frame count starts again at 0, the rate model
    /// starts over, and frames still in the input ring are dropped. The
    /// speakers fall silent for about L and come back in step. A capture
    /// switch never needs it ([`crate::input::InputWriter::new_capture`]).
    pub fn restart(&mut self, now: Instant) {
        self.handle_timeout(now);
        let t = self.clock.seconds(now);
        self.restart_timeline(t);
        self.publish();
    }

    /// Starts the timeline over at `t` seconds (P143).
    fn restart_timeline(&mut self, t: f64) {
        if let Some((_, frames)) = self.step_due {
            self.apply_step(frames, t, false);
        }
        if let Some(frames) = self.pending_latency.take() {
            self.latency_frames = frames;
        }
        self.try_in_silence(t);
        self.origin = self.input.rebase() as i64;
        self.packet_base += self.next_cut + 1;
        self.next_cut = 0;
        self.model.reset(t);
        self.margin.clear();
        self.model.ensure(last_frame(0));
        let latency = self.latency();
        let context = context!(self);
        let paused = self.paused;
        for speaker in &mut self.speakers {
            speaker.delay_due = false;
            speaker.second_copies.clear();
            if let Some((delay, _)) = speaker.pending_delay.take() {
                speaker.delay = delay;
            }
            if paused {
                // Flushed at the pause; the resume places it (P213).
                continue;
            }
            if speaker.started {
                // The new timeline's packets wait for the FLUSH answer, which
                // may queue behind another request (P143, P213).
                speaker.awaiting_flush = true;
                flush(speaker, &context, t, &mut self.events);
            } else {
                let first = context.first_due(speaker.delay, t);
                speaker.start_at(first, speaker.first_rtptime);
                report_first_sound(speaker, &context, &mut self.events);
            }
        }
        self.events.push_back(StreamEvent::Timeline {
            latency,
            origin: self.origin,
            restarted: true,
        });
    }

    /// The host paused the stream (P213): every speaker that has sent
    /// audio gets a FLUSH naming the first packet it has not sent, and no
    /// audio or sync goes out until [`Stream::resume`]. Packets already cut
    /// for later sends are dropped and earlier ones are no longer resent;
    /// the host's frames are still counted. False when already paused.
    pub fn pause(&mut self, now: Instant) -> bool {
        if self.paused {
            return false;
        }
        self.handle_timeout(now);
        self.paused = true;
        for speaker in self.speakers.iter_mut().filter(|s| s.started) {
            speaker.awaiting_flush = true;
            for sent in speaker.history.iter_mut() {
                sent.valid = false;
            }
            speaker.second_copies.clear();
            self.events.push_back(StreamEvent::Flush {
                speaker: speaker.id,
                seq: speaker.next_seq,
                rtptime: speaker.next_rtptime(),
            });
        }
        true
    }

    /// The host resumed the stream (P213): with no new request, every
    /// speaker continues from its flush point with the first whole packet
    /// at or after the first frame the host delivers from now, flagged as
    /// the first, in step with the others. Delay and Lf changes held during
    /// the pause apply here. A speaker whose pause FLUSH is unanswered
    /// waits for [`Stream::flush_answered`]. False when not paused.
    pub fn resume(&mut self, now: Instant) -> bool {
        if !self.paused {
            return false;
        }
        self.handle_timeout(now);
        self.paused = false;
        let mut latency_changed = false;
        if let Some(frames) = self.pending_latency.take()
            && frames != self.latency_frames
        {
            self.latency_frames = frames;
            latency_changed = true;
        }
        let t = self.clock.seconds(now);
        latency_changed |= self.try_in_silence(t);
        let first_frame = (self.input.written() as i64 - self.origin).max(0) as u64;
        let packet = first_frame.div_ceil(PACKET_FRAMES).max(self.next_cut);
        let context = context!(self);
        for speaker in &mut self.speakers {
            speaker.delay_due = false;
            if let Some((delay, _)) = speaker.pending_delay.take() {
                speaker.delay = delay;
            }
            let rtptime = if speaker.started {
                speaker.next_rtptime()
            } else {
                speaker.first_rtptime
            };
            speaker.start_at(packet, rtptime);
            report_first_sound(speaker, &context, &mut self.events);
        }
        if latency_changed {
            self.events.push_back(StreamEvent::Timeline {
                latency: self.latency(),
                origin: self.origin,
                restarted: false,
            });
        }
        self.publish();
        true
    }

    /// True between [`Stream::pause`] and [`Stream::resume`].
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// The speaker's session answered a FLUSH, or will send none: a resume
    /// no longer waits for it (P213).
    pub fn flush_answered(&mut self, id: SpeakerId, _now: Instant) {
        if let Some(speaker) = self.speakers.iter_mut().find(|s| s.id == id) {
            speaker.awaiting_flush = false;
        }
    }

    /// The speaker's pause or restart FLUSH is not answered yet (P143,
    /// P213).
    pub fn waits_for_flush(&self, id: SpeakerId) -> bool {
        self.speakers
            .iter()
            .any(|speaker| speaker.id == id && speaker.awaiting_flush)
    }

    /// Whether a speaker's packets are on schedule now.
    fn sending(&self, speaker: &Speaker) -> bool {
        !self.paused && !speaker.awaiting_flush
    }

    /// A packet from the receiver on a speaker's control socket: a
    /// retransmission request is answered (P86, P87, P88); anything else is
    /// ignored.
    pub fn handle_control_packet(&mut self, id: SpeakerId, bytes: &[u8], now: Instant) {
        let Some(request) = ResendRequest::parse(bytes) else {
            return;
        };
        let t = self.clock.seconds(now);
        let mut gap = false;
        let context = context!(self);
        let Some(speaker) = self.speakers.iter_mut().find(|s| s.id == id) else {
            return;
        };
        let counters = speaker.counters.clone();
        counters.resend_requests.fetch_add(1, Ordering::Relaxed);
        speaker.rate_requests += 1;
        let second = t.floor() as i64;
        let recent = &mut speaker.recent_requests;
        while recent
            .front()
            .is_some_and(|old| second - old.second >= RATE_WINDOW as i64)
        {
            recent.pop_front();
        }
        match recent.back_mut() {
            Some(last) if last.second == second => last.requests += 1,
            _ => recent.push_back(RateSecond {
                second,
                first: t,
                requests: 1,
                tight: 0,
            }),
        }
        // More than 20 within the minute, spread over 5 seconds and 10 s
        // or more: the receiver is cutting out, however fast it asks (P200).
        let requests: u32 = recent.iter().map(|s| s.requests).sum();
        let steady = requests as usize > RATE_GAP_REQUESTS
            && recent.len() >= RATE_SECONDS
            && recent.front().is_some_and(|old| t - old.first >= RATE_SPAN);
        let ip = speaker.destinations.control.ip();
        let mut tight = None;
        // Faster than the line, counted from the try and afresh after each
        // FLUSH: 6 requests within 10 s in 3 distinct seconds (P258).
        let fast = self
            .rate_levels
            .iter()
            .find_map(|floor| floor.watch.as_ref().filter(|_| floor.ip == ip))
            .is_some_and(|watch| {
                let window = recent
                    .iter()
                    .filter(|s| s.first >= watch.since && second - s.second < FAST_SPAN);
                window.clone().map(|s| s.requests).sum::<u32>() >= FAST_REQUESTS
                    && window.count() >= FAST_SECONDS
            });
        for offset in 0..request.count.min(MAX_RESEND_REQUEST) {
            let seq = request.first.wrapping_add(offset);
            let slot = usize::from(seq) % HISTORY_PACKETS;
            let entry = speaker.history[slot];
            let age = speaker.next_seq.wrapping_sub(seq);
            let in_history =
                entry.valid && entry.seq == seq && (1..=HISTORY_PACKETS as u16).contains(&age);
            let held = in_history && entry.resends < MAX_RESENDS;
            // Asked for once its play time here passed, it was played
            // without (P200); a packet of an earlier timeline never counts.
            let play = entry
                .number
                .checked_sub(context.packet_base)
                .map(|j| context.heard((j * PACKET_FRAMES) as i64, speaker.delay));
            if in_history && let Some(play) = play {
                gap |= t >= play;
                let j = entry.number - context.packet_base;
                let sent = context.cut_due(j) + speaker.delay;
                tight.get_or_insert(t < play && t >= (sent + play) / 2.0);
            }
            let bytes = match &speaker.sealed {
                // The sealed packet exactly as first sent, never sealed
                // again (P155).
                Some(sealed) => held.then(|| sealed.get(slot)).flatten().map(|packet| {
                    let mut bytes = Vec::with_capacity(RESEND_PREFIX_BYTES + packet.len());
                    bytes.extend_from_slice(&rtp::resend_prefix(seq));
                    bytes.extend_from_slice(packet);
                    bytes
                }),
                None => held
                    .then(|| context.payload(speaker.own.as_ref(), speaker.codec, entry.number))
                    .flatten()
                    .map(|(payload, _)| {
                        let header = RtpHeader {
                            marker: entry.marker,
                            seq,
                            timestamp: entry.timestamp,
                            ssrc: speaker.ssrc,
                        };
                        let mut bytes =
                            Vec::with_capacity(RESEND_PREFIX_BYTES + HEADER_BYTES + payload.len());
                        bytes.extend_from_slice(&rtp::resend_prefix(seq));
                        bytes.extend_from_slice(&header.encode());
                        bytes.extend_from_slice(payload);
                        bytes
                    }),
            };
            let Some(bytes) = bytes else {
                counters.not_held.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            // A packet of an earlier timeline has no play time left (P143).
            let play = play.unwrap_or(f64::NEG_INFINITY);
            let copies = &mut speaker.second_copies;
            copies.retain(|copy| copy.seq != seq);
            if copies.len() < MAX_SECOND_COPIES {
                copies.push_back(SecondCopy {
                    seq,
                    due: t + seconds(SECOND_COPY_SPACING),
                    play,
                    bytes: bytes.clone(),
                });
            } else {
                counters
                    .second_copies_skipped
                    .fetch_add(1, Ordering::Relaxed);
            }
            self.transmits.push_back(Transmit {
                speaker: speaker.id,
                kind: PacketKind::Resend,
                from: LocalSocket::Control,
                to: speaker.destinations.control,
                bytes,
            });
            speaker.history[slot].resends += 1;
            counters.packets_resent.fetch_add(1, Ordering::Relaxed);
        }
        // A starved receiver: 3 tight requests in the minute, in at least
        // 2 distinct seconds (P260).
        if tight == Some(true)
            && let Some(last) = speaker.recent_requests.back_mut()
        {
            last.tight += 1;
        }
        let tight_seconds = speaker.recent_requests.iter().filter(|s| s.tight > 0);
        let steady = steady
            || (tight_seconds.clone().map(|s| s.tight).sum::<u32>() >= STARVED_REQUESTS
                && tight_seconds.count() >= STARVED_SECONDS);
        if self.watched_request(t, ip, gap, fast, steady) {
            return;
        }
        if gap || steady {
            for floor in self.rate_levels.iter_mut().filter(|floor| floor.ip == ip) {
                floor.served = 0.0;
            }
        }
        // After a step down the rate may step up again.
        if steady && self.step_floor == 0 && self.step_due.is_none() && self.clean_since.is_none() {
            self.stepped_up = false;
        }
        if gap || steady {
            self.note_gap(t, steady);
            if self.step_wanted.is_some() && !self.askers.contains(&ip) {
                self.askers.push(ip);
            }
        }
        if steady {
            self.fade_step(t);
        }
    }

    /// A receiver whose gap or resend rate stepped Lf up is in the stream,
    /// so the step holds (P200).
    fn held(&self) -> bool {
        self.speakers.iter().any(|speaker| {
            let ip = speaker.destinations.control.ip();
            self.rate_levels
                .iter()
                .any(|floor| floor.ip == ip && floor.frames > 0)
        })
    }

    /// The largest floor of a receiver in the stream, 0 for none.
    fn floor_in_stream(&self) -> u32 {
        self.rate_levels
            .iter()
            .filter(|floor| self.in_stream(floor.ip))
            .map(|floor| floor.frames)
            .max()
            .unwrap_or(0)
    }

    fn in_stream(&self, ip: IpAddr) -> bool {
        self.speakers
            .iter()
            .any(|speaker| speaker.destinations.control.ip() == ip)
    }

    /// Counts the time a floor's receiver sounds in the stream toward its
    /// wait (P256), and ends each watch that passed without a failure
    /// (P258).
    fn serve(&mut self, t: f64) {
        let dt = (t - self.served_at).max(0.0);
        self.served_at = t;
        for floor in &mut self.rate_levels {
            let sounds = self.speakers.iter().any(|speaker| {
                speaker.destinations.control.ip() == floor.ip
                    && speaker.started
                    && t >= speaker.first_sound
            });
            if sounds && !self.paused && floor.watch.is_none() {
                floor.served += dt;
            }
            if floor
                .watch
                .as_ref()
                .is_some_and(|watch| t - watch.since >= self.watch)
            {
                floor.watch = None;
                floor.wait = self.first_wait;
                self.events.push_back(StreamEvent::MarginTryHeld {
                    receiver: floor.ip,
                    wait: Duration::from_secs_f64(floor.wait),
                });
            }
        }
    }

    /// A floor above the start level whose wait was served (P256).
    fn due(&self, floor: &Floor) -> bool {
        floor.frames > self.host_latency && floor.watch.is_none() && floor.served >= floor.wait
    }

    /// When a try is due for a receiver in the stream or `joining`, the Lf
    /// it goes to: the start level or a larger floor of another (P258).
    fn try_target(&self, joining: Option<IpAddr>) -> Option<u32> {
        let here = |ip: IpAddr| joining == Some(ip) || self.in_stream(ip);
        let mut due = false;
        let mut to = self.host_latency;
        for floor in self.rate_levels.iter().filter(|floor| here(floor.ip)) {
            if self.due(floor) {
                due = true;
            } else {
                to = to.max(floor.frames);
            }
        }
        due.then_some(to)
    }

    /// Every due receiver tries together, to Lf `to` (P258).
    fn commit_tries(&mut self, t: f64, joining: Option<IpAddr>, to: u32) {
        for k in 0..self.rate_levels.len() {
            let floor = &self.rate_levels[k];
            if !(self.due(floor) && (joining == Some(floor.ip) || self.in_stream(floor.ip))) {
                continue;
            }
            let floor = &mut self.rate_levels[k];
            self.events.push_back(StreamEvent::MarginTry {
                receiver: floor.ip,
                from: floor.frames,
                to,
                wait: Duration::from_secs_f64(floor.wait),
            });
            floor.watch = Some(Watch {
                since: t,
                left: floor.frames,
                requests: 0,
                late: 0,
            });
            floor.frames = 0;
            floor.served = 0.0;
        }
        self.step_floor = self.floor_in_stream();
        // Back under its floor, the receiver steps up again as P200 says.
        self.stepped_up = false;
    }

    /// A try at a point that already silences every speaker, a resume or a
    /// restart (P257 b, c): Lf goes straight to the try's level.
    fn try_in_silence(&mut self, t: f64) -> bool {
        match self.try_target(None) {
            Some(to) => {
                self.commit_tries(t, None, to);
                let lower = to < self.latency_frames;
                self.latency_frames = self.latency_frames.min(to);
                lower
            }
            None => false,
        }
    }

    /// A request from `ip` while it is watched after a try: a late one,
    /// `fast` or `steady` fail the try, and its floor goes back at once to
    /// the level the try left, by P254's fade-out (P258). True when it
    /// failed.
    fn watched_request(&mut self, t: f64, ip: IpAddr, gap: bool, fast: bool, steady: bool) -> bool {
        let Some(floor) = self
            .rate_levels
            .iter_mut()
            .find(|floor| floor.ip == ip && floor.watch.is_some())
        else {
            return false;
        };
        let watch = floor.watch.as_mut().expect("watched");
        watch.requests += 1;
        watch.late += u32::from(gap);
        if !(gap || fast || steady) {
            return false;
        }
        let watch = floor.watch.take().expect("watched");
        floor.frames = watch.left;
        floor.wait = (floor.wait * 2.0).min(self.wait_ceiling);
        floor.served = 0.0;
        self.events.push_back(StreamEvent::MarginTryFailed {
            receiver: ip,
            back_to: watch.left,
            after: Duration::from_secs_f64((t - watch.since).max(0.0)),
            requests: watch.requests,
            late: watch.late,
            next_wait: Duration::from_secs_f64(floor.wait),
        });
        self.step_floor = self.step_floor.max(watch.left);
        // A try's own step down may still be on its way.
        let to = self
            .step_due
            .map_or(self.latency_frames, |(_, frames)| frames);
        if watch.left > to {
            self.stepped_up = true;
            self.step_wait_told = false;
            self.step_wanted = Some(watch.left);
            if !self.askers.contains(&ip) {
                self.askers.push(ip);
            }
            self.fade_step(t);
        }
        true
    }

    /// Answers a timing request from `source` that reached a speaker's
    /// timing socket at `received` (P90, P91, P92): the reply, stamped with
    /// `transmit` (the sender clock read just before the send call), goes
    /// from the timing socket to the request's source, whatever port SETUP
    /// named, `timing_port=0` included. Anything that isn't a timing
    /// request gives `None` and is ignored; the sender sends no timing
    /// requests of its own. The driver also tells the speaker's session,
    /// for P131.
    pub fn answer_timing(
        &mut self,
        id: SpeakerId,
        request: &[u8],
        source: SocketAddr,
        received: Instant,
        transmit: Instant,
    ) -> Option<Transmit> {
        let reply = rtp::timing_reply(request, self.clock.ntp(received), self.clock.ntp(transmit))?;
        let speaker = self.speakers.iter().find(|s| s.id == id)?;
        speaker
            .counters
            .timing_replies
            .fetch_add(1, Ordering::Relaxed);
        Some(Transmit {
            speaker: id,
            kind: PacketKind::Timing,
            from: LocalSocket::Timing,
            to: source,
            bytes: reply.to_vec(),
        })
    }

    /// Feeds the host's arrivals to the model and M. A resumed capture's
    /// first write (the writer moved up to the floor) is placed where its
    /// frames would be had they just arrived on schedule: its last frame at
    /// the n whose c(n) is its arrival, never before the first frame not
    /// yet cut. The frames in between stay silence the stream inserted
    /// (P96, P112), and the rate model and M see the resumed audio on time.
    fn take_arrivals(&mut self) {
        let floor = self.input.floor();
        let first_uncut = (self.next_cut * PACKET_FRAMES) as i64;
        let mut origin = self.origin;
        let mut resumed = false;
        let mut arrived = false;
        let model = &mut self.model;
        let margin = &mut self.margin;
        self.input.drain_arrivals(|arrival| {
            arrived = true;
            let at = arrival.nanos as f64 / 1e9;
            // Unless the stream already cut into those frames under the
            // old placement.
            if arrival.resync && floor <= arrival.start {
                let count = (arrival.end - arrival.start) as i64;
                let first = (model.frame_at(at) - count + 1).max(first_uncut);
                origin = arrival.start as i64 - first;
                model.forget_arrivals();
                resumed = true;
            }
            let end = arrival.end as i64 - origin;
            if end <= 0 {
                return;
            }
            let start = (arrival.start as i64 - origin).max(0);
            if arrival.late {
                // A late block tells M how late the host can be (P84), but
                // its frames were dropped, so it stays out of the rate fit.
                // A residual longer than M can ever be is a pause, not
                // jitter, and doesn't count.
                let residual = model.residual(end, at);
                if residual <= MAX_MARGIN {
                    margin.observe(at, residual, (end - start) as f64 / NOMINAL_RATE);
                }
                return;
            }
            let (residual, block) = model.observe(start, end, at);
            margin.observe(at, residual, block);
        });
        self.origin = origin;
        self.hole_idle &= !arrived;
        if resumed {
            self.events.push_back(StreamEvent::Timeline {
                latency: self.latency(),
                origin,
                restarted: false,
            });
        }
    }

    /// Cuts and encodes the next packet of the timeline (P80, P112).
    fn cut(&mut self) {
        let start = (self.next_cut * PACKET_FRAMES) as i64 + self.origin;

        let silent = self.input.cut(start, &mut self.frames);
        if self.next_cut < self.silence_until {
            audio::ramp_gain(&mut self.frames, self.cut_gain, 0.0);
            self.cut_gain = 0.0;
        } else if self.cut_gain == self.master_gain {
            audio::apply_gain(&mut self.frames, self.master_gain);
        } else {
            audio::ramp_gain(&mut self.frames, self.cut_gain, self.master_gain);
            self.cut_gain = self.master_gain;
        }
        let peak = self.frames.iter().flatten().map(|s| s.unsigned_abs()).max();
        self.quiet_packets = match peak {
            Some(peak) if peak >= QUIET_PEAK => 0,
            _ => self.quiet_packets + 1,
        };
        let number = self.packet_base + self.next_cut;
        encode(Codec::Pcm, &self.frames, &mut self.pcm, number, silent);
        if !self.speakers.is_empty() {
            encode(Codec::Alac, &self.frames, &mut self.alac, number, silent);
            self.counters
                .packets_encoded
                .fetch_add(1, Ordering::Relaxed);
        }
        for speaker in &mut self.speakers {
            if let Some(own) = &mut speaker.own {
                let mut frames = self.frames;
                audio::apply_gain(&mut frames, own.gain);
                encode(speaker.codec, &frames, &mut own.store, number, silent);
            }
        }
        self.counters
            .silent_frames
            .fetch_add(u64::from(silent), Ordering::Relaxed);
        self.next_cut += 1;
    }

    fn publish(&mut self) {
        self.counters
            .rate_bits
            .store(self.model.rate().to_bits(), Ordering::Relaxed);
        let dropped = self.input.dropped();
        self.counters
            .input_frames_dropped
            .store(dropped, Ordering::Relaxed);
    }
}
