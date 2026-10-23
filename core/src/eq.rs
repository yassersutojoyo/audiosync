//! Ten-band equalizer and preamp (spec section 3: S22 to S34).
//!
//! The equalizer is split into two halves that share nothing but atomics:
//!
//! - [`EqControl`] belongs to the control thread (the host's main thread). It
//!   keeps the settings and the design rate, computes the biquad coefficients
//!   and publishes them through two coefficient sets (S31).
//! - [`EqProcessor`] belongs to the capture thread. It keeps the filter state
//!   and equalizes interleaved stereo float32 blocks in place (S26).
//!
//! A settings change takes effect at a block boundary, for all ten bands, the
//! preamp and the enabled flag together. The control thread fills the idle set
//! and flips the active index; at the start of a block the capture thread
//! copies the active set, publishing which set it is copying so the control
//! side never rewrites a set in use. The capture thread never waits: if a flip
//! races its copy, it keeps the previous coefficients for one more block (S31,
//! S130). Filter state lives only in the processor and is kept across
//! coefficient changes.

#![forbid(unsafe_code)]

use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[cfg(not(target_has_atomic = "64"))]
compile_error!(
    "audiosync-core shares values with audio threads through lock-free 64-bit atomics (S128)"
);

/// Number of bands (S22).
pub const BAND_COUNT: usize = 10;

/// Band centers in Hz, band 0 to band 9, in ascending order (S22).
pub const BAND_CENTERS_HZ: [f64; BAND_COUNT] = [
    32.0, 64.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16_000.0,
];

/// Quality factor of every band, about one octave (S25).
pub const BAND_Q: f64 = 1.41;

/// A band's design frequency never exceeds this fraction of the sample rate (S25).
pub const MAX_CENTER_RATIO: f64 = 0.45;

/// Design rate until the host sets the capture rate (S30).
pub const DEFAULT_SAMPLE_RATE: f64 = 48_000.0;

/// Lowest band gain or preamp the host's controls offer, in dB (S23).
pub const MIN_GAIN_DB: f64 = -12.0;

/// Highest band gain or preamp the host's controls offer, in dB (S23).
pub const MAX_GAIN_DB: f64 = 12.0;

/// Step of the host's gain and preamp controls, in dB (S23).
pub const GAIN_STEP_DB: f64 = 0.5;

/// Equalizer settings as the host persists them (S24).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqSettings {
    /// Off by default; while off, audio passes untouched (S27).
    pub enabled: bool,
    /// Gain applied before the bands, in dB (S26).
    pub preamp_db: f64,
    /// Gain of each band in dB, band 0 (32 Hz) to band 9 (16 kHz).
    pub gains_db: [f64; BAND_COUNT],
}

impl Default for EqSettings {
    fn default() -> Self {
        EqSettings {
            enabled: false,
            preamp_db: 0.0,
            gains_db: [0.0; BAND_COUNT],
        }
    }
}

impl EqSettings {
    /// Sets the band gains of `preset`; the enabled flag and the preamp stay as they are (S33).
    pub fn apply_preset(&mut self, preset: &Preset) {
        self.gains_db = preset.gains_db;
    }

    /// The preset whose ten gains all equal these settings' gains exactly, if any (S33).
    pub fn matching_preset(&self) -> Option<&'static Preset> {
        PRESETS
            .iter()
            .find(|preset| preset.gains_db == self.gains_db)
    }

    /// Reset: flat gains and a 0 dB preamp, so the picker reads Flat; the
    /// enabled flag stays (S33).
    pub fn reset(&mut self) {
        self.gains_db = PRESETS[0].gains_db;
        self.preamp_db = 0.0;
    }

    /// These settings with the preamp and every gain quantized for the host's controls (S23).
    pub fn quantized(&self) -> EqSettings {
        EqSettings {
            enabled: self.enabled,
            preamp_db: quantize_gain_db(self.preamp_db),
            gains_db: self.gains_db.map(quantize_gain_db),
        }
    }
}

/// Clamps a gain to −12…+12 dB and rounds it to the nearest 0.5 dB step, as the
/// host's controls do (S23). NaN becomes 0 dB. The DSP itself applies any finite
/// value it is given.
pub fn quantize_gain_db(db: f64) -> f64 {
    if db.is_nan() {
        return 0.0;
    }
    let steps = (db.clamp(MIN_GAIN_DB, MAX_GAIN_DB) / GAIN_STEP_DB).round();
    // Adding 0.0 turns a negative zero into a positive one.
    steps * GAIN_STEP_DB + 0.0
}

/// A named set of band gains (S33).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    /// Stable identifier, as the host stores it.
    pub id: &'static str,
    /// Band gains in dB, 32 Hz to 16 kHz.
    pub gains_db: [f64; BAND_COUNT],
}

/// Number of presets (S33).
pub const PRESET_COUNT: usize = 12;

/// The presets, in menu order (S33).
pub const PRESETS: [Preset; PRESET_COUNT] = [
    Preset {
        id: "flat",
        gains_db: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        id: "bassBoost",
        gains_db: [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        id: "bassCut",
        gains_db: [-6.0, -5.0, -4.0, -2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        id: "trebleBoost",
        gains_db: [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0],
    },
    Preset {
        id: "vocal",
        gains_db: [-2.0, -2.0, -1.0, 1.0, 3.0, 4.0, 3.0, 1.0, 0.0, -1.0],
    },
    Preset {
        id: "rock",
        gains_db: [4.0, 3.0, 2.0, 0.0, -1.0, -1.0, 1.0, 3.0, 4.0, 4.0],
    },
    Preset {
        id: "pop",
        gains_db: [-1.0, 0.0, 2.0, 3.0, 4.0, 3.0, 1.0, 0.0, -1.0, -1.0],
    },
    Preset {
        id: "jazz",
        gains_db: [3.0, 2.0, 1.0, 2.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0],
    },
    Preset {
        id: "classical",
        gains_db: [4.0, 3.0, 2.0, 1.0, -1.0, -1.0, 0.0, 2.0, 3.0, 4.0],
    },
    Preset {
        id: "electronic",
        gains_db: [5.0, 4.0, 1.0, 0.0, -2.0, 1.0, 0.0, 1.0, 4.0, 5.0],
    },
    Preset {
        id: "loudness",
        gains_db: [6.0, 4.0, 0.0, 0.0, -2.0, 0.0, -1.0, -2.0, 4.0, 2.0],
    },
    Preset {
        id: "smallSpeakers",
        gains_db: [-4.0, -2.0, 2.0, 3.0, 2.0, 1.0, 1.0, 2.0, 2.0, 1.0],
    },
];

/// The preset with this identifier.
pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// Most curves the user can save (S270).
pub const MAX_USER_CURVES: usize = 50;

/// Longest name of a saved curve, in Unicode scalars (S270).
pub const MAX_CURVE_NAME_CHARS: usize = 40;

/// A curve the user saved under a name: ten band gains, no preamp (S270).
#[derive(Clone, Debug, PartialEq)]
pub struct UserCurve {
    pub name: String,
    /// Band gains in dB, 32 Hz to 16 kHz, on the controls' steps.
    pub gains_db: [f64; BAND_COUNT],
}

/// The curve a set of gains matches: a preset by its index in [`PRESETS`],
/// or a saved curve by its index in the user's list (S33, S270).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurveMatch {
    Preset(usize),
    User(usize),
}

/// The first curve whose ten gains all equal `gains_db` exactly: the presets
/// in menu order, then the saved curves in their order. The preamp never
/// counts (S33, S270).
pub fn matching_curve(gains_db: &[f64; BAND_COUNT], user: &[UserCurve]) -> Option<CurveMatch> {
    if let Some(index) = PRESETS.iter().position(|p| p.gains_db == *gains_db) {
        return Some(CurveMatch::Preset(index));
    }
    user.iter()
        .position(|curve| curve.gains_db == *gains_db)
        .map(CurveMatch::User)
}

/// A typed name as a curve keeps it: trimmed and cut to
/// [`MAX_CURVE_NAME_CHARS`]; `None` when nothing is left (S270).
pub fn curve_name(typed: &str) -> Option<String> {
    let name: String = typed.trim().chars().take(MAX_CURVE_NAME_CHARS).collect();
    let name = name.trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Saves `gains_db` under `typed`. A saved curve of the same name, ignoring
/// case, takes the new gains in its place; otherwise the curve goes last.
/// Returns its index, or `None` for an empty name or a full list (S270).
pub fn save_curve(
    curves: &mut Vec<UserCurve>,
    typed: &str,
    gains_db: &[f64; BAND_COUNT],
) -> Option<usize> {
    let name = curve_name(typed)?;
    let gains_db = gains_db.map(quantize_gain_db);
    if let Some(index) = curves.iter().position(|c| same_name(&c.name, &name)) {
        curves[index] = UserCurve { name, gains_db };
        return Some(index);
    }
    if curves.len() >= MAX_USER_CURVES {
        return None;
    }
    curves.push(UserCurve { name, gains_db });
    Some(curves.len() - 1)
}

/// Renames the saved curve at `index`; false for a bad index, an empty name
/// or a name another saved curve has, ignoring case (S270).
pub fn rename_curve(curves: &mut [UserCurve], index: usize, typed: &str) -> bool {
    let Some(name) = curve_name(typed) else {
        return false;
    };
    let taken = curves
        .iter()
        .enumerate()
        .any(|(i, c)| i != index && same_name(&c.name, &name));
    match curves.get_mut(index) {
        Some(curve) if !taken => {
            curve.name = name;
            true
        }
        _ => false,
    }
}

/// Deletes the saved curve at `index`; false for a bad index (S270).
pub fn delete_curve(curves: &mut Vec<UserCurve>, index: usize) -> bool {
    if index >= curves.len() {
        return false;
    }
    curves.remove(index);
    true
}

/// Saved curves as read back from storage: names as [`curve_name`] keeps
/// them, gains on the controls' steps (a band that is not a number is 0 dB),
/// unnamed curves and later duplicates of a name left out, at most
/// [`MAX_USER_CURVES`] (S270).
pub fn sanitized_curves(saved: impl IntoIterator<Item = UserCurve>) -> Vec<UserCurve> {
    let mut curves: Vec<UserCurve> = Vec::new();
    for curve in saved {
        if curves.len() >= MAX_USER_CURVES {
            break;
        }
        let Some(name) = curve_name(&curve.name) else {
            continue;
        };
        if curves.iter().any(|c| same_name(&c.name, &name)) {
            continue;
        }
        curves.push(UserCurve {
            name,
            gains_db: curve.gains_db.map(quantize_gain_db),
        });
    }
    curves
}

/// Normalized biquad coefficients (a0 = 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad {
    /// Feed-forward coefficient of `x[n]`.
    pub b0: f64,
    /// Feed-forward coefficient of `x[n−1]`.
    pub b1: f64,
    /// Feed-forward coefficient of `x[n−2]`.
    pub b2: f64,
    /// Feedback coefficient of `y[n−1]`.
    pub a1: f64,
    /// Feedback coefficient of `y[n−2]`.
    pub a2: f64,
}

impl Biquad {
    /// RBJ-cookbook peaking filter with Q = 1.41 at `center_hz`, design frequency
    /// limited to 0.45 × `sample_rate` (S25).
    pub fn peaking(center_hz: f64, gain_db: f64, sample_rate: f64) -> Biquad {
        let f = center_hz.min(MAX_CENTER_RATIO * sample_rate);
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f / sample_rate;
        let alpha = w0.sin() / (2.0 * BAND_Q);
        let a0 = 1.0 + alpha / a;
        Biquad {
            b0: (1.0 + alpha * a) / a0,
            b1: -2.0 * w0.cos() / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: -2.0 * w0.cos() / a0,
            a2: (1.0 - alpha / a) / a0,
        }
    }
}

/// The ten band filters for these gains at this rate (S25).
pub fn design_bands(gains_db: &[f64; BAND_COUNT], sample_rate: f64) -> [Biquad; BAND_COUNT] {
    std::array::from_fn(|band| Biquad::peaking(BAND_CENTERS_HZ[band], gains_db[band], sample_rate))
}

/// Everything the capture thread needs for one block, copied as a whole.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Snapshot {
    enabled: bool,
    /// Linear preamp factor, 10^(preamp/20).
    preamp: f64,
    bands: [Biquad; BAND_COUNT],
    /// Changes with every rate change; a processor that adopts a set with a new
    /// epoch zeroes its filter state first (S30).
    epoch: u64,
}

impl Snapshot {
    fn design(settings: &EqSettings, sample_rate: f64, epoch: u64) -> Snapshot {
        Snapshot {
            enabled: settings.enabled,
            preamp: 10f64.powf(settings.preamp_db / 20.0),
            bands: design_bands(&settings.gains_db, sample_rate),
            epoch,
        }
    }
}

/// One published coefficient set. Every value is an atomic, so a racing copy is
/// never undefined behavior; the active/reading protocol makes it never torn.
struct SharedSet {
    enabled: AtomicBool,
    preamp: AtomicU64,
    bands: [[AtomicU64; 5]; BAND_COUNT],
    epoch: AtomicU64,
}

impl SharedSet {
    fn new(snapshot: &Snapshot) -> SharedSet {
        let set = SharedSet {
            enabled: AtomicBool::new(false),
            preamp: AtomicU64::new(0),
            bands: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            epoch: AtomicU64::new(0),
        };
        set.store(snapshot);
        set
    }

    /// Relaxed stores: ordering comes from the SeqCst `active` flip that follows.
    fn store(&self, snapshot: &Snapshot) {
        self.enabled.store(snapshot.enabled, Ordering::Relaxed);
        self.preamp
            .store(snapshot.preamp.to_bits(), Ordering::Relaxed);
        for (slots, band) in self.bands.iter().zip(snapshot.bands.iter()) {
            let values = [band.b0, band.b1, band.b2, band.a1, band.a2];
            for (slot, value) in slots.iter().zip(values) {
                slot.store(value.to_bits(), Ordering::Relaxed);
            }
        }
        self.epoch.store(snapshot.epoch, Ordering::Relaxed);
    }

    /// Relaxed loads: ordering comes from the SeqCst `active`/`reading` protocol.
    fn load(&self) -> Snapshot {
        let value = |slot: &AtomicU64| f64::from_bits(slot.load(Ordering::Relaxed));
        Snapshot {
            enabled: self.enabled.load(Ordering::Relaxed),
            preamp: value(&self.preamp),
            bands: std::array::from_fn(|band| {
                let slots = &self.bands[band];
                Biquad {
                    b0: value(&slots[0]),
                    b1: value(&slots[1]),
                    b2: value(&slots[2]),
                    a1: value(&slots[3]),
                    a2: value(&slots[4]),
                }
            }),
            epoch: self.epoch.load(Ordering::Relaxed),
        }
    }
}

/// `reading` value while the capture thread copies no set.
const NOT_READING: usize = 2;

/// State shared by the two halves: atomics only (S128).
struct Shared {
    sets: [SharedSet; 2],
    /// Index of the set holding the current settings (SeqCst, S133).
    active: AtomicUsize,
    /// Index of the set the capture thread is copying, or `NOT_READING` (SeqCst, S133).
    reading: AtomicUsize,
    /// Bumped after every flip, so the capture thread copies only after a change.
    version: AtomicU64,
    /// Channels reset after a non-finite cascade output, for diagnostics (S131).
    nan_resets: AtomicU64,
}

/// Creates an equalizer: the control half for the main thread and the processing
/// half for the capture thread. It starts disabled and flat, designed for
/// 48 000 Hz (S24, S30).
pub fn equalizer() -> (EqControl, EqProcessor) {
    let settings = EqSettings::default();
    let snapshot = Snapshot::design(&settings, DEFAULT_SAMPLE_RATE, 0);
    let shared = Arc::new(Shared {
        sets: [SharedSet::new(&snapshot), SharedSet::new(&snapshot)],
        active: AtomicUsize::new(0),
        reading: AtomicUsize::new(NOT_READING),
        version: AtomicU64::new(0),
        nan_resets: AtomicU64::new(0),
    });
    let control = EqControl {
        shared: Arc::clone(&shared),
        settings,
        sample_rate: DEFAULT_SAMPLE_RATE,
        epoch: 0,
    };
    let processor = EqProcessor {
        shared,
        current: snapshot,
        state: [[BandState::ZERO; BAND_COUNT]; 2],
        was_enabled: false,
        seen_version: 0,
        #[cfg(test)]
        biquad_steps: 0,
    };
    (control, processor)
}

/// The control half: settings, design rate and coefficient publishing (S31).
///
/// Every method that changes something takes `&mut self`, so exactly one thread
/// at a time publishes; the audio thread is never blocked by it.
pub struct EqControl {
    shared: Arc<Shared>,
    settings: EqSettings,
    sample_rate: f64,
    epoch: u64,
}

impl EqControl {
    /// The settings in effect (after replacing non-finite values with 0 dB).
    pub fn settings(&self) -> EqSettings {
        self.settings
    }

    /// The design rate in Hz (S30).
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// The band filters currently published for the capture thread.
    pub fn bands(&self) -> [Biquad; BAND_COUNT] {
        design_bands(&self.settings.gains_db, self.sample_rate)
    }

    /// Publishes new settings. They take effect at the next block boundary,
    /// all ten bands, the preamp and the enabled flag together; filter state is
    /// kept (S31). Any finite gain is applied as given (S23); a non-finite gain
    /// or preamp counts as 0 dB.
    pub fn set_settings(&mut self, settings: EqSettings) {
        let settings = EqSettings {
            enabled: settings.enabled,
            preamp_db: finite_or_zero(settings.preamp_db),
            gains_db: settings.gains_db.map(finite_or_zero),
        };
        if settings == self.settings {
            return;
        }
        self.settings = settings;
        self.publish();
    }

    /// Sets the design rate, called only while capture is stopped (S30, S114):
    /// the coefficients are recomputed and the processor zeroes all filter state
    /// before its next block. The same rate again is a no-op. Returns false (and
    /// changes nothing) for a rate that is not finite and positive.
    pub fn set_sample_rate(&mut self, sample_rate: f64) -> bool {
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return false;
        }
        if sample_rate == self.sample_rate {
            return true;
        }
        self.sample_rate = sample_rate;
        self.epoch += 1;
        self.publish();
        true
    }

    /// How many times a channel's cascade produced NaN or ±infinity and was
    /// reset (S29). Counted by the capture thread, read from any thread (S131).
    pub fn nan_resets(&self) -> u64 {
        self.shared.nan_resets.load(Ordering::Relaxed)
    }

    fn publish(&mut self) {
        let snapshot = Snapshot::design(&self.settings, self.sample_rate, self.epoch);
        self.publish_snapshot(&snapshot);
    }

    fn publish_snapshot(&mut self, snapshot: &Snapshot) {
        let shared = &*self.shared;
        let idle = 1 - shared.active.load(Ordering::SeqCst);
        // Never rewrite a set the capture thread is copying; it leaves within a
        // few dozen loads (S31: the control thread may yield briefly).
        while shared.reading.load(Ordering::SeqCst) == idle {
            std::thread::yield_now();
        }
        shared.sets[idle].store(snapshot);
        shared.active.store(idle, Ordering::SeqCst);
        shared.version.fetch_add(1, Ordering::SeqCst);
    }
}

fn finite_or_zero(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

/// Filter state smaller than this is cleared at the end of every block. It is
/// 600 dB below full scale, so clearing it can't change what any output plays
/// audibly, and it is far above the subnormal range (below about 2.2e-308).
const STATE_FLOOR: f64 = 1e-30;

/// Transposed direct form II state of one band on one channel.
#[derive(Clone, Copy, Debug, PartialEq)]
struct BandState {
    z1: f64,
    z2: f64,
}

impl BandState {
    const ZERO: BandState = BandState { z1: 0.0, z2: 0.0 };

    /// Clears state below [`STATE_FLOOR`]. In silence a filter's state only
    /// decays, never to 0 by itself: it sinks into subnormal numbers, which
    /// many processors compute far more slowly, and a boosted bass band can
    /// stay there in a rounding limit cycle, costing CPU for nothing.
    fn clear_tiny(&mut self) {
        if self.z1.abs() < STATE_FLOOR {
            self.z1 = 0.0;
        }
        if self.z2.abs() < STATE_FLOOR {
            self.z2 = 0.0;
        }
    }
}

/// The processing half, owned by the capture thread (S34).
pub struct EqProcessor {
    shared: Arc<Shared>,
    current: Snapshot,
    /// Per channel (L, R) and per band (S26).
    state: [[BandState; BAND_COUNT]; 2],
    was_enabled: bool,
    seen_version: u64,
    /// Biquad steps taken, to check the O(20·N) bound (S127).
    #[cfg(test)]
    biquad_steps: u64,
}

impl EqProcessor {
    /// Equalizes one block of interleaved stereo float32 frames in place (S26).
    ///
    /// The block holds `interleaved.len() / 2` frames; an odd trailing sample is
    /// left untouched. Disabled, the block is left untouched (S27). Real-time
    /// safe: no allocation, lock, wait or system call (S126); 20 biquad steps
    /// per frame (S127). State too small to hear is cleared after each block,
    /// so silence never leaves the filters working on subnormal numbers.
    pub fn process(&mut self, interleaved: &mut [f32]) {
        self.pick_up();
        if !self.current.enabled {
            self.was_enabled = false;
            return;
        }
        if !self.was_enabled {
            // S28: the first enabled block after a disabled stretch starts
            // from zeroed filters; their memory is from audio that is long gone.
            self.zero_state();
            self.was_enabled = true;
        }
        let samples = interleaved.len() / 2 * 2;
        let block = &mut interleaved[..samples];
        let preamp = self.current.preamp;
        let bands = &self.current.bands;
        let mut resets = 0u64;
        for (channel, saved) in self.state.iter_mut().enumerate() {
            let mut state = *saved;
            for frame in block.chunks_exact_mut(2) {
                let mut y = f64::from(frame[channel]) * preamp;
                for (band, z) in bands.iter().zip(state.iter_mut()) {
                    let x = y;
                    y = band.b0 * x + z.z1;
                    z.z1 = band.b1 * x - band.a1 * y + z.z2;
                    z.z2 = band.b2 * x - band.a2 * y;
                }
                frame[channel] = if y.is_finite() {
                    y.clamp(-1.0, 1.0) as f32
                } else {
                    // NaN or ±infinity: this channel starts over from silence (S29).
                    state = [BandState::ZERO; BAND_COUNT];
                    resets += 1;
                    0.0
                };
            }
            for band in &mut state {
                band.clear_tiny();
            }
            *saved = state;
        }
        #[cfg(test)]
        {
            self.biquad_steps += (samples * BAND_COUNT) as u64;
        }
        if resets != 0 {
            self.shared.nan_resets.fetch_add(resets, Ordering::Relaxed);
        }
    }

    /// Equalizes `block` once, in place, then hands the same samples to the
    /// ring buffer and to the AirPlay leg, so every output hears identical
    /// audio (S7, S34). Real-time safe when both consumers are.
    pub fn equalize_and_deliver(
        &mut self,
        block: &mut [f32],
        to_ring: impl FnOnce(&[f32]),
        to_airplay: impl FnOnce(&[f32]),
    ) {
        self.process(block);
        to_ring(block);
        to_airplay(block);
    }

    /// Copies the active coefficient set if a new one was published (S31).
    fn pick_up(&mut self) {
        let shared = &*self.shared;
        let version = shared.version.load(Ordering::SeqCst);
        if version == self.seen_version {
            return;
        }
        let active = shared.active.load(Ordering::SeqCst);
        shared.reading.store(active, Ordering::SeqCst);
        // If the control side flipped between the load and the store above, the
        // set may be under rewrite: skip the copy and try again next block.
        if shared.active.load(Ordering::SeqCst) == active {
            let next = shared.sets[active].load();
            if next.epoch != self.current.epoch {
                // A new design rate: stale state would click (S30).
                self.state = [[BandState::ZERO; BAND_COUNT]; 2];
            }
            self.current = next;
            self.seen_version = version;
        }
        shared.reading.store(NOT_READING, Ordering::SeqCst);
    }

    fn zero_state(&mut self) {
        self.state = [[BandState::ZERO; BAND_COUNT]; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, freq: f64, rate: f64, amplitude: f64) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let x = (amplitude * (2.0 * PI * freq * i as f64 / rate).sin()) as f32;
                [x, x]
            })
            .collect()
    }

    fn snapshot_of(k: u64) -> Snapshot {
        let v = k as f64;
        Snapshot {
            enabled: k % 2 == 0,
            preamp: v,
            bands: [Biquad {
                b0: v,
                b1: v,
                b2: v,
                a1: v,
                a2: v,
            }; BAND_COUNT],
            epoch: 0,
        }
    }

    // covers: S31, S130, S132, S133
    #[test]
    fn a_published_set_is_adopted_whole_while_the_control_thread_keeps_flipping() {
        let (mut control, mut processor) = equalizer();
        let stop = Arc::new(AtomicBool::new(false));
        let publisher = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut k = 1u64;
                while !stop.load(Ordering::Relaxed) {
                    control.publish_snapshot(&snapshot_of(k));
                    k += 1;
                }
                k
            })
        };
        let mut adopted = 0u64;
        let mut last = 0.0;
        let started = std::time::Instant::now();
        let mut rounds = 0u64;
        // At least 200 000 blocks, and until a few sets were picked up (bounded,
        // for a loaded machine that starts the publisher late).
        while rounds < 200_000 || (adopted < 3 && started.elapsed().as_secs() < 20) {
            rounds += 1;
            processor.pick_up();
            if processor.seen_version == 0 {
                continue;
            }
            let s = processor.current;
            let k = s.preamp;
            // Every value of the copy comes from the same publication.
            assert!(
                s.bands
                    .iter()
                    .all(|b| [b.b0, b.b1, b.b2, b.a1, b.a2].iter().all(|&v| v == k))
            );
            assert_eq!(s.enabled, (k as u64) % 2 == 0);
            assert!(k >= last, "never goes back to an older set");
            if k != last {
                adopted += 1;
                last = k;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let published = publisher.join().unwrap();
        assert!(published > 1);
        assert!(adopted > 1, "picked up {adopted} of {published} sets");
    }

    // covers: S31, S130
    #[test]
    fn the_control_side_never_rewrites_the_set_being_copied() {
        let (mut control, mut processor) = equalizer();
        let shared = Arc::clone(&processor.shared);
        let idle = 1 - shared.active.load(Ordering::SeqCst);
        // The capture thread is still copying the idle set (it loaded `active`
        // just before the previous flip).
        shared.reading.store(idle, Ordering::SeqCst);
        let publisher = std::thread::spawn(move || {
            control.set_settings(EqSettings {
                enabled: true,
                preamp_db: -6.0,
                ..EqSettings::default()
            });
            control
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            shared.version.load(Ordering::SeqCst),
            0,
            "waits for the copy"
        );
        assert!(
            !shared.sets[idle].load().enabled,
            "the set in use was not touched"
        );
        shared.reading.store(NOT_READING, Ordering::SeqCst);
        let control = publisher.join().unwrap();
        assert_eq!(shared.version.load(Ordering::SeqCst), 1);
        assert_eq!(shared.active.load(Ordering::SeqCst), idle);
        assert!(control.settings().enabled);
        // The next block adopts the whole set at once.
        processor.pick_up();
        assert!(processor.current.enabled);
        assert!((processor.current.preamp - 10f64.powf(-6.0 / 20.0)).abs() < 1e-15);
    }

    // covers: S127
    #[test]
    fn work_per_block_is_twenty_biquad_steps_per_frame() {
        let (mut control, mut processor) = equalizer();
        control.set_settings(EqSettings {
            enabled: true,
            preamp_db: 0.0,
            gains_db: PRESETS[10].gains_db,
        });
        for frames in [1usize, 64, 512, 4096] {
            let mut block = sine(frames, 1000.0, 48_000.0, 0.25);
            block[0] = f32::NAN;
            let before = processor.biquad_steps;
            processor.process(&mut block);
            assert_eq!(processor.biquad_steps - before, 20 * frames as u64);
        }
        control.set_settings(EqSettings::default());
        let before = processor.biquad_steps;
        processor.process(&mut sine(512, 1000.0, 48_000.0, 0.25));
        assert_eq!(
            processor.biquad_steps, before,
            "disabled costs no biquad step"
        );
    }

    // covers: S26, S127
    #[test]
    fn silence_after_boosted_bass_settles_to_an_all_zero_state() {
        let (mut control, mut processor) = equalizer();
        let bass_boost = PRESETS[1];
        assert_eq!(bass_boost.id, "bassBoost");
        control.set_settings(EqSettings {
            enabled: true,
            preamp_db: 0.0,
            gains_db: bass_boost.gains_db,
        });
        processor.process(&mut sine(48_000, 40.0, 48_000.0, 0.5));
        let ringing = |processor: &EqProcessor| {
            processor
                .state
                .iter()
                .flatten()
                .any(|band| *band != BandState::ZERO)
        };
        assert!(
            ringing(&processor),
            "the boosted bands ring after the music"
        );

        // A paused app: twenty seconds of digital silence in 512-frame blocks.
        let mut block = vec![0.0f32; 2 * 512];
        for _ in 0..20 * 48_000 / 512 {
            block.fill(0.0);
            processor.process(&mut block);
            for band in processor.state.iter().flatten() {
                assert!(
                    !band.z1.is_subnormal() && !band.z2.is_subnormal(),
                    "no block ends on subnormal state: {band:?}"
                );
            }
        }
        assert!(!ringing(&processor), "{:?}", processor.state);
        block.fill(0.0);
        processor.process(&mut block);
        assert!(block.iter().all(|&s| s == 0.0), "silence stays exactly 0");
    }
}
