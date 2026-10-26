//! Speaker presets: saved scenes of which speakers play and how loud, with
//! the equalizer curve (S251 to S254). Like the rest of [`crate::policy`],
//! these run in the host, which keeps the presets in its settings and carries
//! out a recall plan.
//!
//! Members are speaker keys as text (S149), the form the hosts store.

use super::speakers::{SpeakerKey, new_group_name, renamed_group};
use super::zones::{self, MemberKind, SavedGroup, Zones, zone_volume};
use crate::eq::{BAND_COUNT, quantize_gain_db};
use crate::volume::{DEFAULT_MASTER_POSITION, sanitize_own, sanitize_position};

/// One stored member: its speaker key and its own volume slider position,
/// 0 to 100. A fixed-volume Cast speaker (C68) has none.
#[derive(Clone, Debug, PartialEq)]
pub struct PresetMember {
    pub key: String,
    pub volume: Option<f64>,
}

/// A zone volume a preset stores, by zone ID (S245).
#[derive(Clone, Debug, PartialEq)]
pub struct PresetZone {
    pub id: String,
    pub volume: f64,
}

/// One preset (S251): the speakers on at save in list order, their own
/// volumes, the volume of each zone a member was in, the master position and
/// the ten equalizer gains. Never a preamp, the equalizer switch, a delay, an
/// offset or the AirPlay adjustment.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub members: Vec<PresetMember>,
    pub zones: Vec<PresetZone>,
    pub master: f64,
    pub gains_db: [f64; BAND_COUNT],
}

/// Whether a stored member key can stay in a preset: a speaker key with an ID.
fn usable(key: &str) -> Option<SpeakerKey> {
    SpeakerKey::parse(key).filter(|parsed| !parsed.id().is_empty())
}

/// Which of `keys` a preset keeps (S251): not a bad key, not one earlier in
/// the list, and not a Cast key after the first.
pub fn member_keeps<'a>(keys: impl IntoIterator<Item = Option<&'a str>>) -> Vec<bool> {
    let mut seen: Vec<&str> = Vec::new();
    let mut cast = false;
    keys.into_iter()
        .map(|key| {
            let Some(key) = key else { return false };
            let Some(parsed) = usable(key) else {
                return false;
            };
            if seen.contains(&key) {
                return false;
            }
            if matches!(parsed, SpeakerKey::Cast(_)) {
                if cast {
                    return false;
                }
                cast = true;
            }
            seen.push(key);
            true
        })
        .collect()
}

/// A stored own volume whose member isn't known: at least 0 and not capped,
/// since an AirPlay member's may pass 100 (S290); none when it isn't a
/// number. A local or Cast member is clamped where its volume is applied.
pub fn member_volume(stored: Option<f64>) -> Option<f64> {
    stored.filter(|volume| volume.is_finite()).map(sanitize_own)
}

/// Member `key`'s stored own volume: an AirPlay member's as
/// [`member_volume`] keeps it, any other clamped to 0 to 100.
fn member_volume_of(key: &str, stored: Option<f64>) -> Option<f64> {
    match usable(key) {
        Some(SpeakerKey::AirPlay(_)) => member_volume(stored),
        _ => stored.and_then(sanitize_position),
    }
}

/// A stored master position: clamped to 0 to 100, the default when it isn't
/// a number.
pub fn master_volume(stored: f64) -> f64 {
    sanitize_position(stored).unwrap_or(DEFAULT_MASTER_POSITION)
}

impl Preset {
    /// The preset as S251 keeps it on load: bad, repeated and second Cast
    /// member keys dropped, a repeated zone ID dropped, a local or Cast
    /// member's volume clamped to 0 to 100 (an AirPlay member's may pass
    /// 100, S290) and the gains on the controls' steps (S23).
    pub fn sanitized(self) -> Preset {
        let keeps = member_keeps(self.members.iter().map(|member| Some(member.key.as_str())));
        let members = self
            .members
            .into_iter()
            .zip(keeps)
            .filter(|(_, keep)| *keep)
            .map(|(member, _)| PresetMember {
                volume: member_volume_of(&member.key, member.volume),
                ..member
            })
            .collect();
        let mut zones: Vec<PresetZone> = Vec::with_capacity(self.zones.len());
        for zone in self.zones {
            if !zones.iter().any(|kept| kept.id == zone.id) {
                zones.push(PresetZone {
                    volume: zone_volume(zone.volume),
                    ..zone
                });
            }
        }
        Preset {
            members,
            zones,
            master: master_volume(self.master),
            gains_db: self.gains_db.map(quantize_gain_db),
            ..self
        }
    }

    /// The preset a save makes of `scene` (S251), with the zone volume of
    /// every zone a member is in now, in member order.
    pub fn capture(id: &str, name: &str, scene: &Scene<'_>) -> Preset {
        let members: Vec<PresetMember> = scene
            .on
            .iter()
            .map(|speaker| PresetMember {
                key: speaker.key.clone(),
                volume: speaker.volume,
            })
            .collect();
        let zones = zones_of(
            members.iter().map(|member| member.key.as_str()),
            scene.zones,
        );
        Preset {
            id: id.to_owned(),
            name: name.to_owned(),
            members,
            zones,
            master: scene.master,
            gains_db: scene.gains_db,
        }
        .sanitized()
    }

    /// Whether `scene` is this preset exactly (S253): see [`matches`].
    pub fn is_current(&self, scene: &Scene<'_>) -> bool {
        let members: Vec<MemberNow> = self
            .members
            .iter()
            .map(|member| {
                let now = scene.on.iter().find(|speaker| speaker.key == member.key);
                MemberNow {
                    on: now.is_some(),
                    stored_volume: member.volume,
                    volume: now.and_then(|speaker| speaker.volume),
                }
            })
            .collect();
        let zones: Vec<ZoneNow> = self
            .zones
            .iter()
            .map(|zone| ZoneNow {
                stored: zone.volume,
                volume: scene.zones.get(&zone.id).map(|now| now.volume),
            })
            .collect();
        matches(
            &members,
            scene.on.len(),
            &zones,
            Levels {
                master: self.master,
                gains_db: self.gains_db,
            },
            Levels {
                master: scene.master,
                gains_db: scene.gains_db,
            },
        )
    }

    /// A speaker's key changed from `from` to `to` (S204): `to` takes
    /// `from`'s place, unless the preset holds `to` already, which then
    /// keeps its place and `from` goes.
    pub fn rename_key(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        if self.members.iter().any(|member| member.key == to) {
            self.members.retain(|member| member.key != from);
            return;
        }
        for member in &mut self.members {
            if member.key == from {
                to.clone_into(&mut member.key);
            }
        }
    }
}

/// The zone volumes of the zones `keys` are in, one per zone, in key order.
fn zones_of<'a>(keys: impl IntoIterator<Item = &'a str>, zones: &Zones) -> Vec<PresetZone> {
    let mut out: Vec<PresetZone> = Vec::new();
    for key in keys {
        if let Some(zone) = zones.zone_of(key)
            && !out.iter().any(|kept| kept.id == zone.id)
        {
            out.push(PresetZone {
                id: zone.id.clone(),
                volume: zone.volume,
            });
        }
    }
    out
}

/// The presets as stored: each [`Preset::sanitized`], and a preset whose ID
/// an earlier one has dropped.
pub fn from_stored(presets: impl IntoIterator<Item = Preset>) -> Vec<Preset> {
    let mut out: Vec<Preset> = Vec::new();
    for preset in presets {
        if !out.iter().any(|kept| kept.id == preset.id) {
            out.push(preset.sanitized());
        }
    }
    out
}

/// A new preset's name (S254, S150): the typed name, trimmed; without one,
/// `default_name(n)` ("Preset n") with n one more than the number of
/// presets, counting up past names in use.
pub fn new_preset_name<S: AsRef<str>>(
    typed: &str,
    existing: &[S],
    default_name: impl Fn(usize) -> String,
) -> String {
    new_group_name(typed, existing, default_name)
}

/// A preset's name after a rename (S254, S150): a blank name keeps the old one.
pub fn renamed_preset(current: &str, typed: &str) -> String {
    renamed_group(current, typed)
}

/// One speaker on now, as a save and the current-preset check see it: its
/// key and its own volume, none for a fixed-volume Cast speaker.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerNow {
    pub key: String,
    pub volume: Option<f64>,
}

/// What the host plays now (S251, S253): the speakers on, in list order,
/// waiting AirPlay 2 speakers included and the reference output left out;
/// the zones; the master position and the ten gains.
#[derive(Clone, Copy, Debug)]
pub struct Scene<'a> {
    pub on: &'a [SpeakerNow],
    pub zones: &'a Zones,
    pub master: f64,
    pub gains_db: [f64; BAND_COUNT],
}

/// One stored member as the current-preset check sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MemberNow {
    /// The speaker is on now.
    pub on: bool,
    pub stored_volume: Option<f64>,
    /// Its own volume now; none for a fixed-volume Cast speaker.
    pub volume: Option<f64>,
}

/// One stored zone volume as the current-preset check sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ZoneNow {
    pub stored: f64,
    /// The zone's volume now; none once the zone is deleted.
    pub volume: Option<f64>,
}

/// The master position and the ten gains.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Levels {
    pub master: f64,
    pub gains_db: [f64; BAND_COUNT],
}

/// Whether the host plays a preset exactly (S253), with no tolerance: every
/// stored member is on and `on_now` speakers are on in all, so the on sets
/// are equal; every stored own volume is the speaker's now, unless either
/// side has none (a fixed-volume Cast speaker); every stored zone that still
/// exists has its volume; the master and the ten gains are equal. The preamp
/// and the equalizer switch are not part of it.
pub fn matches(
    members: &[MemberNow],
    on_now: usize,
    zones: &[ZoneNow],
    stored: Levels,
    now: Levels,
) -> bool {
    on_now == members.len()
        && members.iter().all(|member| {
            member.on
                && match (member.stored_volume, member.volume) {
                    (Some(stored), Some(now)) => stored == now,
                    _ => true,
                }
        })
        && zones
            .iter()
            .all(|zone| zone.volume.is_none_or(|now| now == zone.stored))
        && stored == now
}

/// The first of `presets` that `scene` is (S253): with two equal ones, the
/// first in list order.
pub fn current<'a>(presets: &'a [Preset], scene: &Scene<'_>) -> Option<&'a Preset> {
    presets.iter().find(|preset| preset.is_current(scene))
}

/// What the host knows of one stored member at recall, matched by its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecallMember {
    pub kind: MemberKind,
    /// The host lists a speaker with this key now, hidden or not.
    pub found: bool,
    /// Hidden with Hide Speaker.
    pub hidden: bool,
    /// Switched on.
    pub on: bool,
    /// A Cast speaker the host can't start now, such as one another app
    /// casts to.
    pub busy: bool,
    /// A stereo pair or home theater target while the AirPlay 2 switch is
    /// off (S246): not listed.
    pub needs_airplay2: bool,
    /// A speaker that plays as part of a stereo pair or home theater target
    /// now (S247).
    pub in_target: bool,
    /// A Cast speaker whose volume can't be changed from here (C68).
    pub fixed_volume: bool,
    /// The preset stores an own volume for it.
    pub has_volume: bool,
}

/// What the host knows of a speaker outside the preset at recall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecallOther {
    pub on: bool,
    /// An output the user can never switch off, the reference output: a
    /// recall leaves it alone.
    pub reference: bool,
}

/// Why a stored member stays off at recall; the notice names each (S252).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftOff {
    /// No speaker with its key is listed.
    NotFound,
    Hidden,
    /// "Needs AirPlay 2 (Experimental)".
    NeedsAirPlay2,
    /// "Plays as part of <target>".
    InTarget,
    /// A Cast speaker the host can't start now.
    Busy,
}

/// What a recall does with one stored member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecallStep {
    /// Switched on, or stays on.
    On,
    /// Switched on, but held back by P169 until the other AirPlay 2 speaker
    /// stops.
    Waiting,
    LeftOff(LeftOff),
    /// A Cast key after the first, which a sanitized preset never holds:
    /// left alone, and no notice names it.
    Ignored,
}

/// What a recall does as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecallAction {
    /// No stored member can go on: nothing changes, and the notice names
    /// every member (P1).
    Nothing,
    /// The plan is carried out, streaming starting first when
    /// `start_streaming`.
    Apply { start_streaming: bool },
}

/// A recall's plan (S252).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecallPlan {
    pub action: RecallAction,
    /// One step per stored member, in preset order.
    pub steps: Vec<RecallStep>,
    /// Per stored member: its own volume is set.
    pub set_volume: Vec<bool>,
    /// Per speaker outside the preset: it is switched off.
    pub switch_off: Vec<bool>,
}

impl RecallPlan {
    /// Whether the recall leaves a stored member off, so it posts its notice;
    /// a full recall clears it instead.
    pub fn needs_notice(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, RecallStep::LeftOff(_)))
    }
}

/// The plan of a preset recall (S252), a full scene. Every stored member
/// that is listed, not hidden, not a target that needs AirPlay 2, not
/// playing as part of a target and, for a Cast speaker, not busy goes on;
/// every other speaker that is on goes off, Cast included, but the
/// reference output; streaming starts when it is off. Every AirPlay 2 member
/// goes on, and all but the one P169 lets play (the one already playing,
/// else the first) wait. A member that goes on gets its stored own volume,
/// unless it is a fixed-volume Cast speaker. When no member can go on,
/// nothing changes (P1).
pub fn recall_plan(
    members: &[RecallMember],
    others: &[RecallOther],
    streaming: bool,
) -> RecallPlan {
    let first_cast = members
        .iter()
        .position(|member| member.kind == MemberKind::Cast);
    let mut steps: Vec<RecallStep> = members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            if member.kind == MemberKind::Cast && first_cast != Some(index) {
                RecallStep::Ignored
            } else if member.needs_airplay2 {
                RecallStep::LeftOff(LeftOff::NeedsAirPlay2)
            } else if !member.found {
                RecallStep::LeftOff(LeftOff::NotFound)
            } else if member.hidden {
                RecallStep::LeftOff(LeftOff::Hidden)
            } else if member.in_target {
                RecallStep::LeftOff(LeftOff::InTarget)
            } else if member.kind == MemberKind::Cast && member.busy {
                RecallStep::LeftOff(LeftOff::Busy)
            } else {
                RecallStep::On
            }
        })
        .collect();
    let airplay2: Vec<usize> = (0..members.len())
        .filter(|&index| {
            steps[index] == RecallStep::On && members[index].kind == MemberKind::AirPlay2
        })
        .collect();
    let plays = airplay2
        .iter()
        .find(|&&index| streaming && members[index].on)
        .or(airplay2.first())
        .copied();
    for index in airplay2 {
        if plays != Some(index) {
            steps[index] = RecallStep::Waiting;
        }
    }
    let goes_on = |step: &RecallStep| matches!(step, RecallStep::On | RecallStep::Waiting);
    if !steps.iter().any(goes_on) {
        return RecallPlan {
            action: RecallAction::Nothing,
            set_volume: vec![false; members.len()],
            switch_off: vec![false; others.len()],
            steps,
        };
    }
    let set_volume = members
        .iter()
        .zip(&steps)
        .map(|(member, step)| goes_on(step) && member.has_volume && !member.fixed_volume)
        .collect();
    RecallPlan {
        action: RecallAction::Apply {
            start_streaming: !streaming,
        },
        set_volume,
        switch_off: others
            .iter()
            .map(|other| other.on && !other.reference)
            .collect(),
        steps,
    }
}

/// The zone volumes a recall sets (S252): each stored zone that still
/// exists; a deleted one is skipped (P2).
pub fn recall_zone_volumes<'a>(preset: &'a Preset, zones: &Zones) -> Vec<(&'a str, f64)> {
    preset
        .zones
        .iter()
        .filter(|zone| zones.get(&zone.id).is_some())
        .map(|zone| (zone.id.as_str(), zone.volume))
        .collect()
}

/// What the migration reads besides the groups (S254): the zones now (after
/// S248), each member's saved own volume, the master and the gains.
pub struct Saved<'a> {
    pub zones: &'a Zones,
    pub volume: &'a dyn Fn(&str) -> Option<f64>,
    pub master: f64,
    pub gains_db: [f64; BAND_COUNT],
}

/// The presets the saved groups become (S254): S248 replayed from the
/// groups, and each group it left a member out of becomes a preset with the
/// group's ID and name, its full member list in group order (the first Cast
/// key only), each member's saved own volume, the volume of each zone a
/// member is in now, the master and the gains. A group that lost nothing
/// gives no preset. The same groups and settings always give the same
/// presets.
pub fn presets_from_groups(groups: &[SavedGroup<'_>], saved: &Saved<'_>) -> Vec<Preset> {
    let kept = zones::zones_from_groups(groups.iter().copied());
    groups
        .iter()
        .filter(|group| {
            let zone = kept.get(group.id);
            group
                .members
                .iter()
                .any(|key| zone.is_none_or(|zone| !zone.members.contains(key)))
        })
        .map(|group| {
            let members: Vec<PresetMember> = group
                .members
                .iter()
                .map(|key| PresetMember {
                    key: key.clone(),
                    volume: (saved.volume)(key),
                })
                .collect();
            Preset {
                id: group.id.to_owned(),
                name: group.name.to_owned(),
                zones: zones_of(group.members.iter().map(String::as_str), saved.zones),
                members,
                master: saved.master,
                gains_db: saved.gains_db,
            }
            .sanitized()
        })
        .collect()
}

/// The one-time migration (S254): the presets to add before the mark, or
/// `None` once the mark is set, so later launches change nothing.
pub fn migrate(marked: bool, groups: &[SavedGroup<'_>], saved: &Saved<'_>) -> Option<Vec<Preset>> {
    (!marked).then(|| presets_from_groups(groups, saved))
}
