//! Calibrate All Speakers (spec S235 to S241): the reference and then every
//! speaker that is on, local outputs, AirPlay speakers and the Cast speaker
//! alike, each heard alone while the default input device records, in the
//! order and for as long as the core's [`Run`] asks. Its [`Sheet`] is what
//! the window shows; nothing changes until the user applies it.
//!
//! The sweep reaches a local output through the ring and a network speaker
//! through its sender's feed ([`Route`]); the others stay silent: the local
//! outputs at gain 0, the other AirPlay speakers muted, and every feed but
//! the sweep's carrying the faded music. [`AllRun`] is ticked by the UI
//! thread; its decisions need no hardware, so they are tested against an
//! [`AllRig`] of fakes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use audiosync_core::calibrate::{MAX_RATE, MIN_RATE, SIGNAL_SECONDS};
use audiosync_core::calibrate_all::{
    Class, EndReason, PlanError, RowState, Run, Sheet, Speaker, Step as RunStep, Take, ZoneSpeaker,
    airplay_wait_ms, cast_wait_ms, zone_plan,
};
use audiosync_core::policy::delay::{MAX_HOLD_BACK_MS, native_airplay_latency_ms};
use audiosync_core::policy::speakers::SpeakerKey;

use super::backend::{AudioBackend, CastStatus};
use super::calibration::{
    CalibrationTap, Failure, LATENCY_ALLOWANCE_S, Microphone, MicrophoneError, MicrophoneId,
    Missing, Route, SIGNAL_WAIT_S, STALL_S, WARM_UP_LIMIT_S, WARM_UP_S, ns, trim_start,
};
use super::outputs::{DeviceTraits, MicKind};
use crate::debug_log::DebugLog;

/// A speaker that is on, as a run sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    /// Its backend ID: an endpoint ID, or an AirPlay or Cast device ID.
    pub id: String,
    pub class: Class,
    /// Its stored delay as the core reads it for `class`: a local offset,
    /// the AirPlay delay as shown, or a Cast offset (`None`: none stored).
    pub offset_ms: Option<f64>,
    /// When its sweep is expected in a recording that starts as the sweep
    /// enters its path (S235), in ms.
    pub latency_ms: f64,
    /// The AirPlay sender latency L_s read with `offset_ms` (S238); only an
    /// AirPlay member's is used.
    pub sender_ms: f64,
}

impl Member {
    /// Its ID on the sheet, unique across the classes.
    pub fn key(&self) -> String {
        member_key(self.class, &self.id)
    }

    /// Its speaker key (S149), as a zone holds it.
    pub fn speaker_key(&self) -> String {
        let id = self.id.clone();
        match self.class {
            Class::Local => SpeakerKey::Local(id),
            Class::AirPlay => SpeakerKey::AirPlay(id),
            Class::Cast => SpeakerKey::Cast(id),
        }
        .to_string()
    }

    fn same(&self, other: &Member) -> bool {
        self.class == other.class && self.id == other.id
    }

    fn route(&self) -> Route {
        match self.class {
            Class::Local => Route::Ring,
            Class::AirPlay => Route::AirPlay,
            Class::Cast => Route::Cast,
        }
    }
}

pub fn member_key(class: Class, id: &str) -> String {
    let prefix = match class {
        Class::Local => "local",
        Class::AirPlay => "airplay",
        Class::Cast => "cast",
    };
    format!("{prefix}:{id}")
}

/// What plays now, as a run depends on it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lineup {
    /// The running session's number; `None` while streaming is off.
    pub session: Option<u64>,
    /// The reference output, by endpoint ID.
    pub reference: Option<String>,
    /// Every speaker that plays: the reference first, the other local
    /// outputs, the AirPlay speakers, then the Cast speaker.
    pub members: Vec<Member>,
    /// Each playing local output's hold-back, in ms.
    pub hold_backs_ms: BTreeMap<String, f64>,
    /// The playing local outputs' devices, by endpoint ID; an output
    /// missing here takes part.
    pub devices: BTreeMap<String, LocalDevice>,
    /// The reference's switch is off: named with streaming while nothing plays.
    pub reference_off: bool,
    /// Every speaker whose switch is off, by class and ID, for the same text.
    pub switched_off: Vec<(Class, String)>,
}

/// A playing local output's device, as the plan reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalDevice {
    pub name: String,
    pub traits: DeviceTraits,
}

/// A playing local output that microphone calibration leaves out by its
/// kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftOut {
    /// Its endpoint ID.
    pub id: String,
    pub kind: MicKind,
    /// The user chose Include Anyway: it is measured as a speaker.
    pub included: bool,
}

impl Lineup {
    fn kind_of(&self, member: &Member) -> MicKind {
        if member.class != Class::Local || self.reference.as_deref() == Some(member.id.as_str()) {
            return MicKind::Speaker;
        }
        self.devices
            .get(&member.id)
            .map_or(MicKind::Speaker, |device| device.traits.mic_kind())
    }

    /// The lineup a run measures, and the outputs left out by kind in their
    /// order. One whose ID is in `included` stays in, flagged as included.
    pub fn plan(&self, included: &BTreeSet<String>) -> (Lineup, Vec<LeftOut>) {
        let mut planned = self.clone();
        let mut left_out = Vec::new();
        planned.members.retain(|member| {
            let kind = self.kind_of(member);
            if kind == MicKind::Speaker {
                return true;
            }
            let included = included.contains(&member.id);
            left_out.push(LeftOut {
                id: member.id.clone(),
                kind,
                included,
            });
            included
        });
        (planned, left_out)
    }
}

/// Why a run can't start over `lineup` (S235): a reference that plays and
/// another speaker that plays and that the microphone can hear.
pub fn blocked(lineup: &Lineup, included: &BTreeSet<String>) -> Option<Failure> {
    let Some(reference) = lineup.reference.as_deref() else {
        return Some(Failure::NoReference);
    };
    let reference_plays = lineup
        .members
        .iter()
        .any(|m| m.class == Class::Local && m.id == reference);
    if lineup.session.is_none() {
        return Some(Failure::NotPlaying(Missing {
            reference: lineup.reference_off,
            ..Missing::STREAMING
        }));
    }
    if !reference_plays {
        return Some(Failure::NotPlaying(Missing::REFERENCE));
    }
    let (planned, left_out) = lineup.plan(included);
    if planned.members.len() >= 2 {
        return None;
    }
    Some(if left_out.iter().any(|left| !left.included) {
        Failure::OnlyLeftOut
    } else {
        Failure::NothingToMeasure
    })
}

/// The zone a Calibrate Zone run measures (S255), as the host reads it at
/// the start.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZoneScope {
    /// The zone's member keys (S149), in zone order.
    pub members: Vec<String>,
    /// Of those, the present ones (S246): listed, not hidden, not a target
    /// that needs AirPlay 2 and not playing as part of a target.
    pub present: BTreeSet<String>,
    /// The present members that are off, with their stored delays: listed
    /// on the sheet and never swept.
    pub off: Vec<Member>,
}

impl ZoneScope {
    /// The sheet's rows over `everyone`, each with whether it is off: the
    /// reference, then the present members in zone order.
    fn rows(&self, everyone: &Lineup) -> Result<Vec<(Member, bool)>, PlanError> {
        let reference = everyone
            .reference
            .as_deref()
            .map(|id| SpeakerKey::Local(id.to_owned()).to_string())
            .ok_or(PlanError::NoReference)?;
        let playing = everyone.members.len();
        let all: Vec<&Member> = everyone.members.iter().chain(&self.off).collect();
        let speakers: Vec<ZoneSpeaker> = all
            .iter()
            .enumerate()
            .map(|(i, member)| {
                let key = member.speaker_key();
                ZoneSpeaker {
                    present: self.present.contains(&key) || !self.members.contains(&key),
                    on: i < playing,
                    key,
                }
            })
            .collect();
        let plan = zone_plan(&self.members, &reference, &speakers)?;
        Ok(plan
            .rows
            .iter()
            .zip(plan.off)
            .map(|(i, off)| (all[*i].clone(), off))
            .collect())
    }
}

/// What a run uses of the app: the backend, through the blanket impl below,
/// or fakes in the tests.
pub trait AllRig {
    fn lineup(&self) -> Lineup;
    fn tap(&self) -> Option<Arc<CalibrationTap>>;
    /// Only `who` is heard; `None`: every speaker again.
    fn solo(&mut self, who: Option<&Member>);
    /// Starts recording the default input device, with room for `seconds`.
    fn open_microphone(&mut self, seconds: f64) -> Result<Box<dyn Microphone>, MicrophoneError>;
}

impl<A: AudioBackend> AllRig for A {
    fn lineup(&self) -> Lineup {
        lineup(self)
    }

    fn tap(&self) -> Option<Arc<CalibrationTap>> {
        self.calibration_tap()
    }

    fn solo(&mut self, who: Option<&Member>) {
        // A network speaker's ID matches no local output: they all go quiet.
        self.set_calibration_solo(who.map(|member| member.id.as_str()));
        self.set_calibration_airplay_solo(
            who.filter(|member| member.class == Class::AirPlay)
                .map(|member| member.id.as_str()),
        );
    }

    fn open_microphone(&mut self, seconds: f64) -> Result<Box<dyn Microphone>, MicrophoneError> {
        AudioBackend::open_microphone(self, seconds)
    }
}

/// The speakers that play on `audio`, with their stored delays and the
/// latencies their windows are cut from.
pub fn lineup<A: AudioBackend + ?Sized>(audio: &A) -> Lineup {
    let engine = audio.calibration_engine();
    let outputs = audio.outputs();
    let allowance_ms = LATENCY_ALLOWANCE_S * 1000.0;
    let devices = engine
        .playing
        .iter()
        .filter_map(|id| {
            let output = outputs.iter().find(|output| output.id == *id)?;
            let device = LocalDevice {
                name: output.name.clone(),
                traits: output.traits.clone(),
            };
            Some((id.clone(), device))
        })
        .collect();
    let mut locals: Vec<Member> = engine
        .playing
        .iter()
        .map(|id| Member {
            id: id.clone(),
            class: Class::Local,
            offset_ms: outputs
                .iter()
                .find(|output| output.id == *id)
                .map(|output| output.delay_ms),
            latency_ms: finite_ms(engine.hold_backs_ms.get(id).copied()) + allowance_ms,
            sender_ms: 0.0,
        })
        .collect();
    locals.sort_by(|a, b| {
        let first = |m: &Member| engine.reference.as_deref() != Some(m.id.as_str());
        (first(a), &a.id).cmp(&(first(b), &b.id))
    });
    let casts: Vec<Member> = audio
        .cast_speakers()
        .into_iter()
        .filter(|cast| cast.enabled && cast.status == CastStatus::Playing)
        .map(|cast| {
            let offset_ms = (cast.offset_ms > 0.0).then_some(cast.offset_ms);
            Member {
                offset_ms,
                latency_ms: cast_wait_ms(offset_ms, cast.hold_back_ms, cast.format),
                id: cast.id,
                class: Class::Cast,
                sender_ms: 0.0,
            }
        })
        .collect();
    let cast_offset_ms = casts
        .iter()
        .filter_map(|cast| cast.offset_ms)
        .fold(0.0, f64::max);
    let sender_ms = audio.airplay_sender_latency_ms();
    let base_ms = sender_ms.unwrap_or_else(|| audio.airplay_adjustment_base_ms());
    let shown_ms = native_airplay_latency_ms(base_ms, audio.airplay_adjustment());
    let airplay: Vec<Member> = audio
        .airplay_speakers()
        .into_iter()
        .filter(|speaker| speaker.enabled && speaker.playing)
        .map(|speaker| Member {
            latency_ms: airplay_wait_ms(
                sender_ms,
                shown_ms.max(cast_offset_ms),
                speaker.delay_ms,
                speaker.reported_latency_ms,
            ),
            id: speaker.id,
            class: Class::AirPlay,
            offset_ms: Some(shown_ms),
            sender_ms: base_ms,
        })
        .collect();
    let reference_off = outputs
        .iter()
        .any(|output| output.is_reference && !output.enabled);
    let switched_off = outputs
        .iter()
        .filter(|output| !output.enabled)
        .map(|output| (Class::Local, output.id.clone()))
        .chain(
            audio
                .airplay_speakers()
                .into_iter()
                .filter(|speaker| !speaker.enabled)
                .map(|speaker| (Class::AirPlay, speaker.id)),
        )
        .chain(
            audio
                .cast_speakers()
                .into_iter()
                .filter(|cast| !cast.enabled)
                .map(|cast| (Class::Cast, cast.id)),
        )
        .collect();
    Lineup {
        session: engine.session,
        reference: engine.reference,
        members: locals.into_iter().chain(airplay).chain(casts).collect(),
        hold_backs_ms: engine.hold_backs_ms,
        devices,
        reference_off,
        switched_off,
    }
}

fn finite_ms(ms: Option<f64>) -> f64 {
    ms.filter(|ms| ms.is_finite()).unwrap_or(0.0).max(0.0)
}

/// Where a run stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The music fades out and what is on its way plays out.
    Preparing,
    /// A sweep plays on the sheet's row `row` alone while the microphone
    /// records; `searching` for a Cast speaker without a stored offset.
    Measuring {
        row: usize,
        take: Take,
        searching: bool,
    },
    /// The core looks for the sweep in the recording.
    Estimating {
        row: usize,
        take: Take,
    },
    Ended,
}

/// How a run ended.
pub struct Finished {
    /// The results; `None` only when the estimate itself was lost.
    pub sheet: Option<Sheet>,
    /// The microphone or the capture stopped the run.
    pub failure: Option<Failure>,
}

enum Stage {
    Quiet {
        until_ns: u64,
    },
    Warming {
        step: RunStep,
        mic: Box<dyn Microphone>,
        since_ns: u64,
    },
    Playing {
        step: RunStep,
        mic: Box<dyn Microphone>,
        asked_ns: u64,
    },
    Recording {
        step: RunStep,
        mic: Box<dyn Microphone>,
        start: usize,
        deadline_ns: u64,
    },
    /// The run is on a thread of its own: a searching window takes an FFT
    /// of millions of frames.
    Estimating {
        step: RunStep,
        receive: Receiver<Run>,
    },
    /// Ended; a cut-off sweep plays out on the speaker heard before the
    /// others are heard again.
    Settling {
        until_ns: u64,
    },
    Over,
}

impl Stage {
    /// The microphone stopped on its own before its recording was whole:
    /// waiting for the deadline would only keep the music faded.
    fn microphone_lost(&self) -> bool {
        match self {
            Stage::Warming { mic, .. } | Stage::Playing { mic, .. } => mic.stopped(),
            Stage::Recording {
                step, mic, start, ..
            } => mic.stopped() && mic.frames_recorded() < start + step.window_frames,
            _ => false,
        }
    }
}

/// One Calibrate All run, or one Measure Again, from the music fading out
/// to the sheet. The UI thread ticks it every few tens of ms.
pub struct AllRun {
    run: Option<Run>,
    /// The sheet's rows.
    members: Vec<Member>,
    /// The rows this run measures, the reference's first.
    taking_part: Vec<usize>,
    /// The speakers that play and aren't measured, kept quiet in the mix
    /// send by the solo until the run gives the outputs back (S255).
    silenced: Vec<Member>,
    /// The row a Measure Again measures.
    again: Option<usize>,
    /// The outputs measured whatever their kind.
    included: BTreeSet<String>,
    rate: u32,
    /// The reference, by endpoint ID.
    reference: String,
    stage: Stage,
    tap: Arc<CalibrationTap>,
    session: u64,
    /// The hold-backs of the local outputs taking part, at the start.
    hold_backs_ms: BTreeMap<String, f64>,
    first_microphone: Option<MicrophoneId>,
    /// Every speaker on the last sweep's path has played it by then, so
    /// none of them may be heard again before.
    clear_ns: u64,
    progress: Progress,
    /// A change seen while the estimate ran, applied once it is back.
    pending_end: Option<EndReason>,
    finished: Option<Finished>,
    log: DebugLog,
}

impl AllRun {
    /// Starts a run over every speaker that plays and that the microphone
    /// can hear, with the outputs in `included` measured whatever their
    /// kind: the music fades out. Refused with why when it can't start;
    /// nothing changed then.
    pub fn start(
        rig: &mut dyn AllRig,
        included: &BTreeSet<String>,
        now_ns: u64,
        log: DebugLog,
    ) -> Result<AllRun, Failure> {
        AllRun::start_over(rig, included, None, now_ns, log)
    }

    /// Calibrate Zone (S255): a run over `zone`'s present members that are
    /// on, its members that are off listed and never swept, and every other
    /// speaker that plays silenced until the run gives the outputs back.
    pub fn start_zone(
        rig: &mut dyn AllRig,
        included: &BTreeSet<String>,
        zone: &ZoneScope,
        now_ns: u64,
        log: DebugLog,
    ) -> Result<AllRun, Failure> {
        AllRun::start_over(rig, included, Some(zone), now_ns, log)
    }

    fn start_over(
        rig: &mut dyn AllRig,
        included: &BTreeSet<String>,
        zone: Option<&ZoneScope>,
        now_ns: u64,
        log: DebugLog,
    ) -> Result<AllRun, Failure> {
        let everyone = rig.lineup();
        let refuse = |failure: Failure| {
            log.log(|| format!("Calibrate All refused: {failure:?}"));
            Err(failure)
        };
        log_plan(&log, &everyone, included);
        if let Some(failure) = blocked(&everyone, included) {
            return refuse(failure);
        }
        let (Some(reference), Some(session), Some(tap)) =
            (everyone.reference.clone(), everyone.session, rig.tap())
        else {
            return refuse(Failure::NotPlaying(Missing::STREAMING));
        };
        let rows = match zone.map(|zone| zone.rows(&everyone)) {
            None => everyone
                .members
                .iter()
                .map(|m| (m.clone(), false))
                .collect(),
            Some(Ok(rows)) => rows,
            Some(Err(error)) => return refuse(plan_failure(error)),
        };
        let on = Lineup {
            members: rows
                .iter()
                .filter(|(_, off)| !off)
                .map(|(m, _)| m.clone())
                .collect(),
            ..everyone.clone()
        };
        if let Some(failure) = blocked(&on, included) {
            return refuse(failure);
        }
        let measured = on.plan(included).0.members;
        let rows: Vec<(Member, bool)> = rows
            .into_iter()
            .filter(|(m, off)| *off || measured.iter().any(|n| n.same(m)))
            .collect();
        let rate = match probe(rig) {
            Ok(rate) => rate,
            Err(failure) => return refuse(failure),
        };
        let speakers = rows.iter().map(|(m, _)| speaker(m)).collect();
        let off: Vec<bool> = rows.iter().map(|(_, off)| *off).collect();
        let run = match Run::listing(rate, &member_key(Class::Local, &reference), speakers, &off) {
            Ok(run) => run,
            Err(error) => return refuse(plan_failure(error)),
        };
        let members: Vec<Member> = rows.into_iter().map(|(m, _)| m).collect();
        let taking_part: Vec<usize> = (0..members.len()).filter(|row| !off[*row]).collect();
        let silenced = everyone
            .plan(included)
            .0
            .members
            .into_iter()
            .filter(|m| !measured.iter().any(|n| n.same(m)))
            .collect();
        log.log(|| {
            let keys: Vec<String> = members.iter().map(Member::key).collect();
            format!(
                "Calibrate All started at {rate} Hz against {reference}: {keys:?}, off {off:?}, silenced {silenced:?}"
            )
        });
        Ok(AllRun::new(
            run,
            members,
            taking_part,
            silenced,
            None,
            included.clone(),
            rate,
            reference,
            session,
            tap,
            &everyone,
            now_ns,
            log,
        ))
    }

    /// Measures the sheet's row `row` again against a new reference sweep
    /// (S239). `members` and `rate` are the sheet's own.
    pub fn again(
        rig: &mut dyn AllRig,
        sheet: &Sheet,
        members: &[Member],
        rate: u32,
        row: usize,
        now_ns: u64,
        log: DebugLog,
    ) -> Result<AllRun, Failure> {
        let refuse = |failure: Failure| {
            log.log(|| format!("Measure Again of row {row} refused: {failure:?}"));
            Err(failure)
        };
        let off = sheet.is_off(row);
        if !off && sheet.measure_again(row).is_none() {
            return refuse(Failure::Unfinished);
        }
        let Some(reference_row) = sheet
            .rows()
            .iter()
            .position(|r| r.state == RowState::Reference)
        else {
            return refuse(Failure::NoReference);
        };
        let (Some(reference), Some(measured)) = (members.get(reference_row), members.get(row))
        else {
            return refuse(Failure::Unfinished);
        };
        let lineup = rig.lineup();
        if lineup.reference.as_deref() != Some(reference.id.as_str()) {
            return refuse(Failure::NoReference);
        }
        let now_of = |m: &Member| {
            lineup
                .members
                .iter()
                .find(|n| n.class == m.class && n.id == m.id)
        };
        let (Some(session), Some(tap), Some(_), Some(now)) = (
            lineup.session,
            rig.tap(),
            now_of(reference),
            now_of(measured),
        ) else {
            let stopped = lineup.session.is_none() || rig.tap().is_none();
            return refuse(Failure::NotPlaying(Missing {
                streaming: stopped,
                speaker: if stopped {
                    lineup
                        .switched_off
                        .iter()
                        .any(|(class, id)| *class == measured.class && *id == measured.id)
                } else {
                    now_of(measured).is_none()
                },
                reference: if stopped {
                    lineup.reference_off
                } else {
                    now_of(reference).is_none()
                },
            }));
        };
        // An off row was read while it didn't play: it starts from its values now (S255).
        let run = if off {
            sheet.measure_again_off(row, now.offset_ms, now.latency_ms)
        } else if now.offset_ms != measured.offset_ms {
            return refuse(Failure::Interrupted);
        } else {
            sheet.measure_again(row)
        };
        let Some(run) = run else {
            return refuse(Failure::Unfinished);
        };
        match probe(rig) {
            Ok(heard) if heard == rate => {}
            Ok(_) => return refuse(Failure::MicrophoneChanged),
            Err(failure) => return refuse(failure),
        }
        log.log(|| format!("Measure Again of {} started", measured.key()));
        // The sheet's rows were all measured, whatever their kind.
        let included = members
            .iter()
            .filter(|m| m.class == Class::Local)
            .map(|m| m.id.clone())
            .collect();
        let silenced = lineup
            .members
            .iter()
            .filter(|m| !m.same(reference) && !m.same(measured))
            .cloned()
            .collect();
        let mut members = members.to_vec();
        if off {
            members[row] = now.clone();
        }
        Ok(AllRun::new(
            run,
            members,
            vec![reference_row, row],
            silenced,
            Some(row),
            included,
            rate,
            reference.id.clone(),
            session,
            tap,
            &lineup,
            now_ns,
            log,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        run: Run,
        members: Vec<Member>,
        taking_part: Vec<usize>,
        silenced: Vec<Member>,
        again: Option<usize>,
        included: BTreeSet<String>,
        rate: u32,
        reference: String,
        session: u64,
        tap: Arc<CalibrationTap>,
        lineup: &Lineup,
        now_ns: u64,
        log: DebugLog,
    ) -> AllRun {
        let hold_backs_ms = taking_part
            .iter()
            .filter(|row| members[**row].class == Class::Local)
            .filter_map(|row| {
                let id = &members[*row].id;
                lineup.hold_backs_ms.get(id).map(|ms| (id.clone(), *ms))
            })
            .collect();
        // Every speaker still plays the music until it faded: the longest
        // way any of them has to play out sets the wait.
        let quiet_ms = lineup
            .members
            .iter()
            .map(|m| m.latency_ms)
            .fold(LATENCY_ALLOWANCE_S * 1000.0, f64::max)
            .min(MAX_HOLD_BACK_MS);
        tap.set_route(Route::Ring);
        tap.engage();
        AllRun {
            run: Some(run),
            members,
            taking_part,
            silenced,
            again,
            included,
            rate,
            reference,
            stage: Stage::Quiet {
                until_ns: now_ns + ns(quiet_ms / 1000.0),
            },
            tap,
            session,
            hold_backs_ms,
            first_microphone: None,
            clear_ns: 0,
            progress: Progress::Preparing,
            pending_end: None,
            finished: None,
            log,
        }
    }

    /// The sheet's rows, with what each speaker was at the start.
    pub fn members(&self) -> &[Member] {
        &self.members
    }

    /// The microphone's rate the run records at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The row a Measure Again measures; `None` for a whole run.
    pub fn again_row(&self) -> Option<usize> {
        self.again
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }

    pub fn is_running(&self) -> bool {
        self.progress != Progress::Ended
    }

    /// Whether it still holds the outputs (running, or a cut-off sweep
    /// playing out).
    pub fn holds_outputs(&self) -> bool {
        !matches!(self.stage, Stage::Over)
    }

    /// How the run ended, once; the outputs may still be held a moment.
    pub fn take_finished(&mut self) -> Option<Finished> {
        self.finished.take()
    }

    /// The user's Cancel: the sweep fades out and nothing changes.
    pub fn cancel(&mut self, rig: &mut dyn AllRig, now_ns: u64) {
        self.stop(rig, EndReason::Cancelled, now_ns);
    }

    /// Stops the run with `reason`, as a change the host sees (a zone
    /// click, the measured zone changing, S240) or the user's Cancel.
    pub fn stop(&mut self, rig: &mut dyn AllRig, reason: EndReason, now_ns: u64) {
        if !self.is_running() {
            return;
        }
        self.log
            .log(|| format!("Calibrate All stopped: {reason:?}"));
        if matches!(self.stage, Stage::Estimating { .. }) {
            self.pending_end.get_or_insert(reason);
        } else {
            self.end(rig, Some(reason), None, now_ns);
        }
    }

    /// Moves the run on. Returns whether what the window shows changed.
    pub fn tick(&mut self, rig: &mut dyn AllRig, now_ns: u64) -> bool {
        let before = (self.progress, self.holds_outputs());
        self.advance(rig, now_ns);
        before != (self.progress, self.holds_outputs())
    }

    fn advance(&mut self, rig: &mut dyn AllRig, now_ns: u64) {
        match &self.stage {
            Stage::Over => return,
            Stage::Settling { until_ns } => {
                if rig.lineup().session != Some(self.session) || now_ns >= *until_ns {
                    self.give_back(rig);
                }
                return;
            }
            _ => {}
        }
        if let Some(reason) = self.change(&rig.lineup().plan(&self.included).0) {
            if !matches!(self.stage, Stage::Estimating { .. }) {
                self.log
                    .log(|| format!("Calibrate All stopped by a change: {reason:?}"));
                return self.end(rig, Some(reason), None, now_ns);
            }
            self.pending_end.get_or_insert(reason);
        }
        if self.stage.microphone_lost() {
            return self.fail(rig, Failure::NoMicrophone, now_ns);
        }
        match std::mem::replace(&mut self.stage, Stage::Over) {
            Stage::Quiet { until_ns } => {
                self.stage = Stage::Quiet { until_ns };
                if now_ns >= until_ns {
                    self.next_step(rig, now_ns);
                }
            }
            Stage::Warming {
                step,
                mic,
                since_ns,
            } => {
                if mic.first_frame_ns().is_some() && now_ns >= since_ns + ns(WARM_UP_S) {
                    self.tap.play();
                    // Until it starts, the latest it may start counts.
                    self.clear_ns = now_ns + ns(SIGNAL_WAIT_S) + self.played_out_ns(step.row);
                    self.stage = Stage::Playing {
                        step,
                        mic,
                        asked_ns: now_ns,
                    };
                } else {
                    let cold = now_ns >= since_ns + ns(WARM_UP_LIMIT_S);
                    self.stage = Stage::Warming {
                        step,
                        mic,
                        since_ns,
                    };
                    if cold {
                        self.fail(rig, Failure::NoMicrophone, now_ns);
                    }
                }
            }
            Stage::Playing {
                step,
                mic,
                asked_ns,
            } => match (self.tap.played_at_ns(), mic.first_frame_ns()) {
                (Some(played), Some(first)) => {
                    let Some(start) = trim_start(played, first, self.rate) else {
                        self.stage = Stage::Playing {
                            step,
                            mic,
                            asked_ns,
                        };
                        return self.fail(rig, Failure::NoMicrophone, now_ns);
                    };
                    self.clear_ns = played + self.played_out_ns(step.row);
                    let seconds = step.window_frames as f64 / f64::from(self.rate);
                    self.stage = Stage::Recording {
                        step,
                        mic,
                        start,
                        deadline_ns: now_ns + ns(seconds + STALL_S),
                    };
                }
                _ => {
                    let silent = now_ns >= asked_ns + ns(SIGNAL_WAIT_S);
                    self.stage = Stage::Playing {
                        step,
                        mic,
                        asked_ns,
                    };
                    if silent {
                        self.fail(rig, Failure::SourceSilent, now_ns);
                    }
                }
            },
            Stage::Recording {
                step,
                mut mic,
                start,
                deadline_ns,
            } => {
                let end = start + step.window_frames;
                let waiting = mic.frames_recorded() < end && now_ns < deadline_ns;
                let skipped = mic.gap_at().is_some_and(|gap| gap < end);
                if waiting || skipped {
                    self.stage = Stage::Recording {
                        step,
                        mic,
                        start,
                        deadline_ns,
                    };
                    if skipped {
                        self.fail(rig, Failure::MicrophoneSkipped, now_ns);
                    }
                    return;
                }
                let samples = mic.take(start, step.window_frames);
                drop(mic);
                self.log.log(|| {
                    format!(
                        "Calibrate All: row {} {:?} recorded, {} frames from frame {start}",
                        step.row,
                        step.take,
                        samples.len()
                    )
                });
                self.estimate(rig, step, samples, now_ns);
            }
            Stage::Estimating { step, receive } => match receive.try_recv() {
                Ok(run) => {
                    self.run = Some(run);
                    match self.pending_end.take() {
                        Some(reason) => self.end(rig, Some(reason), None, now_ns),
                        None => self.next_step(rig, now_ns),
                    }
                }
                Err(TryRecvError::Empty) => self.stage = Stage::Estimating { step, receive },
                Err(TryRecvError::Disconnected) => {
                    self.progress = Progress::Measuring {
                        row: step.row,
                        take: step.take,
                        searching: false,
                    };
                    self.fail(rig, Failure::Unfinished, now_ns);
                }
            },
            other => self.stage = other,
        }
    }

    /// Why the run can't go on, from what plays now (S240).
    fn change(&self, lineup: &Lineup) -> Option<EndReason> {
        if lineup.session != Some(self.session) {
            return Some(EndReason::SourceChanged);
        }
        if lineup.reference.as_deref() != Some(self.reference.as_str()) {
            return Some(EndReason::SpeakersChanged);
        }
        for row in &self.taking_part {
            let member = &self.members[*row];
            match lineup.members.iter().find(|now| now.same(member)) {
                None => return Some(EndReason::SpeakerTurnedOff),
                Some(now) if now.offset_ms != member.offset_ms => {
                    return Some(EndReason::SpeakersChanged);
                }
                Some(_) => {}
            }
        }
        // A silenced speaker turned off, or any other turned on, changes the list.
        let known = |now: &Member| {
            self.taking_part
                .iter()
                .map(|row| &self.members[*row])
                .chain(&self.silenced)
                .any(|m| m.same(now))
        };
        let listed = |m: &Member| lineup.members.iter().any(|now| now.same(m));
        if self.again.is_none()
            && (!lineup.members.iter().all(known) || !self.silenced.iter().all(listed))
        {
            return Some(EndReason::SpeakersChanged);
        }
        let moved = self
            .hold_backs_ms
            .iter()
            .any(|(id, ms)| lineup.hold_backs_ms.get(id) != Some(ms));
        moved.then_some(EndReason::SpeakersChanged)
    }

    fn next_step(&mut self, rig: &mut dyn AllRig, now_ns: u64) {
        match self.run.as_ref().and_then(Run::step) {
            // A speaker on the last sweep's path that plays it later than
            // the one measured would play it into this recording.
            Some(_) if now_ns < self.clear_ns => {
                self.stage = Stage::Quiet {
                    until_ns: self.clear_ns,
                };
            }
            Some(step) => self.begin(rig, step, now_ns),
            None => self.end(rig, None, None, now_ns),
        }
    }

    /// One sweep: only the row's speaker is heard, the signal takes its
    /// path, the microphone starts, and the signal plays once it is warm.
    fn begin(&mut self, rig: &mut dyn AllRig, step: RunStep, now_ns: u64) {
        let member = self.members[step.row].clone();
        self.progress = Progress::Measuring {
            row: step.row,
            take: step.take,
            searching: step.searching,
        };
        rig.solo(Some(&member));
        self.tap.set_route(member.route());
        let room = WARM_UP_LIMIT_S
            + SIGNAL_WAIT_S
            + step.window_frames as f64 / f64::from(self.rate)
            + STALL_S;
        let mic = match rig.open_microphone(room) {
            Ok(mic) => mic,
            Err(error) => return self.fail(rig, error.into(), now_ns),
        };
        let heard = mic.id();
        let changed = heard.rate != self.rate
            || self
                .first_microphone
                .as_ref()
                .is_some_and(|first| *first != heard);
        if changed {
            drop(mic);
            return self.fail(rig, Failure::MicrophoneChanged, now_ns);
        }
        self.log.log(|| {
            format!(
                "Calibrate All: {:?} of {}, only it heard, {} frames{}",
                step.take,
                member.key(),
                step.window_frames,
                if step.searching { ", searching" } else { "" }
            )
        });
        self.first_microphone.get_or_insert(heard);
        self.stage = Stage::Warming {
            step,
            mic,
            since_ns: now_ns,
        };
    }

    fn estimate(&mut self, rig: &mut dyn AllRig, step: RunStep, samples: Vec<f32>, now_ns: u64) {
        let Some(mut run) = self.run.take() else {
            return self.fail(rig, Failure::Unfinished, now_ns);
        };
        let (send, receive) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("audiosync-calibrate-all".into())
            .spawn(move || {
                run.record(&samples);
                let _ = send.send(run);
            });
        if spawned.is_err() {
            return self.fail(rig, Failure::Unfinished, now_ns);
        }
        self.progress = Progress::Estimating {
            row: step.row,
            take: step.take,
        };
        self.stage = Stage::Estimating { step, receive };
    }

    fn fail(&mut self, rig: &mut dyn AllRig, failure: Failure, now_ns: u64) {
        if !self.is_running() {
            return;
        }
        self.log
            .log(|| format!("Calibrate All stopped: {failure:?}"));
        self.end(rig, Some(EndReason::Cancelled), Some(failure), now_ns);
    }

    /// The sheet is ready at once. A sweep cut off on its way fades out and
    /// plays out on the one speaker heard before the others are heard again
    /// and the music fades in.
    fn end(
        &mut self,
        rig: &mut dyn AllRig,
        reason: Option<EndReason>,
        failure: Option<Failure>,
        now_ns: u64,
    ) {
        if let (Some(reason), Some(run)) = (reason, self.run.as_mut()) {
            run.cancel(reason);
        }
        let sweeping = match &self.stage {
            Stage::Playing { step, .. } | Stage::Recording { step, .. } => Some(step.row),
            _ => None,
        };
        // Dropping the stage stops its microphone.
        self.stage = Stage::Over;
        self.progress = Progress::Ended;
        let sheet = self.run.take().map(Run::finish);
        if let Some(sheet) = &sheet {
            self.log.log(|| {
                format!(
                    "Calibrate All ended ({:?}): {:?}, AirPlay {:?}",
                    sheet.ended(),
                    sheet.rows(),
                    sheet.airplay()
                )
            });
        }
        self.finished = Some(Finished { sheet, failure });
        let same_session = rig.lineup().session == Some(self.session);
        if sweeping.is_some() {
            self.tap.fade_out_signal();
        }
        if same_session && now_ns < self.clear_ns {
            self.stage = Stage::Settling {
                until_ns: self.clear_ns,
            };
        } else {
            self.give_back(rig);
        }
    }

    /// How long after a sweep enters row `row`'s path every speaker on that
    /// path has played it: the AirPlay speakers share one feed, each after
    /// its own delay, and the local outputs share the ring.
    fn played_out_ns(&self, row: usize) -> u64 {
        let route = self.members[row].route();
        let latency_ms = self
            .members
            .iter()
            .chain(&self.silenced)
            .filter(|member| member.route() == route)
            .map(|member| member.latency_ms)
            .fold(0.0, f64::max)
            .min(MAX_HOLD_BACK_MS);
        ns(latency_ms / 1000.0 + SIGNAL_SECONDS)
    }

    /// Every speaker heard again, the music fading back in and the signal
    /// back on the ring's path.
    fn give_back(&mut self, rig: &mut dyn AllRig) {
        self.stage = Stage::Over;
        rig.solo(None);
        self.tap.disengage();
        self.tap.set_route(Route::Ring);
    }
}

impl Drop for AllRun {
    fn drop(&mut self) {
        if self.holds_outputs() {
            self.tap.disengage();
            self.tap.set_route(Route::Ring);
        }
    }
}

/// One line per playing local output: what Windows said about it and
/// whether the run measures it.
fn log_plan(log: &DebugLog, lineup: &Lineup, included: &BTreeSet<String>) {
    for member in lineup.members.iter().filter(|m| m.class == Class::Local) {
        let device = lineup.devices.get(&member.id).cloned().unwrap_or_default();
        let kind = lineup.kind_of(member);
        let result = match kind {
            MicKind::Speaker => "takes part",
            _ if included.contains(&member.id) => "included anyway",
            _ => "left out",
        };
        log.log(|| {
            format!(
                "Calibrate All plan: {:?} ({}), form factor {:?}, enumerator {:?}: {kind:?}, {result}",
                device.name, member.id, device.traits.form_factor, device.traits.enumerator
            )
        });
    }
}

fn speaker(member: &Member) -> Speaker {
    Speaker {
        id: member.key(),
        class: member.class,
        offset_ms: member.offset_ms,
        latency_ms: member.latency_ms,
    }
}

/// The microphone's rate, from a recording opened and closed at once:
/// Windows' privacy settings show only when a stream opens, so a denial
/// comes before the music fades.
fn probe(rig: &mut dyn AllRig) -> Result<u32, Failure> {
    let mut probe = rig.open_microphone(0.1).map_err(Failure::from)?;
    let rate = probe.id().rate;
    drop(probe.take(0, 0));
    if !(MIN_RATE..=MAX_RATE).contains(&rate) {
        return Err(Failure::UnsupportedRate);
    }
    Ok(rate)
}

fn plan_failure(error: PlanError) -> Failure {
    match error {
        PlanError::UnsupportedRate => Failure::UnsupportedRate,
        PlanError::NoReference => Failure::NoReference,
        PlanError::NothingToMeasure => Failure::NothingToMeasure,
        PlanError::DuplicateId => Failure::Unfinished,
        PlanError::TooLong => Failure::TooLong(None),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use audiosync_core::calibrate;
    use audiosync_core::calibrate_all::{ApplyRefusal, SharedState};

    use super::*;

    /// The lowest rate the signal is made at keeps the estimates quick.
    const MIC_RATE: u32 = 22_050;
    const TICK_NS: u64 = 20_000_000;

    /// What goes wrong with every recording the fake microphone opens.
    #[derive(Default)]
    struct MicFault {
        /// No first frame ever comes.
        cold: Cell<bool>,
        /// No frame comes after this moment.
        frozen_at: Cell<Option<u64>>,
        /// It stopped on its own, as an unplugged device does.
        stopped: Cell<bool>,
    }

    impl MicFault {
        fn unplug(&self, now_ns: u64) {
            self.frozen_at.set(Some(now_ns));
            self.stopped.set(true);
        }
    }

    struct FakeMic {
        id: MicrophoneId,
        first_ns: u64,
        now: Rc<Cell<u64>>,
        tap: Arc<CalibrationTap>,
        heard_after_ms: Option<f64>,
        fault: Rc<MicFault>,
    }

    impl Microphone for FakeMic {
        fn id(&self) -> MicrophoneId {
            self.id.clone()
        }

        fn first_frame_ns(&self) -> Option<u64> {
            (!self.fault.cold.get() && self.now.get() > self.first_ns).then_some(self.first_ns)
        }

        fn frames_recorded(&self) -> usize {
            let now = self
                .now
                .get()
                .min(self.fault.frozen_at.get().unwrap_or(u64::MAX));
            let since = now.saturating_sub(self.first_ns);
            (since as f64 * f64::from(self.id.rate) / 1e9) as usize
        }

        fn gap_at(&self) -> Option<usize> {
            None
        }

        fn stopped(&self) -> bool {
            self.fault.stopped.get()
        }

        fn take(&mut self, start: usize, count: usize) -> Vec<f32> {
            let count = count.min(self.frames_recorded().saturating_sub(start));
            let rate = self.id.rate;
            let mut noise = 0x1234_5678_u32;
            let mut recording: Vec<f32> = (0..count)
                .map(|_| {
                    noise ^= noise << 13;
                    noise ^= noise >> 17;
                    noise ^= noise << 5;
                    (f64::from(noise) / f64::from(u32::MAX) - 0.5) as f32 * 0.001
                })
                .collect();
            let (Some(played), Some(after)) = (self.tap.played_at_ns(), self.heard_after_ms) else {
                return recording;
            };
            let signal = calibrate::signal(rate).unwrap();
            let heard_ns = played + ns(after / 1000.0);
            let at = ((heard_ns - self.first_ns) as f64 * f64::from(rate) / 1e9).round() as usize;
            for (i, sample) in signal.iter().enumerate() {
                if let Some(slot) = (at + i)
                    .checked_sub(start)
                    .and_then(|j| recording.get_mut(j))
                {
                    *slot += sample * 0.4;
                }
            }
            recording
        }
    }

    struct FakeRig {
        now: Rc<Cell<u64>>,
        lineup: Lineup,
        tap: Arc<CalibrationTap>,
        solo: Option<Member>,
        /// Every speaker heard alone, by key, in turn.
        solos: Vec<String>,
        /// When each solo began, with the speaker heard alone; `None`:
        /// every speaker heard again.
        solo_times: Vec<(u64, Option<String>)>,
        /// How long after the sweep entered its path each speaker is heard,
        /// per sweep in turn; the last value repeats.
        heard_after_ms: BTreeMap<String, Vec<f64>>,
        sweeps: BTreeMap<String, usize>,
        mic: Result<MicrophoneId, MicrophoneError>,
        mic_fault: Rc<MicFault>,
        /// The capture writes no block, as when the source sends nothing.
        capture_off: bool,
    }

    fn member(class: Class, id: &str, offset_ms: Option<f64>, latency_ms: f64) -> Member {
        Member {
            id: id.to_owned(),
            class,
            offset_ms,
            latency_ms,
            sender_ms: 0.0,
        }
    }

    impl FakeRig {
        /// The PC's speakers as the reference, a USB speaker, an AirPlay
        /// speaker at L = 2000 ms and a Cast speaker at 3000 ms.
        fn new() -> FakeRig {
            FakeRig {
                now: Rc::new(Cell::new(1_000_000_000)),
                lineup: Lineup {
                    session: Some(1),
                    reference: Some("builtin".to_owned()),
                    members: vec![
                        member(Class::Local, "builtin", Some(0.0), 2500.0),
                        member(Class::Local, "usb", Some(150.0), 2650.0),
                        member(Class::AirPlay, "kitchen", Some(2000.0), 2500.0),
                        member(Class::Cast, "den", Some(3000.0), 6000.0),
                    ],
                    hold_backs_ms: BTreeMap::from([
                        ("builtin".to_owned(), 2000.0),
                        ("usb".to_owned(), 2150.0),
                    ]),
                    devices: BTreeMap::new(),
                    reference_off: false,
                    switched_off: Vec::new(),
                },
                tap: Arc::new(CalibrationTap::new()),
                solo: None,
                solos: Vec::new(),
                solo_times: Vec::new(),
                heard_after_ms: BTreeMap::from([
                    ("local:builtin".to_owned(), vec![2010.0]),
                    ("local:usb".to_owned(), vec![2180.0]),
                    ("airplay:kitchen".to_owned(), vec![2050.0]),
                    ("cast:den".to_owned(), vec![3100.0]),
                ]),
                sweeps: BTreeMap::new(),
                mic: Ok(MicrophoneId {
                    device: "mic".to_owned(),
                    rate: MIC_RATE,
                }),
                mic_fault: Rc::default(),
                capture_off: false,
            }
        }

        /// One capture block of 10 ms through the tap, as the converter
        /// writes it.
        fn capture_block(&self) {
            if self.capture_off {
                return;
            }
            let mut block = [0.0_f32; 960];
            let mut cursor = self.tap.block_cursor();
            let starts = cursor == 0;
            self.tap.fade_music(&mut block);
            self.tap.add_signal(&mut block, &mut cursor);
            if starts {
                self.tap.signal_started(self.now.get(), 480);
            }
        }

        fn remove(&mut self, key: &str) {
            self.lineup.members.retain(|m| m.key() != key);
        }
    }

    impl AllRig for FakeRig {
        fn lineup(&self) -> Lineup {
            self.lineup.clone()
        }

        fn tap(&self) -> Option<Arc<CalibrationTap>> {
            Some(Arc::clone(&self.tap))
        }

        fn solo(&mut self, who: Option<&Member>) {
            if let Some(who) = who {
                self.solos.push(who.key());
            }
            self.solo_times.push((self.now.get(), who.map(Member::key)));
            self.solo = who.cloned();
        }

        fn open_microphone(
            &mut self,
            _seconds: f64,
        ) -> Result<Box<dyn Microphone>, MicrophoneError> {
            let id = self.mic.clone()?;
            // Heard only along the path that reaches it.
            let heard = self
                .solo
                .clone()
                .filter(|solo| self.tap.route() == solo.route());
            let heard_after_ms = heard.and_then(|solo| {
                let key = solo.key();
                let turn = self.sweeps.entry(key.clone()).or_default();
                let values = self.heard_after_ms.get(&key)?;
                let value = values[(*turn).min(values.len() - 1)];
                *turn += 1;
                Some(value)
            });
            Ok(Box::new(FakeMic {
                id,
                first_ns: self.now.get() + 30_000_000,
                now: Rc::clone(&self.now),
                tap: Arc::clone(&self.tap),
                heard_after_ms,
                fault: Rc::clone(&self.mic_fault),
            }))
        }
    }

    /// Ticks until the run gave the outputs back; `meddle` runs before each
    /// tick.
    fn run_to_end(
        rig: &mut FakeRig,
        run: &mut AllRun,
        mut meddle: impl FnMut(&mut FakeRig, &mut AllRun),
    ) -> Finished {
        let mut finished = None;
        for _ in 0..20_000 {
            if let Some(done) = run.take_finished() {
                finished = Some(done);
            }
            if !run.holds_outputs() {
                return finished.expect("an ended run has a result");
            }
            meddle(rig, &mut *run);
            rig.now.set(rig.now.get() + TICK_NS);
            rig.capture_block();
            if matches!(run.progress(), Progress::Estimating { .. }) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let now = rig.now.get();
            run.tick(rig, now);
        }
        panic!("the run never ended: {:?}", run.progress());
    }

    /// The results of a run over local speakers `(id, stored offset, heard
    /// after ms)`, the first the reference.
    pub(crate) fn local_sheet(speakers: &[(&str, f64, f64)]) -> (Sheet, Vec<Member>, u32) {
        let speakers: Vec<(Class, &str, Option<f64>, f64)> = speakers
            .iter()
            .map(|&(id, offset, after)| (Class::Local, id, Some(offset), after))
            .collect();
        sheet_of(&speakers)
    }

    /// The results of a run over `(class, id, stored offset, heard after
    /// ms)`, the first a local output and the reference.
    pub(crate) fn sheet_of(
        speakers: &[(Class, &str, Option<f64>, f64)],
    ) -> (Sheet, Vec<Member>, u32) {
        let mut rig = FakeRig::new();
        rig.lineup.reference = Some(speakers[0].1.to_owned());
        rig.lineup.members = speakers
            .iter()
            .map(|&(class, id, offset, after)| member(class, id, offset, after + 450.0))
            .collect();
        rig.lineup.hold_backs_ms = speakers
            .iter()
            .filter(|speaker| speaker.0 == Class::Local)
            .map(|&(_, id, offset, _)| (id.to_owned(), 2000.0 + offset.unwrap_or(0.0)))
            .collect();
        rig.heard_after_ms = speakers
            .iter()
            .map(|&(class, id, _, after)| (member_key(class, id), vec![after]))
            .collect();
        let mut run = start(&mut rig);
        let finished = run_to_end(&mut rig, &mut run, |_, _| {});
        let sheet = finished.sheet.expect("a sheet");
        (sheet, run.members().to_vec(), run.rate())
    }

    fn start(rig: &mut FakeRig) -> AllRun {
        let now = rig.now.get();
        AllRun::start(rig, &BTreeSet::new(), now, DebugLog::default()).expect("it starts")
    }

    fn measured(sheet: &Sheet, row: usize) -> f64 {
        match sheet.rows()[row].state {
            RowState::Certain(m) => m.new_ms,
            other => panic!("row {row} isn't certain: {other:?}"),
        }
    }

    fn given_back(rig: &FakeRig) {
        assert_eq!(rig.solo, None, "every speaker is heard again");
        assert_eq!(rig.tap.route(), Route::Ring);
        // The music fades back in over the next blocks.
        for _ in 0..10 {
            rig.capture_block();
        }
        assert!(!rig.tap.is_active(), "the music is back");
    }

    #[test]
    fn each_speaker_is_heard_alone_on_its_own_path_and_the_sheet_has_every_class() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let finished = run_to_end(&mut rig, &mut run, |_, _| {});
        assert_eq!(finished.failure, None);
        let sheet = finished.sheet.expect("a sheet");
        assert_eq!(sheet.ended(), None);
        assert_eq!(sheet.rows()[0].state, RowState::Reference);
        // Heard later than the reference: the USB speaker by 170 ms, so its
        // offset drops by that; the AirPlay speaker by 40 ms and the Cast
        // speaker by 1090 ms, so the others wait that much longer for them.
        assert!((measured(&sheet, 1) - -20.0).abs() <= 2.0);
        assert!((measured(&sheet, 2) - 2040.0).abs() <= 2.0);
        assert!((measured(&sheet, 3) - 4090.0).abs() <= 2.0);
        let airplay = sheet.airplay().expect("an AirPlay delay");
        assert_eq!(airplay.state, SharedState::Certain);
        assert!((airplay.new_ms.unwrap() - 2040.0).abs() <= 2.0);
        assert_eq!(
            rig.solos,
            [
                "local:builtin",
                "local:usb",
                "local:usb",
                "airplay:kitchen",
                "airplay:kitchen",
                "cast:den",
                "cast:den"
            ]
        );
        given_back(&rig);
    }

    /// A zone of the AirPlay and USB speakers, plus an attic speaker that is
    /// off; the reference and the Cast speaker play outside it.
    fn kitchen_zone() -> ZoneScope {
        let members: Vec<String> = ["ap:kitchen", "lc:usb", "lc:attic"]
            .map(str::to_owned)
            .into();
        ZoneScope {
            present: members.iter().cloned().collect(),
            members,
            off: vec![member(Class::Local, "attic", Some(40.0), 0.0)],
        }
    }

    fn start_zone(rig: &mut FakeRig, zone: &ZoneScope) -> Result<AllRun, Failure> {
        let now = rig.now.get();
        AllRun::start_zone(rig, &BTreeSet::new(), zone, now, DebugLog::default())
    }

    /// A Calibrate Zone run that has just started, for the adapter's tests.
    pub(crate) fn zone_run() -> AllRun {
        start_zone(&mut FakeRig::new(), &kitchen_zone()).expect("it starts")
    }

    // covers: S255
    #[test]
    fn a_zone_run_sweeps_its_members_in_zone_order_and_keeps_the_rest_silenced() {
        let mut rig = FakeRig::new();
        let mut run = start_zone(&mut rig, &kitchen_zone()).expect("it starts");
        let finished = run_to_end(&mut rig, &mut run, |rig, _| {
            assert_ne!(
                rig.solo.as_ref().map(Member::key).as_deref(),
                Some("cast:den")
            );
        });
        assert_eq!(finished.failure, None);
        let sheet = finished.sheet.expect("a sheet");
        assert_eq!(sheet.ended(), None);
        let rows: Vec<&str> = sheet.rows().iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            rows,
            [
                "local:builtin",
                "airplay:kitchen",
                "local:usb",
                "local:attic"
            ]
        );
        assert!((measured(&sheet, 1) - 2040.0).abs() <= 2.0);
        assert!((measured(&sheet, 2) - -20.0).abs() <= 2.0);
        assert!(sheet.is_off(3), "the attic speaker is listed, never swept");
        assert_eq!(sheet.rows()[3].state, RowState::NotMeasured);
        assert_eq!(
            rig.solos,
            [
                "local:builtin",
                "airplay:kitchen",
                "airplay:kitchen",
                "local:usb",
                "local:usb"
            ],
            "the Cast speaker outside the zone is never heard alone"
        );
        given_back(&rig);
    }

    // covers: S255, S246
    #[test]
    fn a_zone_run_needs_a_member_that_is_on_and_present() {
        let mut rig = FakeRig::new();
        let mut zone = kitchen_zone();
        zone.members.retain(|key| key == "lc:attic");
        assert_eq!(
            start_zone(&mut rig, &zone).err(),
            Some(Failure::NothingToMeasure)
        );
        // A hidden member plays on as a speaker outside the zone.
        let mut zone = kitchen_zone();
        zone.present.remove("lc:usb");
        let mut run = start_zone(&mut rig, &zone).expect("it starts");
        let finished = run_to_end(&mut rig, &mut run, |_, _| {});
        let sheet = finished.sheet.expect("a sheet");
        let rows: Vec<&str> = sheet.rows().iter().map(|row| row.id.as_str()).collect();
        assert_eq!(rows, ["local:builtin", "airplay:kitchen", "local:attic"]);
        assert!(!rig.solos.iter().any(|key| key == "local:usb"));
    }

    // covers: S240, S255
    #[test]
    fn a_silenced_speaker_going_or_an_off_member_turning_on_ends_a_zone_run() {
        for meddle in [
            |rig: &mut FakeRig| rig.remove("cast:den"),
            |rig: &mut FakeRig| {
                let attic = member(Class::Local, "attic", Some(40.0), 2540.0);
                rig.lineup.members.push(attic);
            },
        ] {
            let mut rig = FakeRig::new();
            let mut run = start_zone(&mut rig, &kitchen_zone()).expect("it starts");
            let mut ticks = 0;
            let finished = run_to_end(&mut rig, &mut run, |rig, _| {
                ticks += 1;
                if ticks == 50 {
                    meddle(rig);
                }
            });
            let sheet = finished.sheet.expect("a sheet");
            assert_eq!(sheet.ended(), Some(EndReason::SpeakersChanged));
            given_back(&rig);
        }
    }

    #[test]
    fn a_cast_speaker_without_an_offset_is_searched_for_first() {
        let mut rig = FakeRig::new();
        rig.lineup.members.truncate(1);
        rig.lineup
            .members
            .push(member(Class::Cast, "den", None, 3000.0));
        let mut run = start(&mut rig);
        let mut searched = false;
        let finished = run_to_end(&mut rig, &mut run, |_, run| {
            searched |= matches!(
                run.progress(),
                Progress::Measuring {
                    row: 1,
                    take: Take::Sweep(1),
                    searching: true
                }
            );
        });
        assert!(searched, "the progress says it searches");
        let sheet = finished.sheet.expect("a sheet");
        assert!((measured(&sheet, 1) - 1090.0).abs() <= 2.0);
    }

    #[test]
    fn a_speaker_turned_off_mid_run_ends_the_sheet_and_nothing_can_be_applied() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let finished = run_to_end(&mut rig, &mut run, |rig, run| {
            if let Progress::Measuring { row: 2, .. } = run.progress() {
                rig.remove("airplay:kitchen");
            }
        });
        let mut sheet = finished.sheet.expect("a sheet");
        assert_eq!(sheet.ended(), Some(EndReason::SpeakerTurnedOff));
        assert!(
            sheet.rows()[1].state.is_certain(),
            "rows measured before stay"
        );
        assert_eq!(sheet.rows()[3].state, RowState::NotMeasured);
        assert_eq!(
            sheet.apply(),
            Err(ApplyRefusal::Ended(EndReason::SpeakerTurnedOff))
        );
        given_back(&rig);
    }

    #[test]
    fn a_source_switch_a_new_speaker_or_a_delay_edit_ends_the_run_with_its_reason() {
        let switch: fn(&mut FakeRig) = |rig| rig.lineup.session = Some(2);
        let turn_on: fn(&mut FakeRig) = |rig| {
            rig.lineup
                .members
                .push(member(Class::Local, "hdmi", Some(0.0), 2500.0));
        };
        let edit: fn(&mut FakeRig) = |rig| rig.lineup.members[1].offset_ms = Some(400.0);
        for (meddle, reason) in [
            (switch, EndReason::SourceChanged),
            (turn_on, EndReason::SpeakersChanged),
            (edit, EndReason::SpeakersChanged),
        ] {
            let mut rig = FakeRig::new();
            let mut run = start(&mut rig);
            let finished = run_to_end(&mut rig, &mut run, |rig, run| {
                if let Progress::Measuring { row: 1, .. } = run.progress() {
                    meddle(rig);
                }
            });
            assert_eq!(finished.sheet.unwrap().ended(), Some(reason));
            given_back(&rig);
        }
    }

    #[test]
    fn cancel_fades_the_sweep_out_and_gives_every_speaker_back() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let mut cut_off = false;
        let finished = run_to_end(&mut rig, &mut run, |rig, run| {
            if rig.tap.played_at_ns().is_some() && run.is_running() {
                let now = rig.now.get();
                run.cancel(rig, now);
                cut_off = run.holds_outputs() && rig.solo.is_some();
            }
        });
        assert!(cut_off, "the sweep on its way plays out on its speaker");
        let sheet = finished.sheet.expect("a sheet");
        assert_eq!(sheet.ended(), Some(EndReason::Cancelled));
        assert!(sheet.rows()[1..].iter().all(|r| !r.state.is_certain()));
        given_back(&rig);
    }

    /// Two AirPlay speakers on the one AirPlay feed, the patio's 2 s later
    /// than the kitchen's.
    fn two_airplay_rig() -> FakeRig {
        let mut rig = FakeRig::new();
        rig.lineup.members = vec![
            member(Class::Local, "builtin", Some(0.0), 2500.0),
            member(Class::AirPlay, "kitchen", Some(2000.0), 2500.0),
            member(Class::AirPlay, "patio", Some(2000.0), 4500.0),
        ];
        rig.lineup.hold_backs_ms = BTreeMap::from([("builtin".to_owned(), 2000.0)]);
        rig.heard_after_ms
            .insert("airplay:patio".to_owned(), vec![4050.0]);
        rig
    }

    /// Whether `key`'s speaker is heard at `at_ns`, from the solo history.
    fn audible(rig: &FakeRig, key: &str, at_ns: u64) -> bool {
        rig.solo_times
            .iter()
            .rev()
            .find(|(since, _)| *since <= at_ns)
            .is_none_or(|(_, solo)| solo.as_deref().is_none_or(|solo| solo == key))
    }

    /// The AirPlay sweeps that entered the feed, by speaker. A new sweep's
    /// speaker is the one soloed when it starts playing.
    fn note_airplay_sweep(rig: &FakeRig, sweeps: &mut Vec<(String, u64)>, seen: &mut Vec<u64>) {
        let Some(played) = rig.tap.played_at_ns() else {
            return;
        };
        if seen.contains(&played) {
            return;
        }
        seen.push(played);
        if let Some(solo) = rig.solo.as_ref().filter(|m| m.class == Class::AirPlay) {
            sweeps.push((solo.key(), played));
        }
    }

    /// Every AirPlay speaker but the one measured is quiet while it plays
    /// another's sweep from the feed.
    fn assert_no_stray_airplay_sweep(rig: &FakeRig, sweeps: &[(String, u64)]) {
        let signal_ns = ns(calibrate::SIGNAL_SECONDS);
        for (measured, played) in sweeps {
            for (other, after_ms) in [("airplay:kitchen", 2050.0), ("airplay:patio", 4050.0)] {
                if other == measured {
                    continue;
                }
                let from = played + ns(after_ms / 1000.0);
                let to = from + signal_ns;
                let changes = rig
                    .solo_times
                    .iter()
                    .map(|(since, _)| *since)
                    .filter(|since| (from..=to).contains(since));
                for at in std::iter::once(from).chain(changes) {
                    assert!(
                        !audible(rig, other, at),
                        "{other} plays the sweep of {measured} at {at}: {:?}",
                        rig.solo_times
                    );
                }
            }
        }
    }

    #[test]
    fn an_airplay_speaker_later_than_another_stays_muted_until_their_sweeps_played_out() {
        let mut rig = two_airplay_rig();
        let mut run = start(&mut rig);
        let (mut sweeps, mut seen) = (Vec::new(), Vec::new());
        let finished = run_to_end(&mut rig, &mut run, |rig, _| {
            note_airplay_sweep(rig, &mut sweeps, &mut seen);
        });
        assert_eq!(finished.failure, None);
        assert_eq!(sweeps.len(), 4, "two sweeps each: {sweeps:?}");
        assert_no_stray_airplay_sweep(&rig, &sweeps);
        let sheet = finished.sheet.expect("a sheet");
        assert!(sheet.rows()[1..].iter().all(|row| row.state.is_certain()));
        given_back(&rig);

        // Cut off in the kitchen's window: the patio still plays it later.
        let mut rig = two_airplay_rig();
        let mut run = start(&mut rig);
        let (mut sweeps, mut seen) = (Vec::new(), Vec::new());
        run_to_end(&mut rig, &mut run, |rig, run| {
            note_airplay_sweep(rig, &mut sweeps, &mut seen);
            if !sweeps.is_empty() && run.is_running() {
                let now = rig.now.get();
                run.cancel(rig, now);
            }
        });
        assert_eq!(sweeps.len(), 1);
        assert_no_stray_airplay_sweep(&rig, &sweeps);
        given_back(&rig);
    }

    #[test]
    fn a_microphone_that_changes_between_sweeps_stops_the_run() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let finished = run_to_end(&mut rig, &mut run, |rig, run| {
            if let Progress::Measuring { row: 1, .. } = run.progress() {
                rig.mic = Ok(MicrophoneId {
                    device: "headset".to_owned(),
                    rate: MIC_RATE,
                });
            }
        });
        assert_eq!(finished.failure, Some(Failure::MicrophoneChanged));
        assert_eq!(finished.sheet.unwrap().ended(), Some(EndReason::Cancelled));
        given_back(&rig);
    }

    #[test]
    fn a_run_that_cant_start_says_why_and_leaves_the_music_alone() {
        let now = 1_000_000_000;
        let mut rig = FakeRig::new();
        rig.mic = Err(MicrophoneError::Denied);
        assert_eq!(
            AllRun::start(&mut rig, &BTreeSet::new(), now, DebugLog::default()).err(),
            Some(Failure::MicrophoneDenied)
        );
        let mut rig = FakeRig::new();
        rig.lineup.members.truncate(1);
        assert_eq!(
            AllRun::start(&mut rig, &BTreeSet::new(), now, DebugLog::default()).err(),
            Some(Failure::NothingToMeasure)
        );
        let mut rig = FakeRig::new();
        rig.lineup.session = None;
        assert_eq!(
            AllRun::start(&mut rig, &BTreeSet::new(), now, DebugLog::default()).err(),
            Some(Failure::NotPlaying(Missing::STREAMING))
        );
        assert!(!rig.tap.is_active());
        // Its recording wouldn't fit the estimate's limit at this rate.
        let mut rig = FakeRig::new();
        rig.lineup.members[3].latency_ms = 1e9;
        assert_eq!(
            AllRun::start(&mut rig, &BTreeSet::new(), now, DebugLog::default()).err(),
            Some(Failure::TooLong(None))
        );
        assert!(!rig.tap.is_active());
    }

    #[test]
    fn an_unplugged_microphone_ends_the_run_at_once() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let mut unplugged_at = None;
        let mut ended_at = None;
        let finished = run_to_end(&mut rig, &mut run, |rig, run| {
            let first = matches!(run.progress(), Progress::Measuring { row: 1, .. });
            if first && rig.tap.played_at_ns().is_some() && unplugged_at.is_none() {
                unplugged_at = Some(rig.now.get());
                rig.mic_fault.unplug(rig.now.get());
            }
            if ended_at.is_none() && unplugged_at.is_some() && !run.is_running() {
                ended_at = Some(rig.now.get());
            }
        });
        assert_eq!(finished.failure, Some(Failure::NoMicrophone));
        assert_eq!(finished.sheet.unwrap().ended(), Some(EndReason::Cancelled));
        let took = ended_at.unwrap_or_else(|| rig.now.get()) - unplugged_at.expect("unplugged");
        assert!(took <= TICK_NS, "ended {took} ns after the unplug");
        given_back(&rig);
    }

    #[test]
    fn a_microphone_that_stalls_is_waited_for_until_the_deadline() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let mut stalled_at = None;
        let mut estimated_at = None;
        let finished = run_to_end(&mut rig, &mut run, |rig, run| {
            if rig.tap.played_at_ns().is_some() && stalled_at.is_none() {
                stalled_at = Some(rig.now.get());
                rig.mic_fault.frozen_at.set(Some(rig.now.get()));
            }
            if estimated_at.is_none() && matches!(run.progress(), Progress::Estimating { .. }) {
                estimated_at = Some(rig.now.get());
            }
        });
        assert_eq!(finished.failure, None);
        let sheet = finished.sheet.expect("a sheet");
        assert!(
            sheet.rows()[1..]
                .iter()
                .all(|r| matches!(r.state, RowState::Refused(refused) if refused.reason == calibrate::Refusal::TooShort))
        );
        let waited = estimated_at.expect("it estimated") - stalled_at.expect("it stalled");
        assert!(waited >= ns(STALL_S), "gave up after {waited} ns");
        given_back(&rig);
    }

    #[test]
    fn a_microphone_that_never_delivers_a_frame_gives_up_after_the_warm_up_limit() {
        let mut rig = FakeRig::new();
        rig.mic_fault.cold.set(true);
        let started = rig.now.get();
        let mut run = start(&mut rig);
        let mut ended_at = None;
        let finished = run_to_end(&mut rig, &mut run, |rig, run| {
            if ended_at.is_none() && !run.is_running() {
                ended_at = Some(rig.now.get());
            }
        });
        assert_eq!(finished.failure, Some(Failure::NoMicrophone));
        assert_eq!(rig.tap.played_at_ns(), None, "no sweep played");
        assert!(ended_at.unwrap_or_else(|| rig.now.get()) - started >= ns(WARM_UP_LIMIT_S));
        given_back(&rig);
    }

    #[test]
    fn a_source_that_sends_nothing_fails_as_silent() {
        let mut rig = FakeRig::new();
        rig.capture_off = true;
        let mut run = start(&mut rig);
        let finished = run_to_end(&mut rig, &mut run, |_, _| {});
        assert_eq!(finished.failure, Some(Failure::SourceSilent));
        assert_eq!(finished.sheet.unwrap().ended(), Some(EndReason::Cancelled));
        assert_eq!(rig.solo, None);
    }

    #[test]
    fn cancel_while_estimating_ends_the_run_once_the_estimate_is_back() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let mut asked = None;
        let finished = run_to_end(&mut rig, &mut run, |rig, run| {
            if asked.is_none() && matches!(run.progress(), Progress::Estimating { row: 1, .. }) {
                let now = rig.now.get();
                run.cancel(rig, now);
                asked = Some(run.progress());
            }
        });
        assert!(
            matches!(asked, Some(Progress::Estimating { .. })),
            "the estimate runs on: {asked:?}"
        );
        assert_eq!(finished.failure, None);
        let mut sheet = finished.sheet.expect("a sheet");
        assert_eq!(sheet.ended(), Some(EndReason::Cancelled));
        assert_eq!(
            sheet.apply().err(),
            Some(ApplyRefusal::Ended(EndReason::Cancelled))
        );
        given_back(&rig);
    }

    #[test]
    fn measure_again_replaces_its_row_alone() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let members = run.members().to_vec();
        let mut sheet = run_to_end(&mut rig, &mut run, |_, _| {})
            .sheet
            .expect("a sheet");
        let cast_before = measured(&sheet, 3);
        rig.heard_after_ms
            .insert("local:usb".to_owned(), vec![2200.0]);
        let now = rig.now.get();
        let mut again = AllRun::again(
            &mut rig,
            &sheet,
            &members,
            MIC_RATE,
            1,
            now,
            DebugLog::default(),
        )
        .expect("it starts");
        rig.solos.clear();
        let finished = run_to_end(&mut rig, &mut again, |_, _| {});
        assert_eq!(rig.solos, ["local:builtin", "local:usb", "local:usb"]);
        assert!(sheet.replace(finished.sheet.expect("a sheet")));
        assert!((measured(&sheet, 1) - -40.0).abs() <= 2.0);
        assert_eq!(measured(&sheet, 3), cast_before);
        given_back(&rig);
    }

    #[test]
    fn measure_again_of_a_speaker_turned_off_since_is_refused() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let members = run.members().to_vec();
        let sheet = run_to_end(&mut rig, &mut run, |_, _| {})
            .sheet
            .expect("a sheet");
        rig.remove("cast:den");
        let now = rig.now.get();
        let again = AllRun::again(
            &mut rig,
            &sheet,
            &members,
            MIC_RATE,
            3,
            now,
            DebugLog::default(),
        );
        assert_eq!(again.err(), Some(Failure::NotPlaying(Missing::SPEAKER)));
        given_back(&rig);
    }

    #[test]
    fn measure_again_while_stopped_names_the_speaker_only_when_its_switch_is_off() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let members = run.members().to_vec();
        let sheet = run_to_end(&mut rig, &mut run, |_, _| {})
            .sheet
            .expect("a sheet");
        rig.lineup.session = None;
        rig.lineup.members.clear();
        let refusal = |rig: &mut FakeRig| {
            let now = rig.now.get();
            AllRun::again(rig, &sheet, &members, MIC_RATE, 1, now, DebugLog::default()).err()
        };
        assert_eq!(
            refusal(&mut rig),
            Some(Failure::NotPlaying(Missing::STREAMING))
        );
        rig.lineup
            .switched_off
            .push((members[1].class, members[1].id.clone()));
        assert_eq!(
            refusal(&mut rig),
            Some(Failure::NotPlaying(Missing {
                speaker: true,
                ..Missing::STREAMING
            }))
        );
        given_back(&rig);
    }

    #[test]
    fn the_lineup_measures_a_playing_cast_group_under_its_id_and_leaves_one_off_out() {
        use audiosync_cast::discovery::DeviceId;
        use audiosync_cast::host::{AttemptId, HostEvent};
        use audiosync_cast::session::SessionEvent;

        use crate::audio::backend::SourceId;
        use crate::audio::cast::tests::{ATTIC, FakeCast, LIVING};
        use crate::audio::mock::MockBackend;

        let mut backend = MockBackend::demo();
        let cast = FakeCast::default();
        cast.announce(ATTIC, "Attic", 32_123);
        cast.announce(LIVING, "Living Room", 32_124);
        backend.set_cast(cast.link());
        backend.poll();
        assert!(
            backend
                .cast_speakers()
                .iter()
                .all(|speaker| speaker.group.is_some()),
            "both are groups"
        );
        backend.set_output_enabled("speakers", true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        let casts = |backend: &MockBackend| -> Vec<(String, Option<f64>)> {
            lineup(backend)
                .members
                .into_iter()
                .filter(|member| member.class == Class::Cast)
                .map(|member| (member.id, member.offset_ms))
                .collect()
        };
        assert_eq!(casts(&backend), [], "both groups are off");

        backend.set_cast_offset(ATTIC, 2400.0);
        backend.set_cast_enabled(ATTIC, true).unwrap();
        assert_eq!(casts(&backend), [], "Attic still connects");
        cast.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: DeviceId::parse(ATTIC).unwrap(),
            event: SessionEvent::Playing,
        });
        backend.poll();
        assert_eq!(lineup(&backend).members[0].id, "speakers");
        assert_eq!(
            casts(&backend),
            [(ATTIC.to_owned(), Some(2400.0))],
            "Attic is measured as one speaker, Living Room is left out"
        );
    }

    // covers: C77a
    #[test]
    fn only_a_cast_speaker_without_an_offset_waits_for_the_unknown_start_guess() {
        use audiosync_cast::audio::Codec;
        use audiosync_cast::discovery::DeviceId;
        use audiosync_cast::host::{AttemptId, HostEvent};
        use audiosync_cast::session::SessionEvent;
        use audiosync_core::calibrate_all::{
            CAST_LATENCY_ALLOWANCE_MS, CAST_UNKNOWN_FLAC_MS, CAST_UNKNOWN_WAV_MS,
        };

        use crate::audio::backend::SourceId;
        use crate::audio::cast::tests::{ATTIC, FakeCast};
        use crate::audio::mock::MockBackend;

        let mut backend = MockBackend::demo();
        let cast = FakeCast::default();
        cast.announce(ATTIC, "Attic", 32_123);
        backend.set_cast(cast.link());
        backend.poll();
        backend.set_output_enabled("speakers", true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.set_cast_offset(ATTIC, 2400.0);
        backend.set_cast_enabled(ATTIC, true).unwrap();
        cast.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: DeviceId::parse(ATTIC).unwrap(),
            event: SessionEvent::Playing,
        });
        backend.poll();
        let wait = |backend: &MockBackend| {
            let speaker = backend.cast_speakers().into_iter().next().unwrap();
            let guess_ms = match speaker.format {
                Some(Codec::Wav) => CAST_UNKNOWN_WAV_MS,
                _ => CAST_UNKNOWN_FLAC_MS,
            };
            let latency_ms = lineup(backend)
                .members
                .into_iter()
                .find(|member| member.class == Class::Cast)
                .map(|member| member.latency_ms);
            (latency_ms, speaker.hold_back_ms, guess_ms)
        };

        let (latency_ms, hold_back_ms, _) = wait(&backend);
        assert_eq!(
            latency_ms,
            Some(2400.0 + hold_back_ms + CAST_LATENCY_ALLOWANCE_MS),
            "a calibrated speaker waits for its offset, without the guess on top"
        );
        backend.set_cast_offset(ATTIC, 0.0);
        let (latency_ms, hold_back_ms, guess_ms) = wait(&backend);
        assert_eq!(
            latency_ms,
            Some(hold_back_ms + guess_ms),
            "an uncalibrated speaker waits for the guess"
        );
    }

    /// Turns on a local output whose device reports `form_factor` and
    /// `enumerator`, heard 2100 ms after its sweep entered its path.
    fn add_device(rig: &mut FakeRig, id: &str, form_factor: u32, enumerator: &str) {
        let at = rig
            .lineup
            .members
            .iter()
            .position(|m| m.class != Class::Local)
            .unwrap_or(rig.lineup.members.len());
        rig.lineup
            .members
            .insert(at, member(Class::Local, id, Some(100.0), 2600.0));
        rig.lineup.hold_backs_ms.insert(id.to_owned(), 2100.0);
        rig.lineup.devices.insert(
            id.to_owned(),
            LocalDevice {
                name: format!("Output {id}"),
                traits: DeviceTraits {
                    form_factor: Some(form_factor),
                    enumerator: Some(enumerator.to_owned()),
                },
            },
        );
        rig.heard_after_ms
            .insert(member_key(Class::Local, id), vec![2100.0]);
    }

    #[test]
    fn headphones_and_virtual_outputs_are_listed_left_out_and_never_swept() {
        let mut rig = FakeRig::new();
        add_device(&mut rig, "phones", 3, "HDAUDIO");
        add_device(&mut rig, "cable", 1, "ROOT");
        add_device(&mut rig, "bt-speaker", 1, "BTHENUM");
        let (planned, left_out) = rig.lineup.plan(&BTreeSet::new());
        assert_eq!(
            left_out,
            vec![
                LeftOut {
                    id: "phones".to_owned(),
                    kind: MicKind::Headphones,
                    included: false,
                },
                LeftOut {
                    id: "cable".to_owned(),
                    kind: MicKind::Virtual,
                    included: false,
                },
            ]
        );
        let keys: Vec<String> = planned.members.iter().map(Member::key).collect();
        assert_eq!(
            keys,
            [
                "local:builtin",
                "local:usb",
                "local:bt-speaker",
                "airplay:kitchen",
                "cast:den"
            ]
        );
        let mut run = start(&mut rig);
        let finished = run_to_end(&mut rig, &mut run, |_, _| {});
        assert_eq!(finished.failure, None);
        let sheet = finished.sheet.expect("a sheet");
        assert_eq!(sheet.ended(), None, "an output left out changes nothing");
        assert_eq!(sheet.rows().len(), 5);
        assert!(
            run.members()
                .iter()
                .all(|m| m.id != "phones" && m.id != "cable")
        );
        assert!(
            !rig.solos
                .iter()
                .any(|key| key == "local:phones" || key == "local:cable"),
            "no sweep on an output left out: {:?}",
            rig.solos
        );
        assert!(rig.solos.iter().any(|key| key == "local:bt-speaker"));
    }

    #[test]
    fn an_output_included_anyway_is_measured_as_a_speaker() {
        let mut rig = FakeRig::new();
        add_device(&mut rig, "phones", 5, "USB");
        let included = BTreeSet::from(["phones".to_owned()]);
        let (_, left_out) = rig.lineup.plan(&included);
        assert_eq!(
            left_out,
            vec![LeftOut {
                id: "phones".to_owned(),
                kind: MicKind::Headphones,
                included: true,
            }]
        );
        let now = rig.now.get();
        let mut run = AllRun::start(&mut rig, &included, now, DebugLog::default()).expect("starts");
        let finished = run_to_end(&mut rig, &mut run, |_, _| {});
        let sheet = finished.sheet.expect("a sheet");
        let row = run
            .members()
            .iter()
            .position(|m| m.id == "phones")
            .expect("its row");
        assert!(sheet.rows()[row].state.is_certain());
        assert!(rig.solos.iter().any(|key| key == "local:phones"));
    }

    #[test]
    fn only_left_out_outputs_besides_the_reference_block_with_their_own_reason() {
        let mut rig = FakeRig::new();
        rig.lineup.members.truncate(1);
        add_device(&mut rig, "phones", 3, "BTHENUM");
        let none = BTreeSet::new();
        assert_eq!(blocked(&rig.lineup, &none), Some(Failure::OnlyLeftOut));
        let now = rig.now.get();
        assert_eq!(
            AllRun::start(&mut rig, &none, now, DebugLog::default()).err(),
            Some(Failure::OnlyLeftOut)
        );
        assert!(!rig.tap.is_active());
        let included = BTreeSet::from(["phones".to_owned()]);
        assert_eq!(blocked(&rig.lineup, &included), None);
        rig.lineup.members.truncate(1);
        assert_eq!(
            blocked(&rig.lineup, &none),
            Some(Failure::NothingToMeasure),
            "with only the reference on, nothing is left out"
        );
    }
}
