//! Zones: named sets of speakers with a volume of their own, each speaker in
//! at most one (S150, S245 to S248). Like the rest of [`crate::policy`], these
//! run in the host, which keeps the zones in its settings and switches the
//! speakers a click plan names.
//!
//! Members are speaker keys as text (S149), the form the hosts store, so a
//! key this build can't parse still stays in its zone.

use std::collections::BTreeMap;

use super::speakers::{new_group_name, renamed_group};

/// A new zone's volume, and a migrated group's (S245, S248).
pub const DEFAULT_ZONE_VOLUME: f64 = 100.0;

/// One zone (S245): its ID, its name (S150), its members' speaker keys in
/// order, its volume slider position, 0 to 100, and the name each member
/// was last listed under (S286), by key.
#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    pub volume: f64,
    pub member_names: BTreeMap<String, String>,
}

/// A zone volume as stored: clamped to 0 to 100; one that isn't a number is
/// [`DEFAULT_ZONE_VOLUME`].
pub fn zone_volume(stored: f64) -> f64 {
    if stored.is_finite() {
        stored.clamp(0.0, 100.0)
    } else {
        DEFAULT_ZONE_VOLUME
    }
}

/// Every zone, in order, with each speaker key in at most one of them (S245).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Zones {
    list: Vec<Zone>,
}

impl Zones {
    /// The zones as stored, made to follow S245: a key already in an earlier
    /// zone, or earlier in the same one, is left out; a zone whose ID an
    /// earlier zone has is dropped; volumes go through [`zone_volume`]; a
    /// name kept for a key that isn't a member, or a blank one, is dropped.
    /// An empty zone stays.
    pub fn from_stored(zones: impl IntoIterator<Item = Zone>) -> Zones {
        let mut out = Zones::default();
        for zone in zones {
            if out.get(&zone.id).is_some() {
                continue;
            }
            let mut members: Vec<String> = Vec::with_capacity(zone.members.len());
            for key in zone.members {
                if out.zone_of(&key).is_none() && !members.contains(&key) {
                    members.push(key);
                }
            }
            let mut member_names = zone.member_names;
            member_names.retain(|key, name| members.contains(key) && !name.trim().is_empty());
            out.list.push(Zone {
                members,
                volume: zone_volume(zone.volume),
                member_names,
                ..zone
            });
        }
        out
    }

    pub fn list(&self) -> &[Zone] {
        &self.list
    }

    pub fn into_list(self) -> Vec<Zone> {
        self.list
    }

    pub fn get(&self, id: &str) -> Option<&Zone> {
        self.list.iter().find(|zone| zone.id == id)
    }

    /// The zone that holds `key`, if one does.
    pub fn zone_of(&self, key: &str) -> Option<&Zone> {
        self.list
            .iter()
            .find(|zone| zone.members.iter().any(|member| member == key))
    }

    /// The name member `key` was last listed under (S286), kept by the zone
    /// that holds it.
    pub fn member_name(&self, key: &str) -> Option<&str> {
        self.zone_of(key)?.member_names.get(key).map(String::as_str)
    }

    /// Keeps `name` as the one member `key` was last listed under (S286).
    /// A blank name, and a key in no zone, change nothing. True when the
    /// kept name changed, so the host saves it.
    pub fn remember_name(&mut self, key: &str, name: &str) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        let Some(zone) = self
            .list
            .iter_mut()
            .find(|zone| zone.members.iter().any(|member| member == key))
        else {
            return false;
        };
        if zone.member_names.get(key).is_some_and(|kept| kept == name) {
            return false;
        }
        zone.member_names.insert(key.to_owned(), name.to_owned());
        true
    }

    /// Adds an empty zone at the end, named as S150 says: the typed name,
    /// trimmed, else `default_name(n)` with n one more than the number of
    /// zones, counting up past names in use. Nothing is added for an ID a
    /// zone already has; either way the zone with that ID is returned.
    pub fn new_zone(
        &mut self,
        id: &str,
        typed: &str,
        default_name: impl Fn(usize) -> String,
    ) -> &Zone {
        if let Some(index) = self.list.iter().position(|zone| zone.id == id) {
            return &self.list[index];
        }
        let names: Vec<&str> = self.list.iter().map(|zone| zone.name.as_str()).collect();
        let name = new_group_name(typed, &names, default_name);
        self.list.push(Zone {
            id: id.to_owned(),
            name,
            members: Vec::new(),
            volume: DEFAULT_ZONE_VOLUME,
            member_names: BTreeMap::new(),
        });
        &self.list[self.list.len() - 1]
    }

    /// Renames zone `id` (S150): a blank name keeps the old one. False when
    /// no zone has that ID.
    pub fn rename(&mut self, id: &str, typed: &str) -> bool {
        let Some(zone) = self.list.iter_mut().find(|zone| zone.id == id) else {
            return false;
        };
        zone.name = renamed_group(&zone.name, typed);
        true
    }

    /// Moves `key` into zone `to`, after its members, taking it out of the
    /// zone that held it (S245), with its kept name (S286). A key already in
    /// `to` keeps its place. False, changing nothing, when no zone has that
    /// ID.
    pub fn move_speaker(&mut self, key: &str, to: &str) -> bool {
        let Some(target) = self.list.iter().position(|zone| zone.id == to) else {
            return false;
        };
        if self.list[target].members.iter().any(|member| member == key) {
            return true;
        }
        let name = self
            .zone_of(key)
            .and_then(|zone| zone.member_names.get(key).cloned());
        self.remove_speaker(key);
        self.list[target].members.push(key.to_owned());
        if let Some(name) = name {
            self.list[target].member_names.insert(key.to_owned(), name);
        }
        true
    }

    /// Takes `key` out of its zone, with its kept name, leaving it in none.
    /// The zone stays, even when it is empty now (S245). False when no zone
    /// held it.
    pub fn remove_speaker(&mut self, key: &str) -> bool {
        let mut removed = false;
        for zone in &mut self.list {
            let before = zone.members.len();
            zone.members.retain(|member| member != key);
            zone.member_names.remove(key);
            removed |= zone.members.len() != before;
        }
        removed
    }

    /// Deletes zone `id`; its members are then in no zone, and nothing else
    /// changes (S245). False when no zone has that ID.
    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.list.len();
        self.list.retain(|zone| zone.id != id);
        self.list.len() != before
    }

    /// Sets zone `id`'s volume, through [`zone_volume`]. False when no zone
    /// has that ID.
    pub fn set_volume(&mut self, id: &str, volume: f64) -> bool {
        let Some(zone) = self.list.iter_mut().find(|zone| zone.id == id) else {
            return false;
        };
        zone.volume = zone_volume(volume);
        true
    }

    /// A speaker's key changed from `from` to `to` (S204): `to` takes
    /// `from`'s place and kept name, unless a zone holds `to` already, which
    /// then keeps it and `from` goes.
    pub fn rename_key(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        if self.zone_of(to).is_some() {
            self.remove_speaker(from);
            return;
        }
        for zone in &mut self.list {
            for member in &mut zone.members {
                if member == from {
                    *member = to.to_owned();
                }
            }
            if let Some(name) = zone.member_names.remove(from) {
                zone.member_names.insert(to.to_owned(), name);
            }
        }
    }
}

/// A saved group, as the migration reads it.
#[derive(Clone, Copy, Debug)]
pub struct SavedGroup<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub members: &'a [String],
}

/// The zones the saved groups become (S248): one per group, in saved order,
/// with the group's ID, its name as it is and volume 100; a key an earlier
/// zone holds is left out of the later one. The same groups always give the
/// same zones, so a migration cut short before its mark runs again to the
/// same result.
pub fn zones_from_groups<'a>(groups: impl IntoIterator<Item = SavedGroup<'a>>) -> Zones {
    Zones::from_stored(groups.into_iter().map(|group| Zone {
        id: group.id.to_owned(),
        name: group.name.to_owned(),
        members: group.members.to_vec(),
        volume: DEFAULT_ZONE_VOLUME,
        member_names: BTreeMap::new(),
    }))
}

/// The one-time migration (S248): the zones to store before the mark, or
/// `None` once the mark is set, so later launches change nothing.
pub fn migrate<'a>(
    marked: bool,
    groups: impl IntoIterator<Item = SavedGroup<'a>>,
) -> Option<Zones> {
    (!marked).then(|| zones_from_groups(groups))
}

/// What kind of speaker a zone member is, for the click plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    Local,
    /// An AirPlay speaker reached over AirPlay 1.
    AirPlay,
    /// An AirPlay speaker or target reached over AirPlay 2 (P169 applies).
    AirPlay2,
    Cast,
}

/// What the host knows of one zone member when the zone is clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub kind: MemberKind,
    /// Listed now and not hidden (S245).
    pub listed: bool,
    /// Switched on.
    pub on: bool,
    /// A Cast speaker the host can't start now, such as one another app
    /// casts to.
    pub busy: bool,
    /// A stereo pair or home theater target while the AirPlay 2 switch is
    /// off (S246): it counts as absent.
    pub needs_airplay2: bool,
    /// A speaker that plays as part of a stereo pair or home theater target
    /// now (S247).
    pub in_target: bool,
}

/// What the host knows outside the zone when it is clicked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Situation {
    pub streaming: bool,
    /// A Cast speaker outside the zone is on.
    pub cast_on_elsewhere: bool,
    /// An AirPlay 2 speaker outside the zone plays (P169).
    pub airplay2_playing_elsewhere: bool,
}

/// Why a Cast member stays off on a click that switches the zone on (S246).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastLeftOff {
    /// A Cast speaker outside the zone is on; the click leaves it on.
    AnotherCastPlays,
    /// Another Cast member of the zone plays: one Cast speaker at a time.
    OneCastAtATime,
    /// The host can't start it now.
    Busy,
}

/// What a click does with one member (S246, S247).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Not listed, or hidden: left alone.
    Absent,
    /// A pair or theater target while AirPlay 2 is off: left alone, and its
    /// row reads "Needs AirPlay 2 (Experimental)".
    NeedsAirPlay2,
    /// It plays as part of a target: left alone, and its row reads "Plays as
    /// part of <target>".
    InTarget,
    /// Switched on, or stays on.
    On,
    /// Switched on, but held back by P169 until the other AirPlay 2 speaker
    /// stops: "Waiting for the other AirPlay 2 speaker".
    Waiting,
    /// Switched off, or stays off.
    Off,
    /// A Cast member that is, or goes, off on a click that switches the zone
    /// on, with the reason its row gives.
    CastLeftOff(CastLeftOff),
}

/// What a click does with the zone as a whole (S246).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickAction {
    /// No member counts: nothing changes.
    Nothing,
    /// Members go on, streaming starting first when `start_streaming`.
    TurnOn { start_streaming: bool },
    /// Every counted member goes off.
    TurnOff,
}

/// A click's plan: the action, and one step per member in zone order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClickPlan {
    pub action: ClickAction,
    pub steps: Vec<Step>,
}

/// The plan of a click on a zone (S246, S247). Only present members count:
/// listed, not hidden, not a target that needs AirPlay 2 and not playing as
/// part of a target. The members a click switches on are every present one
/// but the Cast members, of which at most one goes on: the one already on,
/// else the first not busy, and none while a Cast speaker outside the zone is
/// on. When streaming is on and each of those is on already, the click
/// switches every present member off; otherwise it switches them on, starting
/// streaming when it is off. AirPlay 2 members all go on, and all but the one
/// P169 lets play wait. A zone without a present member does nothing.
pub fn click_plan(members: &[Member], situation: Situation) -> ClickPlan {
    let present = |member: &Member| member.listed && !member.needs_airplay2 && !member.in_target;
    let cast = members
        .iter()
        .position(|m| present(m) && m.kind == MemberKind::Cast && m.on)
        .or_else(|| {
            (!situation.cast_on_elsewhere)
                .then(|| {
                    members
                        .iter()
                        .position(|m| present(m) && m.kind == MemberKind::Cast && !m.busy)
                })
                .flatten()
        });
    let counted = |index: usize| {
        let member = &members[index];
        present(member) && (member.kind != MemberKind::Cast || cast == Some(index))
    };
    let mut any_counted = false;
    let mut all_on = true;
    for (index, member) in members.iter().enumerate() {
        if counted(index) {
            any_counted = true;
            all_on &= member.on;
        }
    }
    let turn_off = situation.streaming && any_counted && all_on;
    let action = if !any_counted {
        ClickAction::Nothing
    } else if turn_off {
        ClickAction::TurnOff
    } else {
        ClickAction::TurnOn {
            start_streaming: !situation.streaming,
        }
    };
    // The AirPlay 2 member P169 lets play: one already playing, else the first.
    let airplay2_plays = (!situation.airplay2_playing_elsewhere)
        .then(|| {
            let mut airplay2 = (0..members.len())
                .filter(|&index| counted(index) && members[index].kind == MemberKind::AirPlay2);
            let first = airplay2.clone().next();
            airplay2
                .find(|&index| situation.streaming && members[index].on)
                .or(first)
        })
        .flatten();
    let steps = members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            // A target needing AirPlay 2 isn't listed, and says why.
            if member.needs_airplay2 {
                Step::NeedsAirPlay2
            } else if !member.listed {
                Step::Absent
            } else if member.in_target {
                Step::InTarget
            } else if turn_off {
                Step::Off
            } else if member.kind == MemberKind::Cast && cast != Some(index) {
                Step::CastLeftOff(if cast.is_some() {
                    CastLeftOff::OneCastAtATime
                } else if situation.cast_on_elsewhere {
                    CastLeftOff::AnotherCastPlays
                } else {
                    CastLeftOff::Busy
                })
            } else if member.kind == MemberKind::AirPlay2 && airplay2_plays != Some(index) {
                Step::Waiting
            } else {
                Step::On
            }
        })
        .collect();
    ClickPlan { action, steps }
}
