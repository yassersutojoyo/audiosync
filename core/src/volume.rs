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

/// The zone position of a speaker in no zone (S71, S249).
pub const NO_ZONE_POSITION: f64 = 100.0;

/// A local output's renderer gain through its zone: `g(local) × g(zone) ×
/// g(master)` (S71, S249). At zone 100 the factor is exactly 1, so the gain
/// is bit-identical to [`output_gain`].
pub fn zoned_output_gain(local_position: f64, zone_position: f64, master_position: f64) -> f64 {
    gain_for_position(local_position)
        * gain_for_position(zone_position)
        * gain_for_position(master_position)
}

/// The linear level of three slider stages, own × zone × master, each
/// `position / 100` clamped to 0…1 (S249): what an AirPlay speaker's volume
/// spreads over its dB range.
pub fn stage_level(own_position: f64, zone_position: f64, master_position: f64) -> f64 {
    (own_position / MAX_POSITION).clamp(0.0, 1.0)
        * (zone_position / MAX_POSITION).clamp(0.0, 1.0)
        * (master_position / MAX_POSITION).clamp(0.0, 1.0)
}

/// [`stage_level`] backwards (S250): the own position, rounded to a whole
/// step and clamped to 0…100, that plays at `level` (0…1) through the zone
/// and the master. None when zone × master is 0, where every own position
/// sounds the same, or `level` isn't finite.
pub fn own_position_for_level(level: f64, zone_position: f64, master_position: f64) -> Option<f64> {
    let scale = (master_position / MAX_POSITION).clamp(0.0, 1.0)
        * (zone_position / MAX_POSITION).clamp(0.0, 1.0);
    if scale.is_nan() || scale <= 0.0 || !level.is_finite() {
        return None;
    }
    Some(
        (level.clamp(0.0, 1.0) / scale * MAX_POSITION)
            .clamp(MIN_POSITION, MAX_POSITION)
            .round(),
    )
}

/// Migrates a legacy linear percentage (0…150) to a position:
/// `p(pct / 100)`, clamped, so 3.49 % becomes 32.68 and anything from 100 %
/// up becomes 100 (S75).
pub fn migrate_legacy_percent(percent: f64) -> f64 {
    position_for_gain(percent / 100.0)
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
        let frames = buffer.len().checked_div(channels).unwrap_or(0);
        let ramp = self.begin(target, frames);
        if channels == 0 {
            return;
        }
        ramp.apply(&mut buffer[..frames * channels], channels, 0);
        // A trailing partial frame is never handed to a device; keep it silent.
        let whole = frames * channels;
        buffer[whole..].fill(0.0);
    }

    /// Starts a callback of `frames` frames at gain `target`: the ramp that
    /// [`GainStage::apply`] runs over a whole buffer, for a renderer that
    /// applies it piece by piece ([`GainRamp::apply`]) with other stages in
    /// between, such as its fade (S183). Real-time safe (S126).
    pub fn begin(&mut self, target: f32, frames: usize) -> GainRamp {
        let target = sanitize_gain(target);
        if !self.primed {
            self.current = target;
            self.primed = true;
        }
        let start = self.current;
        self.current = target;
        let step = if frames == 0 {
            0.0
        } else {
            (target - start) / frames as f32
        };
        GainRamp {
            start,
            target,
            step,
            frames,
        }
    }
}

/// One callback's gain, from [`GainStage::begin`]: the previous gain ramping
/// linearly to the target across the callback, its last frame at the target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainRamp {
    start: f32,
    target: f32,
    step: f32,
    frames: usize,
}

impl GainRamp {
    /// The gain of the callback's frame `index`.
    pub fn gain_at(&self, index: usize) -> f32 {
        if self.start == self.target || index + 1 >= self.frames {
            self.target
        } else {
            self.start + self.step * (index + 1) as f32
        }
    }

    /// Applies the gains of the callback's frames from `first_frame` on to
    /// the whole frames of `chunk`, interleaved with `channels` channels: a
    /// frame whose gain is 0 becomes exact zeros, and NaN or ±infinity
    /// becomes 0 (S64, S65). Real-time safe (S126).
    pub fn apply(&self, chunk: &mut [f32], channels: usize, first_frame: usize) {
        if channels == 0 {
            return;
        }
        for (index, frame) in chunk.chunks_exact_mut(channels).enumerate() {
            let gain = self.gain_at(first_frame + index);
            for sample in frame {
                *sample = if gain == 0.0 {
                    0.0
                } else {
                    let value = *sample * gain;
                    if value.is_finite() { value } else { 0.0 }
                };
            }
        }
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
