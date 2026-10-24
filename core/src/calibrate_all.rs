//! Calibrate All Speakers (S235 to S241): the reference and then every
//! speaker that is on, one at a time, each measured twice against the
//! reference with the microphone as S232 to S234 measure one output, and the
//! results sheet the user applies, undoes or measures a row of again.
//!
//! The host plays and records; [`Run`] says what to play next and how long
//! to record, and turns the recordings into a [`Sheet`]. Nothing here
//! changes a delay: [`Sheet::apply`] only lists the changes, which the host
//! writes. For the control thread: recordings are correlated through an FFT.

use crate::calibrate::{self, GUARD_MS, MAX_RECORDING_FRAMES, Recording, Refusal, Refused};
use crate::policy::delay::{
    self, DEFAULT_AIRPLAY_DELAY_MS, MAX_DELAY_MS, MAX_HOLD_BACK_MS, MAX_OFFSET_MS, MIN_OFFSET_MS,
};
use crate::policy::zones::{Zone, Zones};
use audiosync_cast::audio::Codec;

/// Recorded past the end of the sweep's expected arrival.
pub const TAIL_SECONDS: f64 = 1.0;
/// Above this uncertainty a row is uncertain and its delay stays.
pub const MAX_UNCERTAINTY_MS: f64 = 10.0;
/// Above this spread the AirPlay speakers differ by more than one shared
/// delay can make up for.
pub const MAX_AIRPLAY_SPREAD_MS: f64 = 10.0;

/// Added to a Cast speaker's measured offset for what the measurement
/// leaves out, as for a local output (S232).
pub const CAST_LATENCY_ALLOWANCE_MS: f64 = 500.0;
/// How long a WAV stream already sent to a Cast speaker with no stored
/// offset is taken to play on (S241): within the 2 to 5 s that users
/// report for live WAV (CAST_SPEC C77a).
pub const CAST_UNKNOWN_WAV_MS: f64 = 3000.0;
/// The same for FLAC (S241). Derived, not measured: reports put the start
/// later as the byte rate falls, and FLAC has about half of WAV's. G7
/// replaces it with a measured figure.
pub const CAST_UNKNOWN_FLAC_MS: f64 = 6000.0;

/// How long after audio enters a Cast speaker's feed it is heard, in ms
/// (S241): its stored offset plus the feed's hold-back plus
/// [`CAST_LATENCY_ALLOWANCE_MS`]; with no offset stored (`None`, 0 or
/// less, or not a number) the hold-back plus the guess for `format`, the
/// session's codec, the longer guess while none is reported. It is the
/// speaker's expected arrival (S235) and how long the music sent to it
/// plays out; it never delays another output (C77).
pub fn cast_wait_ms(offset_ms: Option<f64>, hold_back_ms: f64, format: Option<Codec>) -> f64 {
    let hold_back_ms = if hold_back_ms.is_finite() {
        hold_back_ms.clamp(0.0, MAX_HOLD_BACK_MS)
    } else {
        0.0
    };
    match offset_ms.filter(|ms| *ms > 0.0) {
        Some(offset_ms) => offset_ms.min(MAX_DELAY_MS) + hold_back_ms + CAST_LATENCY_ALLOWANCE_MS,
        None => {
            let guess_ms = match format {
                Some(Codec::Wav) => CAST_UNKNOWN_WAV_MS,
                Some(Codec::Flac) | None => CAST_UNKNOWN_FLAC_MS,
            };
            hold_back_ms + guess_ms
        }
    }
}

/// How a speaker's delay is kept, which decides how a measured difference
/// moves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A local output's offset (S175): heard later, it gets a lower offset.
    Local,
    /// A Cast speaker's measured offset: heard later, the others wait longer
    /// for it. The sheet keeps it to 0 to 20 000 ms, the largest hold-back
    /// (S176), although the Cast sender stores up to 120 000 ms.
    Cast,
    /// The one AirPlay delay as shown, 0 to 20 000 ms (S178), shared by
    /// every AirPlay speaker: heard later, the others wait longer.
    AirPlay,
}

/// A speaker that is on when the run starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Speaker {
    pub id: String,
    pub class: Class,
    /// Its stored delay as [`Class`] describes, `None` when it has none.
    /// Every AirPlay speaker carries the shared AirPlay delay.
    pub offset_ms: Option<f64>,
    /// When its sweep is expected in the recording, counted from the moment
    /// the sweep enters its path before its hold-back (S232): its hold-back
    /// plus its latency as the host estimates it, the reference's included.
    pub latency_ms: f64,
}

/// Why a run can't start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// The rate is outside what the test signal is made at (S233).
    UnsupportedRate,
    /// The reference isn't one of the speakers, or isn't a local output.
    NoReference,
    /// Two speakers share an ID.
    DuplicateId,
    /// Only the reference is on.
    NothingToMeasure,
    /// A speaker's expected arrival is too late for its recording to fit
    /// the estimate's 2^21 frames at this rate (S234): cut short, its sweep
    /// would read as too quiet.
    TooLong,
}

/// Why a run or a sheet ended before it could be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Cancelled,
    SpeakersChanged,
    SpeakerTurnedOff,
    SourceChanged,
}

/// Which recording a step asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    Reference,
    /// A speaker's first or second sweep, 1 or 2.
    Sweep(usize),
}

/// What the host plays and records next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The row, in the order the speakers were given.
    pub row: usize,
    pub take: Take,
    /// How many frames to record, from the moment the sweep enters the
    /// speaker's path.
    pub window_frames: usize,
    /// A Cast speaker without a stored offset (none or at most 0 ms, S241):
    /// its first window is the longest the core estimates, so the host shows
    /// that it is searching.
    pub searching: bool,
}

/// A speaker's two sweeps combined.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// The new delay, whole ms, clamped to its class's range.
    pub new_ms: f64,
    /// Half the two sweeps' difference plus [`GUARD_MS`].
    pub uncertainty_ms: f64,
    /// How much later than the reference the speaker was heard, the mean of
    /// both sweeps.
    pub difference_ms: f64,
    /// The new delay hit an end of its range.
    pub clamped: bool,
}

/// What the sheet says about one speaker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RowState {
    Reference,
    /// The run ended before this speaker was measured.
    NotMeasured,
    Certain(Measurement),
    /// Over [`MAX_UNCERTAINTY_MS`]: shown, never applied.
    Uncertain(Measurement),
    Refused(Refused),
}

impl RowState {
    pub fn is_certain(&self) -> bool {
        matches!(self, RowState::Certain(_))
    }
}

/// One speaker's line on the sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub class: Class,
    /// The stored delay before the run, as given.
    pub old_ms: Option<f64>,
    pub state: RowState,
}

/// What the AirPlay speakers' shared delay comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedState {
    /// No AirPlay row is certain: the delay stays.
    NoneCertain,
    Certain,
    /// The certain rows differ by more than [`MAX_AIRPLAY_SPREAD_MS`]: the
    /// host says so, since only one AirPlay delay can be set. The median
    /// still applies.
    Spread,
}

/// The shared AirPlay delay from the median of the certain AirPlay rows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AirPlayShared {
    pub old_ms: Option<f64>,
    /// The median, whole ms; `None` when no AirPlay row is certain.
    pub new_ms: Option<f64>,
    /// The largest minus the smallest difference among the certain rows.
    pub spread_ms: f64,
    pub certain_rows: usize,
    pub state: SharedState,
    /// The range cut the new delay: the AirPlay speakers stay off by the
    /// rest.
    pub clamped: bool,
}

/// What a change writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// One local output's offset or one Cast speaker's offset, by ID.
    Speaker(String),
    /// The one AirPlay delay.
    AirPlay,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub target: Target,
    pub old_ms: Option<f64>,
    pub new_ms: f64,
}

/// The values one Apply replaced, restored in one step.
#[derive(Debug, Clone, PartialEq)]
pub struct Undo {
    previous: Vec<(Target, Option<f64>)>,
}

impl Undo {
    /// Every target with the value it had before Apply; `None` means it had
    /// no stored value.
    pub fn undo(self) -> Vec<(Target, Option<f64>)> {
        self.previous
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub changes: Vec<Change>,
    pub undo: Undo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyRefusal {
    Ended(EndReason),
    NothingCertain,
    /// The sheet was applied once: its rows measured the offsets it replaced.
    AlreadyApplied,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Next {
    Reference,
    First(usize),
    Second {
        at: usize,
        first_ms: f64,
        first_arrival_ms: f64,
    },
    Done,
}

/// One run: the reference's sweep, then each speaker's two.
#[derive(Debug, Clone)]
pub struct Run {
    rate: u32,
    signal: Vec<f32>,
    speakers: Vec<Speaker>,
    /// The row a step names for each speaker: its own position, except in a
    /// Measure Again run, where it is the speaker's row on the sheet.
    rows: Vec<usize>,
    reference: usize,
    order: Vec<usize>,
    states: Vec<RowState>,
    /// Rows listed on the sheet and never swept: a zone member that is off
    /// (S255).
    off: Vec<bool>,
    reference_ms: f64,
    next: Next,
    ended: Option<EndReason>,
}

impl Run {
    /// A run over `speakers`, the reference first and then the others in
    /// the order given.
    pub fn new(rate: u32, reference_id: &str, speakers: Vec<Speaker>) -> Result<Run, PlanError> {
        Run::listing(rate, reference_id, speakers, &[])
    }

    /// [`Run::new`], with each speaker whose `off` entry is true listed on
    /// the sheet as not measured and never swept, its window left out of the
    /// too-long check (S255). The reference is swept whatever its entry; a
    /// missing entry is false.
    pub fn listing(
        rate: u32,
        reference_id: &str,
        speakers: Vec<Speaker>,
        off: &[bool],
    ) -> Result<Run, PlanError> {
        let signal = calibrate::signal(rate).ok_or(PlanError::UnsupportedRate)?;
        for (i, speaker) in speakers.iter().enumerate() {
            if speakers[..i].iter().any(|other| other.id == speaker.id) {
                return Err(PlanError::DuplicateId);
            }
        }
        let reference = speakers
            .iter()
            .position(|s| s.id == reference_id && s.class == Class::Local)
            .ok_or(PlanError::NoReference)?;
        let off: Vec<bool> = (0..speakers.len())
            .map(|i| i != reference && off.get(i).copied().unwrap_or(false))
            .collect();
        let order: Vec<usize> = (0..speakers.len())
            .filter(|i| *i != reference && !off[*i])
            .collect();
        if order.is_empty() {
            return Err(PlanError::NothingToMeasure);
        }
        let mut states = vec![RowState::NotMeasured; speakers.len()];
        states[reference] = RowState::Reference;
        let run = Run {
            rate,
            signal,
            rows: (0..speakers.len()).collect(),
            speakers,
            reference,
            order,
            states,
            off,
            reference_ms: 0.0,
            next: Next::Reference,
            ended: None,
        };
        let cut = (0..run.speakers.len()).any(|row| {
            !run.off[row]
                && !run.unknown_cast(row)
                && run.frames(run.expected_ms(row)) > MAX_RECORDING_FRAMES
        });
        if cut {
            return Err(PlanError::TooLong);
        }
        Ok(run)
    }

    /// What to play and record next; `None` once the run is over.
    pub fn step(&self) -> Option<Step> {
        if self.ended.is_some() {
            return None;
        }
        let (row, take, window_frames, searching) = match self.next {
            Next::Reference => {
                let row = self.reference;
                (row, Take::Reference, self.latency_window(row), false)
            }
            Next::First(at) => {
                let row = self.order[at];
                let searching = self.unknown_cast(row);
                let window = if searching {
                    MAX_RECORDING_FRAMES
                } else {
                    self.latency_window(row)
                };
                (row, Take::Sweep(1), window, searching)
            }
            Next::Second {
                at,
                first_arrival_ms,
                ..
            } => {
                let row = self.order[at];
                let window = if self.unknown_cast(row) {
                    self.window(first_arrival_ms)
                } else {
                    self.latency_window(row)
                };
                (row, Take::Sweep(2), window, false)
            }
            Next::Done => return None,
        };
        Some(Step {
            row: self.rows[row],
            take,
            window_frames,
            searching,
        })
    }

    /// The recording the last [`Run::step`] asked for, mono at the run's
    /// rate. False when no step is waiting.
    pub fn record(&mut self, recording: &[f32]) -> bool {
        if self.ended.is_some() {
            return false;
        }
        match self.next {
            Next::Reference => match calibrate::arrival(&self.signal, recording, self.rate) {
                Ok(found) => {
                    self.reference_ms = self.ms(found.frame);
                    self.next = Next::First(0);
                }
                Err(reason) => {
                    let refused = Refused {
                        reason,
                        recording: Some(Recording::Reference),
                    };
                    for row in &self.order {
                        self.states[*row] = RowState::Refused(refused);
                    }
                    self.next = Next::Done;
                }
            },
            Next::First(at) => match self.difference(recording) {
                Ok((difference_ms, arrival_ms)) => {
                    self.next = Next::Second {
                        at,
                        first_ms: difference_ms,
                        first_arrival_ms: arrival_ms,
                    };
                }
                Err(refused) => {
                    self.states[self.order[at]] = RowState::Refused(refused);
                    self.advance(at);
                }
            },
            Next::Second { at, first_ms, .. } => {
                let row = self.order[at];
                self.states[row] = match self.difference(recording) {
                    Ok((second_ms, _)) => self.combine(row, first_ms, second_ms),
                    Err(refused) => RowState::Refused(refused),
                };
                self.advance(at);
            }
            Next::Done => return false,
        }
        true
    }

    /// Stops the run: its sheet ends with `reason` and applies nothing.
    pub fn cancel(&mut self, reason: EndReason) {
        if self.ended.is_none() && self.next != Next::Done {
            self.ended = Some(reason);
        }
    }

    /// The sheet; a run with steps still waiting ends as cancelled.
    pub fn finish(mut self) -> Sheet {
        self.cancel(EndReason::Cancelled);
        let mut sheet = Sheet {
            rate: self.rate,
            speakers: self.speakers,
            reference: self.reference,
            rows: Vec::new(),
            off: self.off,
            airplay: None,
            ended: self.ended,
            applied: false,
        };
        sheet.rows = sheet
            .speakers
            .iter()
            .zip(self.states)
            .map(|(speaker, state)| Row {
                id: speaker.id.clone(),
                class: speaker.class,
                old_ms: speaker.offset_ms,
                state,
            })
            .collect();
        sheet.airplay = shared(&sheet.rows, &sheet.off);
        sheet
    }

    fn advance(&mut self, at: usize) {
        self.next = if at + 1 < self.order.len() {
            Next::First(at + 1)
        } else {
            Next::Done
        };
    }

    /// A Cast speaker with no stored offset as [`cast_wait_ms`] reads it:
    /// none, 0 ms (what the sender keeps after Undo), less, or not a number.
    fn unknown_cast(&self, row: usize) -> bool {
        let speaker = &self.speakers[row];
        speaker.class == Class::Cast && !speaker.offset_ms.is_some_and(|ms| ms > 0.0)
    }

    fn ms(&self, frame: usize) -> f64 {
        frame as f64 * 1000.0 / f64::from(self.rate)
    }

    fn expected_ms(&self, row: usize) -> f64 {
        let latency = self.speakers[row].latency_ms;
        if latency.is_finite() { latency } else { 0.0 }
    }

    fn latency_window(&self, row: usize) -> usize {
        self.window(self.expected_ms(row))
    }

    /// The sweep expected at `arrival_ms`, its length and [`TAIL_SECONDS`],
    /// at least what the estimate accepts.
    fn frames(&self, arrival_ms: f64) -> usize {
        let rate = f64::from(self.rate);
        let ms = arrival_ms.max(0.0) + (calibrate::SIGNAL_SECONDS + TAIL_SECONDS) * 1000.0;
        let shortest = self.signal.len() + (calibrate::MIN_EXTRA_SECONDS * rate).ceil() as usize;
        let frames = (ms * rate / 1000.0).ceil().min(usize::MAX as f64) as usize;
        frames.max(shortest)
    }

    /// [`Run::frames`] within the estimate's limit. [`Run::new`] refuses a
    /// run the limit would cut, so only a searching Cast speaker's second
    /// window meets it, and its sweep was already found inside the limit.
    fn window(&self, arrival_ms: f64) -> usize {
        self.frames(arrival_ms).min(MAX_RECORDING_FRAMES)
    }

    /// How much later than the reference the sweep arrived, and where.
    fn difference(&self, recording: &[f32]) -> Result<(f64, f64), Refused> {
        let found =
            calibrate::arrival(&self.signal, recording, self.rate).map_err(|reason| Refused {
                reason,
                recording: Some(Recording::Chosen),
            })?;
        let arrival_ms = self.ms(found.frame);
        let difference_ms = arrival_ms - self.reference_ms;
        if difference_ms.abs() > MAX_OFFSET_MS - MIN_OFFSET_MS {
            return Err(Refused {
                reason: Refusal::Implausible,
                recording: None,
            });
        }
        Ok((difference_ms, arrival_ms))
    }

    fn combine(&self, row: usize, first_ms: f64, second_ms: f64) -> RowState {
        let difference_ms = (first_ms + second_ms) / 2.0;
        let uncertainty_ms = (first_ms - second_ms).abs() / 2.0 + GUARD_MS;
        let speaker = &self.speakers[row];
        let (new_ms, clamped) = moved(speaker.class, speaker.offset_ms, difference_ms);
        let measurement = Measurement {
            new_ms,
            uncertainty_ms,
            difference_ms,
            clamped,
        };
        if uncertainty_ms > MAX_UNCERTAINTY_MS {
            RowState::Uncertain(measurement)
        } else {
            RowState::Certain(measurement)
        }
    }
}

/// The stored delay as it counts: a local offset as S175 reads it, a Cast
/// offset 0 when missing, the AirPlay delay its default when missing.
fn counted(class: Class, stored_ms: Option<f64>) -> f64 {
    match class {
        Class::Local => delay::offset_ms(stored_ms),
        Class::Cast => stored_ms.and_then(delay::sanitize_delay_ms).unwrap_or(0.0),
        Class::AirPlay => stored_ms
            .and_then(delay::sanitize_airplay_delay_ms)
            .unwrap_or(DEFAULT_AIRPLAY_DELAY_MS),
    }
}

/// The new delay for a speaker heard `difference_ms` later than the
/// reference, whole ms in its class's range, and whether it was clamped.
fn moved(class: Class, stored_ms: Option<f64>, difference_ms: f64) -> (f64, bool) {
    let old = counted(class, stored_ms);
    let (wanted, low, high) = match class {
        Class::Local => (old - difference_ms, MIN_OFFSET_MS, MAX_OFFSET_MS),
        Class::Cast | Class::AirPlay => (old + difference_ms, 0.0, MAX_DELAY_MS),
    };
    let wanted = wanted.round();
    let new = wanted.clamp(low, high);
    (new, new != wanted)
}

/// The shared delay from the AirPlay rows that took part: an off row's old
/// value was read while it didn't play.
fn shared(rows: &[Row], off: &[bool]) -> Option<AirPlayShared> {
    let airplay: Vec<&Row> = rows
        .iter()
        .enumerate()
        .filter(|(i, r)| r.class == Class::AirPlay && !off.get(*i).copied().unwrap_or(false))
        .map(|(_, r)| r)
        .collect();
    let old_ms = airplay.first()?.old_ms;
    let mut differences: Vec<f64> = airplay
        .iter()
        .filter_map(|row| match row.state {
            RowState::Certain(m) => Some(m.difference_ms),
            _ => None,
        })
        .collect();
    differences.sort_by(f64::total_cmp);
    let (Some(low), Some(high)) = (differences.first(), differences.last()) else {
        return Some(AirPlayShared {
            old_ms,
            new_ms: None,
            spread_ms: 0.0,
            certain_rows: 0,
            state: SharedState::NoneCertain,
            clamped: false,
        });
    };
    let spread_ms = high - low;
    let middle = differences.len() / 2;
    let median = if differences.len() % 2 == 0 {
        (differences[middle - 1] + differences[middle]) / 2.0
    } else {
        differences[middle]
    };
    let (new_ms, clamped) = moved(Class::AirPlay, old_ms, median);
    Some(AirPlayShared {
        old_ms,
        new_ms: Some(new_ms),
        spread_ms,
        certain_rows: differences.len(),
        state: if spread_ms > MAX_AIRPLAY_SPREAD_MS {
            SharedState::Spread
        } else {
            SharedState::Certain
        },
        clamped,
    })
}

/// The results of a run: nothing changes until the host applies it.
#[derive(Debug, Clone)]
pub struct Sheet {
    rate: u32,
    speakers: Vec<Speaker>,
    reference: usize,
    rows: Vec<Row>,
    off: Vec<bool>,
    airplay: Option<AirPlayShared>,
    ended: Option<EndReason>,
    applied: bool,
}

impl Sheet {
    /// One row per speaker, in the order the run was given them.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Whether row `row` is a zone member that was off (S255): listed, not
    /// measured, until a Measure Again measures it.
    pub fn is_off(&self, row: usize) -> bool {
        self.off.get(row).copied().unwrap_or(false)
    }

    /// The shared AirPlay delay; `None` without AirPlay speakers.
    pub fn airplay(&self) -> Option<&AirPlayShared> {
        self.airplay.as_ref()
    }

    pub fn ended(&self) -> Option<EndReason> {
        self.ended
    }

    /// Whether [`Sheet::apply`] succeeded once.
    pub fn applied(&self) -> bool {
        self.applied
    }

    /// The changes for the certain rows, one for the AirPlay delay, and the
    /// values they replace, for one Undo. Refused once the sheet ended,
    /// while no row is certain, and after a first Apply: from then on the
    /// rows describe offsets that are no longer stored, so a second Apply or
    /// a Measure Again would undo the correction.
    pub fn apply(&mut self) -> Result<Applied, ApplyRefusal> {
        let applied = self.changes()?;
        self.applied = true;
        Ok(applied)
    }

    /// What [`Sheet::apply`] would return, leaving the sheet as it is.
    pub fn changes(&self) -> Result<Applied, ApplyRefusal> {
        if self.applied {
            return Err(ApplyRefusal::AlreadyApplied);
        }
        if let Some(reason) = self.ended {
            return Err(ApplyRefusal::Ended(reason));
        }
        let mut changes: Vec<Change> = self
            .rows
            .iter()
            .filter(|row| row.class != Class::AirPlay)
            .filter_map(|row| match row.state {
                RowState::Certain(m) => Some(Change {
                    target: Target::Speaker(row.id.clone()),
                    old_ms: row.old_ms,
                    new_ms: m.new_ms,
                }),
                _ => None,
            })
            .collect();
        if let Some(AirPlayShared {
            old_ms,
            new_ms: Some(new_ms),
            ..
        }) = self.airplay
        {
            changes.push(Change {
                target: Target::AirPlay,
                old_ms,
                new_ms,
            });
        }
        if changes.is_empty() {
            return Err(ApplyRefusal::NothingCertain);
        }
        let previous = changes
            .iter()
            .map(|change| (change.target.clone(), change.old_ms))
            .collect();
        Ok(Applied {
            changes,
            undo: Undo { previous },
        })
    }

    /// A run of the reference and the speaker on `row` alone, with the
    /// values the sheet started from; its steps name the sheet's rows.
    /// `None` for the reference's row, a row out of range, an off row
    /// ([`Sheet::measure_again_off`]), an ended sheet or one already
    /// applied.
    pub fn measure_again(&self, row: usize) -> Option<Run> {
        if self.is_off(row) {
            return None;
        }
        self.again(row, None)
    }

    /// Measure Again on an off row (S255): the speaker, turned on since,
    /// with its stored delay and expected arrival as the host reads them
    /// now, since the sheet read them while it didn't play. `None` for a
    /// row that isn't off and as [`Sheet::measure_again`].
    pub fn measure_again_off(
        &self,
        row: usize,
        offset_ms: Option<f64>,
        latency_ms: f64,
    ) -> Option<Run> {
        if !self.is_off(row) {
            return None;
        }
        self.again(row, Some((offset_ms, latency_ms)))
    }

    fn again(&self, row: usize, now: Option<(Option<f64>, f64)>) -> Option<Run> {
        if self.ended.is_some()
            || self.applied
            || row == self.reference
            || row >= self.speakers.len()
        {
            return None;
        }
        let reference = &self.speakers[self.reference];
        let mut measured = self.speakers[row].clone();
        if let Some((offset_ms, latency_ms)) = now {
            measured.offset_ms = offset_ms;
            measured.latency_ms = latency_ms;
        }
        let speakers = vec![reference.clone(), measured];
        let mut run = Run::new(self.rate, &reference.id, speakers).ok()?;
        run.rows = vec![self.reference, row];
        Some(run)
    }

    /// Puts the row a [`Sheet::measure_again`] run measured in place of the
    /// old one; true when it did. An again run that was cancelled or ended
    /// by a change, or that doesn't come from this sheet's Measure Again,
    /// changes nothing: the sheet and its rows stay as they were.
    pub fn replace(&mut self, again: Sheet) -> bool {
        if again.ended.is_some() || self.ended.is_some() || self.applied {
            return false;
        }
        let [_, measured] = again.rows.as_slice() else {
            return false;
        };
        let Some(at) = self.rows.iter().position(|row| row.id == measured.id) else {
            return false;
        };
        let same = |a: &Speaker, b: &Speaker| a.id == b.id && a.class == b.class;
        if at == self.reference
            || !same(&again.speakers[0], &self.speakers[self.reference])
            || !same(&again.speakers[1], &self.speakers[at])
        {
            return false;
        }
        if self.is_off(at) {
            self.speakers[at] = again.speakers[1].clone();
            self.off[at] = false;
        }
        self.rows[at] = measured.clone();
        self.airplay = shared(&self.rows, &self.off);
        true
    }
}

/// A speaker the host lists, as a Calibrate Zone run (S255) sorts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneSpeaker {
    /// Its speaker key (S149), as a zone holds it.
    pub key: String,
    /// Listed now, not hidden, not a target that needs AirPlay 2 and not
    /// playing as part of a target (S246, S247).
    pub present: bool,
    /// On and playing, so the run can sweep it.
    pub on: bool,
}

/// What a Calibrate Zone run (S255) lists, measures and silences, each by
/// its index in the speakers the host gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZonePlan {
    /// The sheet's rows: the reference, then the zone's present members in
    /// zone order.
    pub rows: Vec<usize>,
    /// One per row: a member that is off, listed and never swept, the
    /// `off` of [`Run::listing`].
    pub off: Vec<bool>,
    /// The speakers that are on and that the run doesn't measure, silenced
    /// in the mix send for the run.
    pub silenced: Vec<usize>,
}

/// The plan of Calibrate Zone (S255) over the zone whose member keys are
/// `members`, in zone order, with the reference keyed `reference`. Refused
/// as [`PlanError::NoReference`] while the reference isn't present and on,
/// and as [`PlanError::NothingToMeasure`] when no member besides it is
/// present and on.
pub fn zone_plan(
    members: &[String],
    reference: &str,
    speakers: &[ZoneSpeaker],
) -> Result<ZonePlan, PlanError> {
    let find = |key: &str| speakers.iter().position(|s| s.key == key);
    let reference_at = find(reference)
        .filter(|&i| speakers[i].present && speakers[i].on)
        .ok_or(PlanError::NoReference)?;
    let mut rows = vec![reference_at];
    let mut off = vec![false];
    for key in members {
        let Some(at) = find(key).filter(|&i| i != reference_at && speakers[i].present) else {
            continue;
        };
        if !rows.contains(&at) {
            rows.push(at);
            off.push(!speakers[at].on);
        }
    }
    if !off.iter().skip(1).any(|is_off| !is_off) {
        return Err(PlanError::NothingToMeasure);
    }
    let silenced = (0..speakers.len())
        .filter(|i| speakers[*i].on && !rows.contains(i))
        .collect();
    Ok(ZonePlan {
        rows,
        off,
        silenced,
    })
}

/// Whom the shared AirPlay delay of a Calibrate Zone sheet moves besides
/// the zone (S237, S255).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AirPlayMoves {
    /// The other zones with a listed AirPlay speaker, by name, in zone order.
    pub zones: Vec<String>,
    /// A listed AirPlay speaker is in no zone.
    pub no_zone: bool,
}

/// What the AirPlay line of a sheet measured over zone `measured` names,
/// `airplay` being the keys of every AirPlay speaker listed now.
pub fn airplay_moves<'a>(
    zones: &Zones,
    measured: &str,
    airplay: impl IntoIterator<Item = &'a str>,
) -> AirPlayMoves {
    let airplay: Vec<&str> = airplay.into_iter().collect();
    let holds = |zone: &Zone| zone.members.iter().any(|m| airplay.contains(&m.as_str()));
    AirPlayMoves {
        zones: zones
            .list()
            .iter()
            .filter(|zone| zone.id != measured && holds(zone))
            .map(|zone| zone.name.clone())
            .collect(),
        no_zone: airplay.iter().any(|key| zones.zone_of(key).is_none()),
    }
}
