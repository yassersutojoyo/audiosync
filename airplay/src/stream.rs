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
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::alac;
use crate::ap2::audio::{PacketSealer, SEALED_OVERHEAD, StreamKey};
use crate::audio::{self, FRAMES_PER_PACKET, PACKET_FRAMES, PCM_PAYLOAD_BYTES, PacketFrames};
use crate::clock::SenderClock;
use crate::input::{self, InputReader, InputWriter};
use crate::latency::{self, LatencySources, ReportedLatency};
use crate::rate::{MAX_MARGIN, Margin, NOMINAL_RATE, RateModel};

use crate::rtp::{
    self, HEADER_BYTES, RESEND_PREFIX_BYTES, ResendRequest, RtpHeader, SyncPacket, TimeAnnounce,
};
use crate::sdp::Codec;
use crate::session::MediaDestinations;
use crate::store::{DELAY_PACKETS, HISTORY_PACKETS, MARGIN_PACKETS, PayloadStore, STORE_PACKETS};
use crate::volume::Volume;

/// The base latency Lf by default: 77 175 frames, 1.75 s (P98).
pub const DEFAULT_LATENCY_FRAMES: u32 = 77_175;
/// The smallest Lf the host may set: 0.25 s (P98).
pub const MIN_LATENCY_FRAMES: u32 = 11_025;
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
/// A packet sent later than this after its time counts as late (P84, P144).
pub const LATE_SEND: Duration = Duration::from_millis(5);
/// The input ring by default: 2 s (at least 1 s, P111).
pub const DEFAULT_INPUT_FRAMES: usize = 2 * audio::SAMPLE_RATE as usize;
/// Times within this many seconds of being due count as due, so a wake-up
/// at an instant rounded to the clock's resolution finds its work.
const DUE_TOLERANCE: f64 = 1e-6;

/// How a stream starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Lf, clamped to 11 025 to 88 200 frames (P98).
    pub latency_frames: u32,
    /// The input ring's size in frames, at least 1 s (P111).
    pub input_frames: usize,
}

impl Default for StreamConfig {
    fn default() -> StreamConfig {
        StreamConfig {
            latency_frames: DEFAULT_LATENCY_FRAMES,
            input_frames: DEFAULT_INPUT_FRAMES,
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
    /// otherwise (P121). It starts unmuted.
    pub software_mute: bool,
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
    timing_replies: AtomicU64,
    late_packets: AtomicU64,
    silent_frames: AtomicU64,
    foreign_datagrams: AtomicU64,
    stream: Arc<StreamCounters>,
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

    /// Packets cut and encoded: one per packet of the timeline, however
    /// many speakers play it (P80).
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
    /// The next timeline packet to send.
    next_packet: u64,
    next_seq: u16,
    /// k_s: frame n has RTP timestamp k_s + n (P81).
    rtp_offset: u32,
    /// The next audio packet carries the marker and the next sync the
    /// first flag (P78, P93).
    first_pending: bool,
    /// Anything sent since RECORD.
    started: bool,
    /// The FLUSH of an explicit pause or a timeline restart is not answered
    /// yet; a resume or the new timeline waits for it (P143, P213).
    awaiting_flush: bool,
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
        let latency = context.latency_frames;
        let heard = (packet * PACKET_FRAMES) as i64 - i64::from(latency);
        let at = context.heard(heard, speaker.delay);
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

/// One stream to many speakers.
#[derive(Debug)]
pub struct Stream {
    clock: SenderClock,
    latency_frames: u32,
    /// An Lf the host set while a speaker sounds (P201).
    pending_latency: Option<u32>,
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
    /// PCM payloads, which are also the source of own encodings (P121).
    pcm: PayloadStore,
    alac: PayloadStore,
    silence_pcm: Box<[u8]>,
    silence_alac: Box<[u8]>,
    speakers: Vec<Speaker>,
    /// The host paused the stream (P213): packets are cut but not sent.
    paused: bool,
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
            input: reader,
            model: RateModel::new(start),
            margin: Margin::new(start),
            origin: 0,
            packet_base: 0,
            next_cut: 0,
            frames: silence,
            pcm: PayloadStore::new(STORE_PACKETS, PCM_PAYLOAD_BYTES),
            alac: PayloadStore::new(STORE_PACKETS, alac::FRAME_BYTES),
            silence_pcm,
            silence_alac: Box::new(silence_alac),
            speakers: Vec::new(),
            paused: false,
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
        let mut next = context.cut_due(self.next_cut);
        for speaker in self.speakers.iter().filter(|s| self.sending(s)) {
            if let Some((_, at)) = speaker.pending_delay {
                next = next.min(at);
            }
            if speaker.next_packet < self.next_cut {
                next = next.min(context.cut_due(speaker.next_packet) + speaker.delay);
            }
        }
        Some(self.clock.instant(next))
    }

    /// Runs everything due at `now`: takes the host's arrivals into the
    /// model, cuts the packets whose time came, applies delay changes whose
    /// merge window ended (P100), and sends every packet whose time came to
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
        if overdue > f64::from(self.latency_frames) / NOMINAL_RATE {
            self.restart_timeline(t);
        }
        loop {
            self.model.ensure(last_frame(self.next_cut));
            if self.model.time(last_frame(self.next_cut)) + self.margin.value() > t + DUE_TOLERANCE
            {
                break;
            }
            self.cut();
        }
        let context = context!(self);
        let silence = (&*self.silence_pcm, &*self.silence_alac);
        let paused = self.paused;
        for speaker in &mut self.speakers {
            if paused || speaker.awaiting_flush {
                continue;
            }
            if let Some((delay, at)) = speaker.pending_delay
                && at <= t + DUE_TOLERANCE
            {
                speaker.pending_delay = None;
                speaker.delay = delay;
                flush(speaker, &context, t, &mut self.events);
            }
            while speaker.next_packet < context.next_cut {
                let due = context.cut_due(speaker.next_packet) + speaker.delay;
                if due > t + DUE_TOLERANCE {
                    break;
                }
                send(speaker, &context, silence, t - due, &mut self.transmits);
            }
        }
        self.publish();
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
                gain: 1.0,
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
            next_packet: 0,
            next_seq: config.first_seq,
            rtp_offset: 0,
            first_pending: true,
            started: false,
            awaiting_flush: false,
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
        };
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
    /// changes, and plays on under its old delay until then.
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
        } else if delay == speaker.delay {
            speaker.pending_delay = None;
        } else {
            speaker.pending_delay = Some((delay, t + seconds(DELAY_MERGE)));
        }
        true
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

    /// Sets Lf, clamped to 11 025 to 88 200 frames (P98). While no speaker
    /// has reached its first-sound time a change flushes every speaker:
    /// each goes on with its next packet, flagged as the first, and states
    /// the new latency from then on. Once one sounds the value is held
    /// until the stream has no speaker or restarts (P201).
    pub fn set_latency(&mut self, frames: u32, now: Instant) {
        let frames = frames.clamp(MIN_LATENCY_FRAMES, MAX_LATENCY_FRAMES);
        self.handle_timeout(now);
        let t = self.clock.seconds(now);
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
        if let Some(frames) = self.pending_latency.take() {
            self.latency_frames = frames;
        }
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
        let first_frame = (self.input.written() as i64 - self.origin).max(0) as u64;
        let packet = first_frame.div_ceil(PACKET_FRAMES).max(self.next_cut);
        let context = context!(self);
        for speaker in &mut self.speakers {
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
    pub fn flush_answered(&mut self, id: SpeakerId) {
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
    pub fn handle_control_packet(&mut self, id: SpeakerId, bytes: &[u8], _now: Instant) {
        let Some(request) = ResendRequest::parse(bytes) else {
            return;
        };
        let context = context!(self);
        let Some(speaker) = self.speakers.iter_mut().find(|s| s.id == id) else {
            return;
        };
        let counters = speaker.counters.clone();
        counters.resend_requests.fetch_add(1, Ordering::Relaxed);
        for offset in 0..request.count.min(MAX_RESEND_REQUEST) {
            let seq = request.first.wrapping_add(offset);
            let slot = usize::from(seq) % HISTORY_PACKETS;
            let entry = speaker.history[slot];
            let age = speaker.next_seq.wrapping_sub(seq);
            let in_history =
                entry.valid && entry.seq == seq && (1..=HISTORY_PACKETS as u16).contains(&age);
            let held = in_history && entry.resends < MAX_RESENDS;
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
        let model = &mut self.model;
        let margin = &mut self.margin;
        self.input.drain_arrivals(|arrival| {
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
        let number = self.packet_base + self.next_cut;
        encode(Codec::Pcm, &self.frames, &mut self.pcm, number, silent);
        encode(Codec::Alac, &self.frames, &mut self.alac, number, silent);
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
        self.counters
            .packets_encoded
            .fetch_add(1, Ordering::Relaxed);
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
