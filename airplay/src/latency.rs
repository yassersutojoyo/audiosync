//! The latency a receiver reports and the base latency the host derives
//! from it (P196 to P202).
//!
//! A receiver may name its own latency in three places: `Audio-Latency` on
//! a RECORD answer (frames, P196), `audioLatencies` in `/info`
//! (microseconds, P197) and `arrivalToRenderLatencyMs` in the AirPlay 2
//! stream SETUP answer (P198). A session's R_s is the largest of them
//! (P199), 0 when none gave a value. The crate reports R_s and never
//! applies it by itself; the host turns the R_s of every speaker into one
//! Lf with [`session_latency_frames`] (P200).

use plist::{Dictionary, Value};

use crate::stream::{DEFAULT_LATENCY_FRAMES, MAX_LATENCY_FRAMES, MIN_LATENCY_FRAMES};

/// The stream's sample rate, which `Audio-Latency` counts in (P196).
const FRAMES_PER_SECOND: u64 = 44_100;
const MICROS_PER_SECOND: u64 = 1_000_000;

/// The room P200 leaves on top of the largest R_s: 22 050 frames, 0.5 s.
pub const LATENCY_MARGIN_FRAMES: u32 = 22_050;

/// Which of the three reports gave a value, as the C interface numbers
/// them (P202).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LatencySources(u8);

impl LatencySources {
    pub const NONE: LatencySources = LatencySources(0);
    /// `Audio-Latency` of a RECORD answer (P196).
    pub const AUDIO_LATENCY: LatencySources = LatencySources(1);
    /// `audioLatencies` of `/info` (P197).
    pub const AUDIO_LATENCIES: LatencySources = LatencySources(2);
    /// `arrivalToRenderLatencyMs` of the stream SETUP answer (P198).
    pub const ARRIVAL_TO_RENDER: LatencySources = LatencySources(4);

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn contains(self, other: LatencySources) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn with(self, other: LatencySources) -> LatencySources {
        LatencySources(self.0 | other.0)
    }
}

/// What one session's receiver reported, each value as it came (P196 to
/// P198). A value that couldn't be read is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportedLatency {
    /// `Audio-Latency`, in frames at 44 100 Hz.
    pub audio_latency_frames: Option<u32>,
    /// The largest usable `outputLatencyMicros` of the last `/info`.
    pub audio_latencies_us: Option<u64>,
    /// `arrivalToRenderLatencyMs`, converted to microseconds.
    pub arrival_to_render_us: Option<u64>,
}

impl ReportedLatency {
    /// Only an `Audio-Latency` header, as on the AirPlay 1 path.
    pub fn from_audio_latency(frames: Option<u32>) -> ReportedLatency {
        ReportedLatency {
            audio_latency_frames: frames,
            ..ReportedLatency::default()
        }
    }

    /// R_s: the largest value, in microseconds, or 0 when none came
    /// (P199).
    pub fn micros(&self) -> u64 {
        [
            self.audio_latency_frames.map(frames_to_micros),
            self.audio_latencies_us,
            self.arrival_to_render_us,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(0)
    }

    /// The reports that gave a value (P199, P202).
    pub fn sources(&self) -> LatencySources {
        let mut sources = LatencySources::NONE;
        if self.audio_latency_frames.is_some() {
            sources = sources.with(LatencySources::AUDIO_LATENCY);
        }
        if self.audio_latencies_us.is_some() {
            sources = sources.with(LatencySources::AUDIO_LATENCIES);
        }
        if self.arrival_to_render_us.is_some() {
            sources = sources.with(LatencySources::ARRIVAL_TO_RENDER);
        }
        sources
    }
}

/// Frames at 44 100 Hz as microseconds, rounded (P199).
pub fn frames_to_micros(frames: u32) -> u64 {
    (u64::from(frames) * MICROS_PER_SECOND + FRAMES_PER_SECOND / 2) / FRAMES_PER_SECOND
}

/// Microseconds as frames at 44 100 Hz, rounded up, since R_s is read as a
/// lower bound (P200).
pub fn micros_to_frames(micros: u64) -> u64 {
    let frames = u128::from(micros) * u128::from(FRAMES_PER_SECOND);
    u64::try_from(frames.div_ceil(u128::from(MICROS_PER_SECOND))).unwrap_or(u64::MAX)
}

/// The Lf one speaker with R_s `micros` would need alone (P200).
pub fn latency_needed_frames(micros: u64) -> u32 {
    let wanted = micros_to_frames(micros).saturating_add(u64::from(LATENCY_MARGIN_FRAMES));
    let floored = wanted.max(u64::from(DEFAULT_LATENCY_FRAMES));
    floored.clamp(u64::from(MIN_LATENCY_FRAMES), u64::from(MAX_LATENCY_FRAMES)) as u32
}

/// The stream's Lf from every speaker's R_s in microseconds:
/// `clamp(max(77 175, R_max + 22 050), 11 025, 88 200)` frames, 77 175 for
/// an empty list (P200).
pub fn session_latency_frames<I>(reported_us: I) -> u32
where
    I: IntoIterator<Item = u64>,
{
    reported_us
        .into_iter()
        .max()
        .map_or(DEFAULT_LATENCY_FRAMES, latency_needed_frames)
}

/// True when Lf would have to exceed 88 200 frames for R_s `micros` (the
/// warning of P200).
pub fn exceeds_largest_latency(micros: u64) -> bool {
    micros_to_frames(micros).saturating_add(u64::from(LATENCY_MARGIN_FRAMES))
        > u64::from(MAX_LATENCY_FRAMES)
}

/// A finite, non-negative integer or real, as a real.
fn non_negative(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Integer(integer) => integer
            .as_unsigned()
            .map(|v| v as f64)
            .or_else(|| integer.as_signed().map(|v| v as f64))?,
        Value::Real(real) => *real,
        _ => return None,
    };
    (number.is_finite() && number >= 0.0).then_some(number)
}

/// `audioLatencies` of an `/info` dictionary: the largest usable
/// `outputLatencyMicros`, skipping entries and keys of another type (P197).
pub fn read_audio_latencies(info: &Dictionary) -> Option<u64> {
    info.get("audioLatencies")?
        .as_array()?
        .iter()
        .filter_map(Value::as_dictionary)
        .filter_map(|entry| non_negative(entry.get("outputLatencyMicros")?))
        .map(|micros| micros.round() as u64)
        .max()
}

/// `arrivalToRenderLatencyMs` of a stream SETUP answer in microseconds, the
/// `streams` entry winning over the top level (P198).
pub fn read_arrival_to_render(answer: &Dictionary) -> Option<u64> {
    const KEY: &str = "arrivalToRenderLatencyMs";
    let stream = answer
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())
        .and_then(Value::as_dictionary);
    let millis = stream
        .and_then(|stream| stream.get(KEY))
        .and_then(non_negative)
        .or_else(|| answer.get(KEY).and_then(non_negative))?;
    Some((millis * 1000.0).round() as u64)
}
