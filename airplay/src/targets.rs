//! Targets made of several receivers: stereo pairs and home theater groups
//! set up in Apple's apps (P171 to P176, P181), recognized from the group
//! keys of discovery (P172). A target is reported to the host as one, its
//! members stay devices of their own, and the sender drives it by giving
//! every member a session of its own (P177).
//!
//! Groups another sender made share only a group key and are never a
//! target (P175). Everything here is sans-I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::discovery::txt::group_key;
use crate::discovery::{Device, DeviceKey, StatusFlags};

/// What kind of target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetKind {
    /// Devices sharing a `tsid` and a `gpn` (P173).
    StereoPair,
    /// The speakers an Apple TV plays its sound through (P174).
    HomeTheater,
}

/// A target's identity: the pair's `tsid`, or the group key of the Apple TV
/// of a home theater group, lowercased. It stays the same while `gid`
/// changes form, so the host keys stored settings by it (P176).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetId {
    pub kind: TargetKind,
    pub key: String,
}

impl TargetId {
    const PAIR: &'static str = "pair:";
    const THEATER: &'static str = "theater:";

    /// The text form, `pair:<tsid>` or `theater:<group key>`, which can't
    /// be mistaken for a device identifier.
    pub fn parse(text: &str) -> Option<TargetId> {
        let (kind, key) = if let Some(key) = text.strip_prefix(Self::PAIR) {
            (TargetKind::StereoPair, key)
        } else {
            (TargetKind::HomeTheater, text.strip_prefix(Self::THEATER)?)
        };
        (!key.is_empty()).then(|| TargetId {
            kind,
            key: key.to_ascii_lowercase(),
        })
    }
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.kind {
            TargetKind::StereoPair => Self::PAIR,
            TargetKind::HomeTheater => Self::THEATER,
        };
        write!(f, "{prefix}{}", self.key)
    }
}

/// One member of a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub key: DeviceKey,
    pub name: String,
    /// The device as discovery sees it; `None` while it is out of view.
    pub device: Option<Box<Device>>,
}

impl Member {
    pub fn in_view(&self) -> bool {
        self.device.is_some()
    }
}

/// A stereo pair or home theater group (P176).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub id: TargetId,
    pub name: String,
    /// Ordered by normalized identifier; members out of view are those the
    /// sender saw before.
    pub members: Vec<Member>,
    /// The leader among the members in view (P173).
    pub leader: Option<DeviceKey>,
    /// The Apple TV of a home theater group, while it is in view (P174). It
    /// is never a member and never gets a session (P177).
    pub control: Option<DeviceKey>,
    /// How many members the target has, when its keys say: 2 for a pair,
    /// `psgsz` less the Apple TV for a home theater group.
    pub expected_members: Option<usize>,
    /// A member in view sets flags bit 14 (P181).
    pub partner_unreachable: bool,
}

impl Target {
    /// Every member is in view and none reports its partner unreachable
    /// (P176, P181).
    pub fn complete(&self) -> bool {
        let in_view = self
            .members
            .iter()
            .filter(|member| member.in_view())
            .count();
        !self.partner_unreachable
            && in_view == self.members.len()
            && self
                .expected_members
                .is_none_or(|expected| in_view >= expected)
    }

    /// The members out of view that the sender can name (P181).
    pub fn missing(&self) -> impl Iterator<Item = &DeviceKey> {
        self.members
            .iter()
            .filter(|member| !member.in_view())
            .map(|member| &member.key)
    }

    pub fn contains(&self, key: &DeviceKey) -> bool {
        self.members.iter().any(|member| member.key == *key)
    }

    /// Who answers a volume read-back: the leader, or the first member in
    /// view (P180).
    pub fn volume_source(&self) -> Option<&DeviceKey> {
        self.leader.as_ref().or_else(|| {
            self.members
                .iter()
                .find(|member| member.in_view())
                .map(|member| &member.key)
        })
    }
}

fn flags(device: &Device) -> StatusFlags {
    device.status_flags()
}

fn same_key(a: Option<&String>, b: Option<&String>) -> Option<String> {
    let a = group_key(a?)?;
    (group_key(b?)? == a).then_some(a)
}

/// The leader among `members` (P173): the one with flags bit 13; when none
/// or several have it, the one whose `gid` middle field is 0; otherwise the
/// lowest identifier.
fn leader(members: &[&Device]) -> Option<DeviceKey> {
    let only = |picked: Vec<&&Device>| match picked.as_slice() {
        [one] => Some(one.key.clone()),
        _ => None,
    };
    only(
        members
            .iter()
            .filter(|device| flags(device).has(StatusFlags::TIGHT_SYNC_LEADER))
            .collect(),
    )
    .or_else(|| {
        only(
            members
                .iter()
                .filter(|device| device.group_keys().gid_middle() == Some(0))
                .collect(),
        )
    })
    .or_else(|| members.iter().map(|device| device.key.clone()).min())
}

/// The name of the first member, in identifier order, that has a `gpn`.
fn public_name(members: &[&Device]) -> Option<String> {
    members
        .iter()
        .find_map(|device| device.group_keys().group_public_name.clone())
}

fn member(device: &Device) -> Member {
    Member {
        key: device.key.clone(),
        name: device.name.clone(),
        device: Some(Box::new(device.clone())),
    }
}

/// The targets among the devices in view (P173, P174), ordered by
/// identity. A device belongs to at most one: the speakers of a home
/// theater group that are also a stereo pair belong to the group. Devices
/// that share only a group key stay apart (P175).
pub fn find_targets<'a>(devices: impl IntoIterator<Item = &'a Device>) -> Vec<Target> {
    let mut devices: Vec<&Device> = devices.into_iter().collect();
    devices.sort_by(|a, b| a.key.cmp(&b.key));
    let mut targets = Vec::new();
    let mut taken = BTreeSet::new();

    let mut theaters: BTreeMap<String, Vec<&Device>> = BTreeMap::new();
    for device in &devices {
        let keys = device.group_keys();
        if device.is_apple_tv() || keys.is_group_leader != Some(false) {
            continue;
        }
        if let Some(key) = same_key(keys.psgid.as_ref(), keys.parent_group_id.as_ref()) {
            theaters.entry(key).or_default().push(device);
        }
    }
    for (key, members) in theaters {
        let control = devices.iter().find(|device| {
            let keys = device.group_keys();
            device.is_apple_tv()
                && keys.is_group_leader == Some(true)
                && [keys.gid.as_ref(), keys.psgid.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|text| group_key(text).as_deref() == Some(key.as_str()))
        });
        let size = control
            .and_then(|device| device.group_keys().psgsz)
            .or_else(|| members.iter().find_map(|device| device.group_keys().psgsz));
        let name = public_name(&members)
            .or_else(|| control.map(|device| device.name.clone()))
            .unwrap_or_else(|| members[0].name.clone());
        taken.extend(members.iter().map(|device| device.key.clone()));
        targets.push(Target {
            id: TargetId {
                kind: TargetKind::HomeTheater,
                key,
            },
            name,
            leader: leader(&members),
            control: control.map(|device| device.key.clone()),
            expected_members: size
                .and_then(|size| usize::try_from(size).ok())
                .map(|size| size.saturating_sub(1)),
            partner_unreachable: members
                .iter()
                .any(|device| flags(device).has(StatusFlags::TIGHT_SYNC_PARTNER_UNREACHABLE)),
            members: members.iter().map(|device| member(device)).collect(),
        });
    }

    let mut pairs: BTreeMap<String, Vec<&Device>> = BTreeMap::new();
    for device in &devices {
        let keys = device.group_keys();
        if taken.contains(&device.key) || keys.group_public_name.is_none() {
            continue;
        }
        if let Some(tsid) = &keys.tight_sync_id {
            pairs
                .entry(tsid.to_ascii_lowercase())
                .or_default()
                .push(device);
        }
    }
    for (tsid, members) in pairs {
        let unreachable = members
            .iter()
            .any(|device| flags(device).has(StatusFlags::TIGHT_SYNC_PARTNER_UNREACHABLE));
        if members.len() < 2 && !unreachable {
            continue;
        }
        targets.push(Target {
            id: TargetId {
                kind: TargetKind::StereoPair,
                key: tsid,
            },
            name: public_name(&members).unwrap_or_else(|| members[0].name.clone()),
            leader: leader(&members),
            control: None,
            expected_members: Some(2),
            partner_unreachable: unreachable,
            members: members.iter().map(|device| member(device)).collect(),
        });
    }
    targets.sort_by(|a, b| a.id.cmp(&b.id));
    targets
}

/// A change in the targets (P176).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetChange {
    /// A target appeared, or its name, members, leader, control device or
    /// completeness changed.
    Updated(Box<Target>),
    /// No member of it is in view any more, or its members no longer form
    /// it.
    Gone(TargetId),
}

/// The targets on the network, recomputed on every change of the devices
/// (P176). Members seen before stay listed, out of view, while their
/// target is incomplete, so the host can name the missing one (P181).
#[derive(Debug, Default)]
pub struct Targets {
    targets: BTreeMap<TargetId, Target>,
    seen: BTreeMap<TargetId, BTreeMap<DeviceKey, String>>,
}

impl Targets {
    pub fn new() -> Targets {
        Targets::default()
    }

    pub fn update<'a>(
        &mut self,
        devices: impl IntoIterator<Item = &'a Device>,
    ) -> Vec<TargetChange> {
        let devices: Vec<&Device> = devices.into_iter().collect();
        let in_view: BTreeSet<&DeviceKey> = devices.iter().map(|device| &device.key).collect();
        let found = find_targets(devices.iter().copied());
        // A device in view outside a target has left it, so it is never named missing there.
        for (id, seen) in &mut self.seen {
            let target = found.iter().find(|target| target.id == *id);
            seen.retain(|key, _| {
                !in_view.contains(key) || target.is_some_and(|target| target.contains(key))
            });
        }
        self.seen.retain(|_, seen| !seen.is_empty());
        let mut next = BTreeMap::new();
        for mut target in found {
            let seen = self.seen.entry(target.id.clone()).or_default();
            for member in &target.members {
                seen.insert(member.key.clone(), member.name.clone());
            }
            // Without a group size, only a member seen before can show the target is short.
            if target.partner_unreachable
                || target
                    .expected_members
                    .is_none_or(|expected| target.members.len() < expected)
            {
                for (key, name) in seen.iter() {
                    if !in_view.contains(key) && !target.contains(key) {
                        target.members.push(Member {
                            key: key.clone(),
                            name: name.clone(),
                            device: None,
                        });
                    }
                }
                target.members.sort_by(|a, b| a.key.cmp(&b.key));
            }
            next.insert(target.id.clone(), target);
        }
        let mut changes = Vec::new();
        for id in self.targets.keys() {
            if !next.contains_key(id) {
                changes.push(TargetChange::Gone(id.clone()));
            }
        }
        for (id, target) in &next {
            if self.targets.get(id) != Some(target) {
                changes.push(TargetChange::Updated(Box::new(target.clone())));
            }
        }
        self.targets = next;
        changes
    }

    pub fn get(&self, id: &TargetId) -> Option<&Target> {
        self.targets.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Target> {
        self.targets.values()
    }

    /// The target a device is a member of.
    pub fn target_of(&self, key: &DeviceKey) -> Option<&TargetId> {
        self.targets
            .values()
            .find(|target| target.contains(key))
            .map(|target| &target.id)
    }
}
