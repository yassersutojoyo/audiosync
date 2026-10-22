//! Cast groups (C89 to C103): the member list from the multizone namespace
//! and the addresses a group session follows while its leader changes.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};

use crate::discovery::DeviceId;

/// One member of a group as the multizone namespace names it (C94).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupMember {
    /// `deviceId` as sent.
    pub device_id: String,
    /// `deviceId` read as a UUID, for the match against discovered `id`s
    /// (C95); `None` when it is not one.
    pub id: Option<DeviceId>,
    pub name: Option<String>,
}

impl GroupMember {
    fn from_value(value: &Value) -> Option<GroupMember> {
        let device_id = value.get("deviceId")?.as_str()?.to_owned();
        Some(GroupMember {
            id: DeviceId::parse(&device_id),
            name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            device_id,
        })
    }

    /// Whether `device_id` names this member: as UUIDs when both read as
    /// one, so case and dashes don't matter (C95), else as text.
    pub fn is(&self, device_id: &str) -> bool {
        match (self.id, DeviceId::parse(device_id)) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => self.device_id.eq_ignore_ascii_case(device_id),
        }
    }
}

/// A message on the multizone namespace (C94, C96).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultizoneMessage {
    /// `MULTIZONE_STATUS`: the whole list.
    Status(Vec<GroupMember>),
    /// `DEVICE_ADDED` or `DEVICE_UPDATED`.
    Upsert(GroupMember),
    /// `DEVICE_REMOVED`.
    Removed(String),
    /// Any other type, or one of the above without the fields it needs;
    /// logged only.
    Other(String),
}

impl MultizoneMessage {
    pub fn from_payload(kind: &str, payload: &Map<String, Value>) -> MultizoneMessage {
        let other = || MultizoneMessage::Other(kind.to_owned());
        match kind {
            "MULTIZONE_STATUS" => payload
                .get("status")
                .and_then(|status| status.get("devices"))
                .and_then(Value::as_array)
                .map_or_else(other, |devices| {
                    MultizoneMessage::Status(
                        devices.iter().filter_map(GroupMember::from_value).collect(),
                    )
                }),
            "DEVICE_ADDED" | "DEVICE_UPDATED" => payload
                .get("device")
                .and_then(GroupMember::from_value)
                .map_or_else(other, MultizoneMessage::Upsert),
            "DEVICE_REMOVED" => payload
                .get("deviceId")
                .and_then(Value::as_str)
                .map_or_else(other, |id| MultizoneMessage::Removed(id.to_owned())),
            _ => other(),
        }
    }
}

/// A group's members as the session knows them: unknown until the first
/// message that names some (C94).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemberList {
    members: Option<Vec<GroupMember>>,
}

impl MemberList {
    pub fn new() -> MemberList {
        MemberList::default()
    }

    /// `None` while no message has named the members.
    pub fn members(&self) -> Option<&[GroupMember]> {
        self.members.as_deref()
    }

    /// Applies a message; true when the list changed. `Other` changes
    /// nothing (C96).
    pub fn apply(&mut self, message: &MultizoneMessage) -> bool {
        let before = self.members.clone();
        match message {
            MultizoneMessage::Status(members) => self.members = Some(members.clone()),
            MultizoneMessage::Upsert(member) => {
                let list = self.members.get_or_insert_with(Vec::new);
                match list.iter_mut().find(|known| known.is(&member.device_id)) {
                    Some(known) => *known = member.clone(),
                    None => list.push(member.clone()),
                }
            }
            MultizoneMessage::Removed(device_id) => {
                if let Some(list) = &mut self.members {
                    list.retain(|known| !known.is(device_id));
                }
            }
            MultizoneMessage::Other(_) => {}
        }
        self.members != before
    }
}

/// The addresses a group session uses, kept current by the host from the
/// group's latest records (C90) and its members' (C103).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupAddresses {
    /// The group's own addresses and port: where its leader is now.
    pub group: Vec<SocketAddr>,
    /// The addresses of the discovered devices that match a member (C95).
    pub members: Vec<IpAddr>,
}

/// [`GroupAddresses`] shared between the host, which updates them as
/// discovery reports, and the session, which reads them on every connect.
#[derive(Clone, Debug, Default)]
pub struct GroupRecords(Arc<Mutex<GroupAddresses>>);

/// The same shared records, not equal contents.
impl PartialEq for GroupRecords {
    fn eq(&self, other: &GroupRecords) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl GroupRecords {
    pub fn new(group: Vec<SocketAddr>) -> GroupRecords {
        GroupRecords(Arc::new(Mutex::new(GroupAddresses {
            group,
            members: Vec::new(),
        })))
    }

    pub fn get(&self) -> GroupAddresses {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The group's records moved or were resolved again. An empty list
    /// keeps the last one: the records expired, and the session retries
    /// where the group was until they come back.
    pub fn set_group(&self, group: Vec<SocketAddr>) {
        if group.is_empty() {
            return;
        }
        self.0.lock().unwrap_or_else(PoisonError::into_inner).group = group;
    }

    pub fn set_members(&self, members: Vec<IpAddr>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .members = members;
    }

    /// Every address the stream server answers for this group (C103).
    pub fn allowed(&self) -> Vec<IpAddr> {
        let addresses = self.get();
        let mut allowed: Vec<IpAddr> = addresses.group.iter().map(SocketAddr::ip).collect();
        allowed.extend(addresses.members);
        allowed.sort();
        allowed.dedup();
        allowed
    }
}
