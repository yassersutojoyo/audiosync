//! Speaker keys, group names, the local device list, the speaker layout and the
//! notice list (S149 to S153). These run in the host, not the audio engine, but
//! a port that replaces the host keeps them (spec section 17).
//!
//! User-facing text stays with the host: where a rule needs a display string
//! (a default group name), the host passes a formatter.

use std::cmp::Ordering;
use std::fmt;

/// A speaker's stable key (S149): `ap:<engine output id>` or `lc:<device UID>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SpeakerKey {
    /// An AirPlay speaker of the engine, by output ID.
    AirPlay(String),
    /// A local output device, by UID.
    Local(String),
}

impl SpeakerKey {
    /// Parses a key, splitting at the first colon only (device UIDs may contain
    /// colons); any prefix other than `ap` or `lc` is rejected (S149).
    pub fn parse(text: &str) -> Option<SpeakerKey> {
        let (prefix, id) = text.split_once(':')?;
        match prefix {
            "ap" => Some(SpeakerKey::AirPlay(id.to_owned())),
            "lc" => Some(SpeakerKey::Local(id.to_owned())),
            _ => None,
        }
    }

    /// The engine output ID or device UID.
    pub fn id(&self) -> &str {
        match self {
            SpeakerKey::AirPlay(id) | SpeakerKey::Local(id) => id,
        }
    }
}

impl fmt::Display for SpeakerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpeakerKey::AirPlay(id) => write!(f, "ap:{id}"),
            SpeakerKey::Local(uid) => write!(f, "lc:{uid}"),
        }
    }
}

/// A group name as typed: trimmed; blank or whitespace-only is no name (S150).
pub fn group_name(typed: &str) -> Option<&str> {
    let trimmed = typed.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// The name of a new group (S150): the typed name, trimmed; without one,
/// `default_name(n)` with n one more than the number of existing groups,
/// counting up past names already in use.
pub fn new_group_name<S: AsRef<str>>(
    typed: &str,
    existing: &[S],
    default_name: impl Fn(usize) -> String,
) -> String {
    if let Some(name) = group_name(typed) {
        return name.to_owned();
    }
    let mut number = existing.len() + 1;
    loop {
        let candidate = default_name(number);
        if !existing.iter().any(|name| name.as_ref() == candidate) {
            return candidate;
        }
        number += 1;
    }
}

/// A group's name after a rename: the typed name, trimmed; a blank rename keeps
/// the old name (S150).
pub fn renamed_group(current: &str, typed: &str) -> String {
    group_name(typed).unwrap_or(current).to_owned()
}

/// How an output device is connected, as the OS reports it (S151, S152).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    /// Built into the computer.
    BuiltIn,
    /// USB.
    Usb,
    /// Bluetooth.
    Bluetooth,
    /// Bluetooth LE.
    BluetoothLe,
    /// HDMI.
    Hdmi,
    /// DisplayPort.
    DisplayPort,
    /// Thunderbolt.
    Thunderbolt,
    /// FireWire.
    FireWire,
    /// PCI.
    Pci,
    /// AVB.
    Avb,
    /// The operating system's own AirPlay output.
    AirPlay,
    /// A virtual (software) device.
    Virtual,
    /// An aggregate device.
    Aggregate,
    /// An automatically created aggregate device.
    AutoAggregate,
    /// Anything else.
    Unknown,
}

impl Transport {
    /// Virtual, aggregate and auto-aggregate devices and the OS's own AirPlay
    /// output are hidden by default; hardware and unknown transports never are (S151).
    pub fn hidden_by_default(self) -> bool {
        matches!(
            self,
            Transport::Virtual
                | Transport::Aggregate
                | Transport::AutoAggregate
                | Transport::AirPlay
        )
    }

    /// The tag a transport shows as (S152).
    pub fn tag(self) -> TypeTag {
        match self {
            Transport::BuiltIn => TypeTag::BuiltIn,
            Transport::Usb => TypeTag::Usb,
            Transport::Bluetooth | Transport::BluetoothLe => TypeTag::Bluetooth,
            Transport::Hdmi => TypeTag::Hdmi,
            Transport::DisplayPort => TypeTag::DisplayPort,
            Transport::Thunderbolt => TypeTag::Thunderbolt,
            Transport::FireWire => TypeTag::FireWire,
            Transport::Pci => TypeTag::Pci,
            Transport::Avb => TypeTag::Avb,
            Transport::AirPlay => TypeTag::AirPlay,
            Transport::Virtual => TypeTag::Virtual,
            Transport::Aggregate | Transport::AutoAggregate => TypeTag::Aggregate,
            Transport::Unknown => TypeTag::Other,
        }
    }

    /// Only built-in outputs are "This Mac" (S152).
    pub fn is_this_computer(self) -> bool {
        self == Transport::BuiltIn
    }
}

/// The type tag a local speaker shows (S152). Icons and colors are the host's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeTag {
    /// Built-in.
    BuiltIn,
    /// Bluetooth and Bluetooth LE.
    Bluetooth,
    /// USB.
    Usb,
    /// HDMI.
    Hdmi,
    /// DisplayPort.
    DisplayPort,
    /// Thunderbolt.
    Thunderbolt,
    /// The OS's own AirPlay output.
    AirPlay,
    /// FireWire.
    FireWire,
    /// Virtual.
    Virtual,
    /// Aggregate and auto-aggregate.
    Aggregate,
    /// PCI.
    Pci,
    /// AVB.
    Avb,
    /// Unknown transports.
    Other,
}

/// The local output list (S151): devices hidden by default stay listed when
/// chosen (enabled, auto-connect, in a group, or already listed); with
/// `show_all` ("Show Virtual Devices") everything is listed. The input order
/// (the OS's order) is kept.
pub fn listed_devices<T>(
    devices: &[T],
    show_all: bool,
    transport: impl Fn(&T) -> Transport,
    chosen: impl Fn(&T) -> bool,
) -> Vec<&T> {
    devices
        .iter()
        .filter(|device| show_all || !transport(device).hidden_by_default() || chosen(device))
        .collect()
}

/// Compares display names the way the speaker list sorts them (S152): ignoring
/// case, with runs of digits compared by numeric value ("Room 2" before
/// "Room 10"). Equal names compare equal, so a stable sort keeps their order.
pub fn compare_names(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let left = take_digits(&mut a);
                let right = take_digits(&mut b);
                let order = compare_numbers(&left, &right);
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                a.next();
                b.next();
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

fn take_digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut digits = String::new();
    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
        digits.push(digit);
    }
    digits
}

/// Compares two runs of ASCII digits by value, without overflow.
fn compare_numbers(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// A speaker as the speaker list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedSpeaker {
    /// Its key.
    pub key: SpeakerKey,
    /// Display name, any length of Unicode (S147).
    pub name: String,
    /// The transport of a local output; `None` for an AirPlay speaker.
    pub transport: Option<Transport>,
}

/// The speaker list's two sections (S152).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// Built-in outputs, in the OS's order.
    pub this_computer: Vec<SpeakerKey>,
    /// Every other local output and every AirPlay speaker, sorted by name.
    pub connected: Vec<SpeakerKey>,
}

/// Lays out the speakers (S152): only built-in outputs are "This Mac", in the
/// order given; every other local output and every AirPlay speaker share one
/// "connected" list sorted with [`compare_names`], ties keeping their order.
pub fn layout(speakers: &[ListedSpeaker]) -> Layout {
    let (this_computer, mut connected): (Vec<&ListedSpeaker>, Vec<&ListedSpeaker>) = speakers
        .iter()
        .partition(|speaker| speaker.transport.is_some_and(Transport::is_this_computer));
    connected.sort_by(|a, b| compare_names(&a.name, &b.name));
    Layout {
        this_computer: this_computer
            .into_iter()
            .map(|speaker| speaker.key.clone())
            .collect(),
        connected: connected
            .into_iter()
            .map(|speaker| speaker.key.clone())
            .collect(),
    }
}

/// At most this many notices are kept (S153).
pub const MAX_NOTICES: usize = 4;

/// What a notice is about; there is at most one notice per subject (S153).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NoticeSubject {
    /// Starting a session.
    Start,
    /// The capture source.
    Source,
    /// Whether captured audio arrives (S18).
    Capture,
    /// One speaker.
    Speaker(SpeakerKey),
    /// The Automation permission for one player, by bundle ID.
    Automation(String),
    /// The Local Network permission.
    LocalNetwork,
    /// The source heard twice: straight from an output as well as through
    /// AudioSync, where capture can't mute the source (Windows).
    DoubleAudio,
    /// The AirPlay sender as a whole: it stopped after an internal error, or
    /// its discovery did.
    AirPlay,
    /// The user's settings file: it couldn't be written (Windows).
    SettingsFile,
    /// The speaker pairings' file: it couldn't be written (Windows).
    PairingsFile,
}

/// A privacy setting a notice can open (S153); the host maps it to its settings app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrivacyPane {
    /// Local Network.
    LocalNetwork,
    /// Automation.
    Automation,
}

/// The notice list (S153): newest first, one per subject (a repost replaces the
/// old notice and moves it to the top), at most four (the oldest drop off).
#[derive(Clone, Debug, PartialEq)]
pub struct Notices<S, N> {
    items: Vec<(S, N)>,
}

impl<S, N> Default for Notices<S, N> {
    fn default() -> Self {
        Notices { items: Vec::new() }
    }
}

impl<S: PartialEq, N> Notices<S, N> {
    /// No notices.
    pub fn new() -> Self {
        Self::default()
    }

    /// Posts a notice about `subject`.
    pub fn post(&mut self, subject: S, notice: N) {
        self.items.retain(|(existing, _)| *existing != subject);
        self.items.insert(0, (subject, notice));
        self.items.truncate(MAX_NOTICES);
    }

    /// Removes the notices about these subjects only; returns whether anything changed.
    pub fn clear(&mut self, subjects: &[S]) -> bool {
        let before = self.items.len();
        self.items
            .retain(|(subject, _)| !subjects.contains(subject));
        self.items.len() != before
    }

    /// Removes every notice; returns whether anything changed.
    pub fn clear_all(&mut self) -> bool {
        let changed = !self.items.is_empty();
        self.items.clear();
        changed
    }

    /// The notices, newest first.
    pub fn iter(&self) -> impl Iterator<Item = (&S, &N)> {
        self.items.iter().map(|(subject, notice)| (subject, notice))
    }

    /// The notice about `subject`.
    pub fn get(&self, subject: &S) -> Option<&N> {
        self.items
            .iter()
            .find(|(existing, _)| existing == subject)
            .map(|(_, notice)| notice)
    }

    /// The notice about `subject`, to change in place: it keeps its place in
    /// the list.
    pub fn get_mut(&mut self, subject: &S) -> Option<&mut N> {
        self.items
            .iter_mut()
            .find(|(existing, _)| existing == subject)
            .map(|(_, notice)| notice)
    }

    /// Number of notices.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there is no notice.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
