//! Volume curve, host-side volume rules and gain application (spec section 7:
//! S70 to S76; renderer gain and output sanitizing: S64, S65).
//!
//! Positions are the 0…100 slider values the user sees; gains are linear
//! factors. The curve is cubic, so 50 is about −18 dB and 10 about −60 dB.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU32, Ordering};

/// Lowest slider position: silence (S70).
pub const MIN_POSITION: f64 = 0.0;

/// Highest slider position: unity gain (S70).
pub const MAX_POSITION: f64 = 100.0;

/// Position of a local output the user has never set, about −13.3 dB (S71).
pub const DEFAULT_LOCAL_POSITION: f64 = 60.0;

/// Master position until the user sets one (S71).
pub const DEFAULT_MASTER_POSITION: f64 = 100.0;

/// For this long after the user changes an AirPlay speaker's volume, the volume
/// the engine reports is not adopted (S73), in seconds.
pub const USER_CHANGE_HOLD: f64 = 2.0;

/// Clamps a position to 0…100 (S76). NaN and −0 count as 0, so the result is
/// never negative zero.
pub fn clamp_position(position: f64) -> f64 {
    if position.is_nan() || position <= MIN_POSITION {
        MIN_POSITION
    } else {
        position.min(MAX_POSITION)
    }
}

/// A volume input from the host, ready to use: non-finite values are ignored
/// (`None`, keep the previous value), others clamped to 0…100 (S76, S124).
pub fn sanitize_position(position: f64) -> Option<f64> {
    position.is_finite().then(|| clamp_position(position))
}

/// Linear gain of a slider position: `(clamp(p, 0, 100) / 100)³` (S70).
pub fn gain_for_position(position: f64) -> f64 {
    let x = clamp_position(position) / MAX_POSITION;
    x * x * x
}

/// Slider position of a linear gain: `100 × cbrt(clamp(g, 0, 1))` (S70). NaN
/// and −0 count as 0, so the result is never negative zero.
pub fn position_for_gain(gain: f64) -> f64 {
    let gain = if gain.is_nan() || gain <= 0.0 {
        0.0
    } else {
        gain.min(1.0)
    };
    MAX_POSITION * gain.cbrt()
}

/// A linear gain in dB; 0 is −∞.
pub fn gain_to_db(gain: f64) -> f64 {
    20.0 * gain.log10()
}

/// A local output's renderer gain: `g(local) × g(master)` (S71).
pub fn output_gain(local_position: f64, master_position: f64) -> f64 {
    gain_for_position(local_position) * gain_for_position(master_position)
}

/// A position on the AirPlay engine's own linear 0…100 scale: the master
/// (S72) and each speaker's volume (S73) are sent as `round(p)`.
pub fn engine_volume(position: f64) -> u8 {
    // clamp_position keeps the value in 0…100, so the cast is exact.
    clamp_position(position).round() as u8
}

/// Whether to adopt the volume the engine reports for a speaker: always,
/// except within 2 s of the user's own change (S73).
pub fn adopt_reported_volume(now: f64, last_user_change: Option<f64>) -> bool {
    last_user_change.is_none_or(|changed| now - changed >= USER_CHANGE_HOLD)
}

/// Migrates a legacy linear percentage (0…150) to a position:
/// `p(pct / 100)`, clamped, so 3.49 % becomes 32.68 and anything from 100 %
/// up becomes 100 (S75).
pub fn migrate_legacy_percent(percent: f64) -> f64 {
    position_for_gain(percent / 100.0)
}

/// Where a volume value goes: the engine's master or one AirPlay speaker
/// (S72 to S74).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VolumeTarget {
    /// The AirPlay engine's master volume.
    Master,
    /// One AirPlay speaker, by engine output ID.
    Output(String),
}

/// Volume writes waiting for the engine, coalesced per target (S74). `K`
/// names the target (the host uses [`VolumeTarget`]: the master or one
/// AirPlay speaker) and `V` is the value to send.
///
/// A target has at most one value waiting, and it is always the most recent
/// one, so a slider drag costs one request per flush instead of one per
/// movement. Timing stays with the caller: [`put`](Self::put) tells it when
/// a target had nothing waiting, which is the only time it queues a flush
/// for that target. Until the flush runs, newer values simply replace the
/// waiting one. The flush takes the value with [`pop`](Self::pop), which
/// leaves the target with nothing waiting, so the next value queues a new
/// flush. Targets are independent entries: a value for one never replaces
/// another's.
///
/// The entries are a short vector searched linearly, holding only the
/// targets that have a value waiting.
#[derive(Clone, Debug, PartialEq)]
pub struct LatestValues<K, V> {
    slots: Vec<(K, V)>,
}

impl<K, V> Default for LatestValues<K, V> {
    fn default() -> Self {
        LatestValues { slots: Vec::new() }
    }
}

impl<K: PartialEq, V: Copy> LatestValues<K, V> {
    /// A table with nothing waiting.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `value` the one to send for `key`, replacing any value already
    /// waiting for it. Returns true when nothing was waiting: the caller then
    /// queues one flush for `key`. Returns false when a flush is already
    /// queued; that flush sends the newest value it finds, so it picks this
    /// one up.
    pub fn put(&mut self, key: K, value: V) -> bool {
        match self.slots.iter_mut().find(|(held, _)| *held == key) {
            Some(slot) => {
                slot.1 = value;
                false
            }
            None => {
                self.slots.push((key, value));
                true
            }
        }
    }

    /// Takes the value waiting for `key`, as its flush does right before
    /// sending, and leaves nothing waiting for it. `None` when nothing is
    /// waiting.
    pub fn pop(&mut self, key: &K) -> Option<V> {
        let index = self.slots.iter().position(|(held, _)| held == key)?;
        Some(self.slots.remove(index).1)
    }

    /// Whether a value is waiting for `key`.
    pub fn holds(&self, key: &K) -> bool {
        self.slots.iter().any(|(held, _)| held == key)
    }

    /// How many targets have a value waiting.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no target has a value waiting.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// A linear gain shared between the control thread and one renderer thread
/// through a relaxed atomic (S64, S133). Negative and non-finite gains are
/// stored as 0.
#[derive(Debug)]
pub struct SharedGain(AtomicU32);

impl SharedGain {
    /// A shared gain with this initial value.
    pub fn new(gain: f32) -> Self {
        SharedGain(AtomicU32::new(sanitize_gain(gain).to_bits()))
    }

    /// Stores a new gain; the renderer applies it from its next callback.
    pub fn set(&self, gain: f32) {
        self.0
            .store(sanitize_gain(gain).to_bits(), Ordering::Relaxed);
    }

    /// The current gain.
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

fn sanitize_gain(gain: f32) -> f32 {
    if gain.is_finite() && gain > 0.0 {
        gain
    } else {
        0.0
    }
}

/// The renderer's gain stage, owned by its output thread (S64, S65).
///
/// The first callback plays at the target gain, so a gain set before `start`
/// applies from the first callback. A later change ramps linearly across one
/// callback and holds from the next. A frame whose gain is 0 is exact zeros,
/// and every NaN or ±infinity is replaced by 0 before the device sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GainStage {
    current: f32,
    primed: bool,
}

impl GainStage {
    /// A stage that has not run yet.
    pub const fn new() -> Self {
        GainStage {
            current: 0.0,
            primed: false,
        }
    }

    /// The gain the last callback ended on.
    pub fn current(&self) -> f32 {
        self.current
    }

    /// Applies the gain to an interleaved device buffer with `channels`
    /// channels, in place. Real-time safe (S126).
    pub fn apply(&mut self, buffer: &mut [f32], channels: usize, target: f32) {
        let target = sanitize_gain(target);
        if !self.primed {
            self.current = target;
            self.primed = true;
        }
        let start = self.current;
        self.current = target;
        if channels == 0 {
            return;
        }
        let frames = buffer.len() / channels;
        let step = if frames == 0 {
            0.0
        } else {
            (target - start) / frames as f32
        };
        for (index, frame) in buffer.chunks_exact_mut(channels).enumerate() {
            let gain = if start == target || index + 1 == frames {
                target
            } else {
                start + step * (index + 1) as f32
            };
            for sample in frame {
                *sample = if gain == 0.0 {
                    0.0
                } else {
                    let value = *sample * gain;
                    if value.is_finite() { value } else { 0.0 }
                };
            }
        }
        // A trailing partial frame is never handed to a device; keep it silent.
        let whole = frames * channels;
        buffer[whole..].fill(0.0);
    }
}

/// Applies a fixed gain to interleaved stereo frames in place, replacing every
/// NaN or ±infinity with 0; gain 0 gives exact zeros (S64, S65). Stateless, so
/// a change applies from the next call. Real-time safe (S126).
pub fn apply_gain(interleaved: &mut [f32], gain: f32) {
    let gain = sanitize_gain(gain);
    for sample in interleaved {
        *sample = if gain == 0.0 {
            0.0
        } else {
            let value = *sample * gain;
            if value.is_finite() { value } else { 0.0 }
        };
    }
}
