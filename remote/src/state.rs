//! The snapshot the app publishes and the commands the page sends back.

use std::collections::HashSet;
use std::fmt;

use serde_json::{Map, Value, json};

/// The top of every volume scale; the bottom is 0.
pub const VOLUME_MAX: f64 = 100.0;

/// Everything the page shows, as the app last published it.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    /// The computer's name as the app shows it, for the page's footer.
    pub computer_name: String,
    /// 0 to 100.
    pub master_volume: f64,
    pub now_playing: Option<NowPlaying>,
    /// In the app's order.
    pub speakers: Vec<Speaker>,
    /// In the app's order; empty without zones.
    pub zones: Vec<Zone>,
    /// In the app's order; empty without presets.
    pub presets: Vec<Preset>,
    /// A microphone run is in progress: every write answers 409 `calibrating`.
    pub calibrating: bool,
    /// What the app can capture, in its source picker's order.
    pub sources: Vec<Source>,
    /// The source in use; `None` when it isn't one of `sources`.
    pub source_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    /// The app the audio comes from.
    pub source: String,
    /// False while the app is paused; true when the snapshot leaves it out.
    pub playing: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Speaker {
    /// Unique in a snapshot; the page puts it in `/api/speakers/{id}`.
    pub id: String,
    pub name: String,
    /// The label the app shows: local, Bluetooth, AirPlay, AirPlay 2, Google Cast.
    pub kind: String,
    pub on: bool,
    /// 0 to 100.
    pub volume: f64,
    /// How far the page's volume keys move it, above 0 and at most 100; 1
    /// when the snapshot leaves it out.
    pub volume_step: f64,
    pub delay_ms: i64,
    pub delay_min_ms: i64,
    pub delay_max_ms: i64,
    /// False when the speaker can't be driven now; its controls then fail with 409.
    pub available: bool,
    /// The row's notice text, if any.
    pub notice: Option<String>,
}

/// A zone as the page shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    /// Unique in a snapshot; the page puts it in `/api/zones/{id}`.
    pub id: String,
    pub name: String,
    /// 0 to 100.
    pub volume: f64,
    /// The speaker IDs of the members present now, in zone order.
    pub members: Vec<String>,
    pub counts: ZoneCounts,
}

/// How many present members a zone has, and how many of them are on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZoneCounts {
    /// Members on, a waiting one included.
    pub on: u32,
    pub total: u32,
    /// Members on that wait for an AirPlay 2 place or for PTP.
    pub waiting: u32,
    /// Every member the app's zone header counts, present or not; `total`
    /// when the snapshot leaves it out.
    pub all: u32,
}

/// A speaker preset as the page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preset {
    /// Unique in a snapshot; the page puts it in `/api/presets/{id}`.
    pub id: String,
    pub name: String,
    /// The speakers play it exactly now; at most one preset is current.
    pub current: bool,
}

/// Something the app can capture, as its source picker lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// Unique in a snapshot; the page posts it to `/api/source`.
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
    /// False when the app can't switch to it now; a switch then fails with
    /// 409. True when the snapshot leaves it out.
    pub available: bool,
    /// The entry's notice text, if any.
    pub notice: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Everything the computer plays.
    System,
    /// One app.
    App,
    /// An input device.
    Input,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::System => "system",
            SourceKind::App => "app",
            SourceKind::Input => "input",
        }
    }

    pub fn parse(text: &str) -> Option<SourceKind> {
        match text {
            "system" => Some(SourceKind::System),
            "app" => Some(SourceKind::App),
            "input" => Some(SourceKind::Input),
            _ => None,
        }
    }
}

/// Why a published snapshot was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the remote state is not valid: {}", self.0)
    }
}

impl std::error::Error for StateError {}

fn invalid(what: impl Into<String>) -> StateError {
    StateError(what.into())
}

fn volume_in_range(volume: f64) -> bool {
    volume.is_finite() && (0.0..=VOLUME_MAX).contains(&volume)
}

fn field<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    at: &str,
) -> Result<&'a Value, StateError> {
    object
        .get(name)
        .ok_or_else(|| invalid(format!("{at}{name} is missing")))
}

fn text_field(object: &Map<String, Value>, name: &str, at: &str) -> Result<String, StateError> {
    field(object, name, at)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("{at}{name} is not a string")))
}

fn bool_field(object: &Map<String, Value>, name: &str, at: &str) -> Result<bool, StateError> {
    field(object, name, at)?
        .as_bool()
        .ok_or_else(|| invalid(format!("{at}{name} is not true or false")))
}

fn int_field(object: &Map<String, Value>, name: &str, at: &str) -> Result<i64, StateError> {
    field(object, name, at)?
        .as_i64()
        .ok_or_else(|| invalid(format!("{at}{name} is not a whole number")))
}

fn count_field(object: &Map<String, Value>, name: &str, at: &str) -> Result<u32, StateError> {
    field(object, name, at)?
        .as_u64()
        .and_then(|count| u32::try_from(count).ok())
        .ok_or_else(|| invalid(format!("{at}{name} is not a whole number from 0")))
}

fn optional_list<'a>(root: &'a Map<String, Value>, name: &str) -> Result<&'a [Value], StateError> {
    match root.get(name) {
        None => Ok(&[]),
        Some(Value::Array(list)) => Ok(list),
        Some(_) => Err(invalid(format!("{name} is not a list"))),
    }
}

fn zone_from(
    item: &Value,
    index: usize,
    speakers: &[Speaker],
    taken: &mut HashSet<String>,
) -> Result<Zone, StateError> {
    let at = format!("zones[{index}].");
    let object = item
        .as_object()
        .ok_or_else(|| invalid(format!("zones[{index}] is not an object")))?;
    let list = field(object, "members", &at)?
        .as_array()
        .ok_or_else(|| invalid(format!("{at}members is not a list")))?;
    let mut members = Vec::with_capacity(list.len());
    for (place, member) in list.iter().enumerate() {
        let id = member
            .as_str()
            .ok_or_else(|| invalid(format!("{at}members[{place}] is not a string")))?;
        if !speakers.iter().any(|speaker| speaker.id == id) {
            return Err(invalid(format!(
                "{at}members[{place}] is not a listed speaker"
            )));
        }
        // One zone per speaker (S245).
        if !taken.insert(id.to_owned()) {
            return Err(invalid(format!(
                "{at}members[{place}] is in a zone already"
            )));
        }
        members.push(id.to_owned());
    }
    let counts = field(object, "counts", &at)?
        .as_object()
        .ok_or_else(|| invalid(format!("{at}counts is not an object")))?;
    let at_counts = format!("{at}counts.");
    let counts = ZoneCounts {
        on: count_field(counts, "on", &at_counts)?,
        total: count_field(counts, "total", &at_counts)?,
        waiting: count_field(counts, "waiting", &at_counts)?,
        all: match counts.get("all") {
            None => count_field(counts, "total", &at_counts)?,
            Some(_) => count_field(counts, "all", &at_counts)?,
        },
    };
    if counts.on > counts.total || counts.waiting > counts.on || counts.total > counts.all {
        return Err(invalid(format!(
            "{at}counts are not waiting <= on <= total <= all"
        )));
    }
    Ok(Zone {
        id: text_field(object, "id", &at)?,
        name: text_field(object, "name", &at)?,
        volume: volume_field(object, "volume", &at)?,
        members,
        counts,
    })
}

fn notice_field(object: &Map<String, Value>, at: &str) -> Result<Option<String>, StateError> {
    match object.get("notice") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(notice)) => Ok(Some(notice.clone())),
        Some(_) => Err(invalid(format!("{at}notice is not a string or null"))),
    }
}

fn sources_field(root: &Map<String, Value>) -> Result<Vec<Source>, StateError> {
    let list = match root.get("sources") {
        None => return Ok(Vec::new()),
        Some(value) => value
            .as_array()
            .ok_or_else(|| invalid("sources is not a list"))?,
    };
    let mut sources = Vec::with_capacity(list.len());
    let mut ids = HashSet::new();
    for (index, item) in list.iter().enumerate() {
        let at = format!("sources[{index}].");
        let object = item
            .as_object()
            .ok_or_else(|| invalid(format!("sources[{index}] is not an object")))?;
        let kind = text_field(object, "kind", &at)?;
        let source = Source {
            id: text_field(object, "id", &at)?,
            name: text_field(object, "name", &at)?,
            kind: SourceKind::parse(&kind)
                .ok_or_else(|| invalid(format!("{at}kind is not system, app or input")))?,
            available: match object.get("available") {
                None => true,
                Some(_) => bool_field(object, "available", &at)?,
            },
            notice: notice_field(object, &at)?,
        };
        if source.id.is_empty() {
            return Err(invalid(format!("{at}id is empty")));
        }
        if !ids.insert(source.id.clone()) {
            return Err(invalid(format!("{at}id is used twice")));
        }
        sources.push(source);
    }
    Ok(sources)
}

fn step_field(object: &Map<String, Value>, at: &str) -> Result<f64, StateError> {
    match object.get("volume_step") {
        None => Ok(1.0),
        Some(value) => value
            .as_f64()
            .filter(|step| step.is_finite() && *step > 0.0 && *step <= VOLUME_MAX)
            .ok_or_else(|| invalid(format!("{at}volume_step is not a number above 0 up to 100"))),
    }
}

fn volume_field(object: &Map<String, Value>, name: &str, at: &str) -> Result<f64, StateError> {
    field(object, name, at)?
        .as_f64()
        .filter(|volume| volume_in_range(*volume))
        .ok_or_else(|| invalid(format!("{at}{name} is not a number from 0 to 100")))
}

impl State {
    /// Reads the app's snapshot: the JSON of the API's state without
    /// `revision`, which the server assigns (one given is ignored).
    pub fn from_json(text: &str) -> Result<State, StateError> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| invalid(format!("not JSON: {error}")))?;
        let root = value.as_object().ok_or_else(|| invalid("not an object"))?;
        let computer_name = match root.get("computer_name") {
            None => String::new(),
            Some(_) => text_field(root, "computer_name", "")?,
        };
        let master = field(root, "master", "")?
            .as_object()
            .ok_or_else(|| invalid("master is not an object"))?;
        let master_volume = volume_field(master, "volume", "master.")?;
        let now_playing = match root.get("now_playing") {
            None | Some(Value::Null) => None,
            Some(Value::Object(playing)) => Some(NowPlaying {
                title: text_field(playing, "title", "now_playing.")?,
                artist: text_field(playing, "artist", "now_playing.")?,
                source: text_field(playing, "source", "now_playing.")?,
                playing: match playing.get("playing") {
                    None => true,
                    Some(_) => bool_field(playing, "playing", "now_playing.")?,
                },
            }),
            Some(_) => return Err(invalid("now_playing is not an object or null")),
        };
        let list = field(root, "speakers", "")?
            .as_array()
            .ok_or_else(|| invalid("speakers is not a list"))?;
        let mut speakers = Vec::with_capacity(list.len());
        let mut ids = HashSet::new();
        for (index, item) in list.iter().enumerate() {
            let at = format!("speakers[{index}].");
            let object = item
                .as_object()
                .ok_or_else(|| invalid(format!("speakers[{index}] is not an object")))?;
            let speaker = Speaker {
                id: text_field(object, "id", &at)?,
                name: text_field(object, "name", &at)?,
                kind: text_field(object, "kind", &at)?,
                on: bool_field(object, "on", &at)?,
                volume: volume_field(object, "volume", &at)?,
                volume_step: step_field(object, &at)?,
                delay_ms: int_field(object, "delay_ms", &at)?,
                delay_min_ms: int_field(object, "delay_min_ms", &at)?,
                delay_max_ms: int_field(object, "delay_max_ms", &at)?,
                available: bool_field(object, "available", &at)?,
                notice: notice_field(object, &at)?,
            };
            if speaker.id.is_empty() {
                return Err(invalid(format!("{at}id is empty")));
            }
            if !ids.insert(speaker.id.clone()) {
                return Err(invalid(format!("{at}id is used twice")));
            }
            if speaker.delay_min_ms > speaker.delay_max_ms {
                return Err(invalid(format!("{at}delay_min_ms is above delay_max_ms")));
            }
            speakers.push(speaker);
        }
        let mut zones = Vec::new();
        let mut zone_ids = HashSet::new();
        let mut taken = HashSet::new();
        for (index, item) in optional_list(root, "zones")?.iter().enumerate() {
            let zone = zone_from(item, index, &speakers, &mut taken)?;
            if zone.id.is_empty() {
                return Err(invalid(format!("zones[{index}].id is empty")));
            }
            if !zone_ids.insert(zone.id.clone()) {
                return Err(invalid(format!("zones[{index}].id is used twice")));
            }
            zones.push(zone);
        }
        let mut presets = Vec::new();
        let mut preset_ids = HashSet::new();
        for (index, item) in optional_list(root, "presets")?.iter().enumerate() {
            let at = format!("presets[{index}].");
            let object = item
                .as_object()
                .ok_or_else(|| invalid(format!("presets[{index}] is not an object")))?;
            let preset = Preset {
                id: text_field(object, "id", &at)?,
                name: text_field(object, "name", &at)?,
                current: bool_field(object, "current", &at)?,
            };
            if preset.id.is_empty() {
                return Err(invalid(format!("{at}id is empty")));
            }
            if !preset_ids.insert(preset.id.clone()) {
                return Err(invalid(format!("{at}id is used twice")));
            }
            presets.push(preset);
        }
        // Only the first exact match in list order is current (S253).
        if presets.iter().filter(|preset| preset.current).count() > 1 {
            return Err(invalid("more than one preset is current"));
        }
        let calibrating = match root.get("calibrating") {
            None => false,
            Some(_) => bool_field(root, "calibrating", "")?,
        };
        let sources = sources_field(root)?;
        // The app may still name a source it no longer lists (an unplugged input).
        let source_id = match root.get("source_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id.clone()),
            Some(_) => return Err(invalid("source_id is not a string or null")),
        }
        .filter(|id| sources.iter().any(|source| &source.id == id));
        Ok(State {
            computer_name,
            master_volume,
            now_playing,
            speakers,
            zones,
            presets,
            calibrating,
            sources,
            source_id,
        })
    }

    /// The API's JSON of this snapshot as revision `revision`, on one line.
    pub fn to_json(&self, revision: u64) -> String {
        let speakers: Vec<Value> = self
            .speakers
            .iter()
            .map(|speaker| {
                json!({
                    "id": speaker.id,
                    "name": speaker.name,
                    "kind": speaker.kind,
                    "on": speaker.on,
                    "volume": speaker.volume,
                    "volume_step": speaker.volume_step,
                    "delay_ms": speaker.delay_ms,
                    "delay_min_ms": speaker.delay_min_ms,
                    "delay_max_ms": speaker.delay_max_ms,
                    "available": speaker.available,
                    "notice": speaker.notice,
                })
            })
            .collect();
        let now_playing = self.now_playing.as_ref().map(|playing| {
            json!({
                "title": playing.title,
                "artist": playing.artist,
                "source": playing.source,
                "playing": playing.playing,
            })
        });
        let zones: Vec<Value> = self
            .zones
            .iter()
            .map(|zone| {
                json!({
                    "id": zone.id,
                    "name": zone.name,
                    "volume": zone.volume,
                    "members": zone.members,
                    "counts": {
                        "on": zone.counts.on,
                        "total": zone.counts.total,
                        "waiting": zone.counts.waiting,
                        "all": zone.counts.all,
                    },
                })
            })
            .collect();
        let presets: Vec<Value> = self
            .presets
            .iter()
            .map(|preset| {
                json!({
                    "id": preset.id,
                    "name": preset.name,
                    "current": preset.current,
                })
            })
            .collect();
        let sources: Vec<Value> = self
            .sources
            .iter()
            .map(|source| {
                json!({
                    "id": source.id,
                    "name": source.name,
                    "kind": source.kind.as_str(),
                    "available": source.available,
                    "notice": source.notice,
                })
            })
            .collect();
        json!({
            "revision": revision,
            "computer_name": self.computer_name,
            "master": { "volume": self.master_volume },
            "now_playing": now_playing,
            "speakers": speakers,
            "zones": zones,
            "presets": presets,
            "calibrating": self.calibrating,
            "sources": sources,
            "source_id": self.source_id,
        })
        .to_string()
    }

    pub fn speaker(&self, id: &str) -> Option<&Speaker> {
        self.speakers.iter().find(|speaker| speaker.id == id)
    }

    /// Whether `change` can be carried out on `zone`: a toggle needs a
    /// member the page may drive, as one that would ask for a code stays off
    /// (S257).
    pub fn check_zone(&self, zone: &Zone, change: &ZoneChange) -> Result<(), ZoneProblem> {
        zone.check(change)?;
        let can_play = zone
            .members
            .iter()
            .any(|id| self.speaker(id).is_some_and(|speaker| speaker.available));
        if change.toggle && !can_play {
            return Err(ZoneProblem::Unavailable);
        }
        Ok(())
    }

    pub fn zone(&self, id: &str) -> Option<&Zone> {
        self.zones.iter().find(|zone| zone.id == id)
    }

    pub fn preset(&self, id: &str) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.id == id)
    }

    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|source| source.id == id)
    }
}

/// What one `POST /api/speakers/{id}` asks to change; at least one field is set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpeakerChange {
    pub on: Option<bool>,
    pub volume: Option<f64>,
    pub delay_ms: Option<i64>,
}

/// What one `POST /api/zones/{id}` asks to change; `toggle` or a volume is set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZoneChange {
    /// A click on the zone: the host's click plan switches its members (S246).
    pub toggle: bool,
    pub volume: Option<f64>,
}

/// A change the page asks for, as the app's callback receives it.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Speaker {
        id: String,
        change: SpeakerChange,
    },
    Master {
        volume: f64,
    },
    Zone {
        id: String,
        change: ZoneChange,
    },
    /// Recalls the preset (S252).
    Preset {
        id: String,
    },
    /// Capture this source, as a pick in the app's source picker would.
    Source {
        id: String,
    },
}

impl Command {
    /// `{"type":"speaker","id":…,"on"?,"volume"?,"delay_ms"?}`,
    /// `{"type":"master","volume":…}`, `{"type":"zone","id":…,"toggle"?,"volume"?}`,
    /// `{"type":"preset","id":…,"recall":true}` or
    /// `{"type":"source","id":…}`, with only the fields that were posted.
    pub fn to_json(&self) -> String {
        let mut object = Map::new();
        match self {
            Command::Speaker { id, change } => {
                object.insert("type".to_owned(), json!("speaker"));
                object.insert("id".to_owned(), json!(id));
                if let Some(on) = change.on {
                    object.insert("on".to_owned(), json!(on));
                }
                if let Some(volume) = change.volume {
                    object.insert("volume".to_owned(), json!(volume));
                }
                if let Some(delay) = change.delay_ms {
                    object.insert("delay_ms".to_owned(), json!(delay));
                }
            }
            Command::Master { volume } => {
                object.insert("type".to_owned(), json!("master"));
                object.insert("volume".to_owned(), json!(volume));
            }
            Command::Zone { id, change } => {
                object.insert("type".to_owned(), json!("zone"));
                object.insert("id".to_owned(), json!(id));
                if change.toggle {
                    object.insert("toggle".to_owned(), json!(true));
                }
                if let Some(volume) = change.volume {
                    object.insert("volume".to_owned(), json!(volume));
                }
            }
            Command::Preset { id } => {
                object.insert("type".to_owned(), json!("preset"));
                object.insert("id".to_owned(), json!(id));
                object.insert("recall".to_owned(), json!(true));
            }
            Command::Source { id } => {
                object.insert("type".to_owned(), json!("source"));
                object.insert("id".to_owned(), json!(id));
            }
        }
        Value::Object(object).to_string()
    }
}

/// Why a request body was refused before any range check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyError {
    /// Not a JSON object, or a field of the wrong type.
    Malformed,
    /// No field that changes anything.
    Empty,
}

/// Why a change is outside what the speaker or the master takes (422).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeError {
    Volume,
    Delay { min_ms: i64, max_ms: i64 },
}

fn body_object(body: &[u8]) -> Result<Map<String, Value>, BodyError> {
    match serde_json::from_slice(body) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err(BodyError::Malformed),
    }
}

fn optional<T>(
    object: &Map<String, Value>,
    name: &str,
    read: impl Fn(&Value) -> Option<T>,
) -> Result<Option<T>, BodyError> {
    match object.get(name) {
        None => Ok(None),
        Some(value) => read(value).map(Some).ok_or(BodyError::Malformed),
    }
}

/// Reads `{on?, volume?, delay_ms?}`; unknown fields are ignored.
pub fn speaker_change_from_body(body: &[u8]) -> Result<SpeakerChange, BodyError> {
    let object = body_object(body)?;
    let change = SpeakerChange {
        on: optional(&object, "on", Value::as_bool)?,
        volume: optional(&object, "volume", Value::as_f64)?,
        delay_ms: optional(&object, "delay_ms", Value::as_i64)?,
    };
    if change == SpeakerChange::default() {
        return Err(BodyError::Empty);
    }
    Ok(change)
}

/// Reads `{volume}`; unknown fields are ignored.
pub fn master_volume_from_body(body: &[u8]) -> Result<f64, BodyError> {
    let object = body_object(body)?;
    optional(&object, "volume", Value::as_f64)?.ok_or(BodyError::Empty)
}

/// Reads `{toggle?, volume?}`; `"toggle": false` asks for nothing, and
/// unknown fields are ignored.
pub fn zone_change_from_body(body: &[u8]) -> Result<ZoneChange, BodyError> {
    let object = body_object(body)?;
    let change = ZoneChange {
        toggle: optional(&object, "toggle", Value::as_bool)?.unwrap_or(false),
        volume: optional(&object, "volume", Value::as_f64)?,
    };
    if change == ZoneChange::default() {
        return Err(BodyError::Empty);
    }
    Ok(change)
}

/// Reads `{recall: true}`; `false` or no `recall` asks for nothing.
pub fn preset_recall_from_body(body: &[u8]) -> Result<(), BodyError> {
    let object = body_object(body)?;
    match optional(&object, "recall", Value::as_bool)? {
        Some(true) => Ok(()),
        _ => Err(BodyError::Empty),
    }
}

/// Reads `{id}`, a source's ID as a string; unknown fields are ignored.
pub fn source_id_from_body(body: &[u8]) -> Result<String, BodyError> {
    let object = body_object(body)?;
    optional(&object, "id", |value| value.as_str().map(str::to_owned))?.ok_or(BodyError::Empty)
}

/// Reads `{code}`, the code as a string.
pub fn pairing_code_from_body(body: &[u8]) -> Result<String, BodyError> {
    let object = body_object(body)?;
    optional(&object, "code", |value| value.as_str().map(str::to_owned))?
        .ok_or(BodyError::Malformed)
}

pub fn check_master_volume(volume: f64) -> Result<(), RangeError> {
    if volume_in_range(volume) {
        Ok(())
    } else {
        Err(RangeError::Volume)
    }
}

/// Why a zone change can't be carried out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZoneProblem {
    /// A toggle on a zone with no member present (409).
    Unavailable,
    Range(RangeError),
}

impl Zone {
    /// Whether `change` can be carried out: a toggle needs a present
    /// member, and a volume is 0 to 100, both ends included.
    pub fn check(&self, change: &ZoneChange) -> Result<(), ZoneProblem> {
        if change.toggle && self.counts.total == 0 {
            return Err(ZoneProblem::Unavailable);
        }
        if let Some(volume) = change.volume {
            check_master_volume(volume).map_err(ZoneProblem::Range)?;
        }
        Ok(())
    }
}

impl Speaker {
    /// Whether `change` stays inside this speaker's volume and delay ranges,
    /// both ends included.
    pub fn check(&self, change: &SpeakerChange) -> Result<(), RangeError> {
        if let Some(volume) = change.volume {
            check_master_volume(volume)?;
        }
        if let Some(delay) = change.delay_ms
            && !(self.delay_min_ms..=self.delay_max_ms).contains(&delay)
        {
            return Err(RangeError::Delay {
                min_ms: self.delay_min_ms,
                max_ms: self.delay_max_ms,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "computer_name": "Den Mac",
        "master": {"volume": 70},
        "now_playing": {"title": "Song", "artist": "Band", "source": "Music"},
        "speakers": [
            {"id": "kitchen", "name": "Kitchen", "kind": "AirPlay 2", "on": true, "volume": 40,
             "delay_ms": 120, "delay_min_ms": 0, "delay_max_ms": 3000, "available": true, "notice": null},
            {"id": "den", "name": "Den", "kind": "Google Cast", "on": false, "volume": 10,
             "delay_ms": 0, "delay_min_ms": -500, "delay_max_ms": 500, "available": false,
             "notice": "Not on the network"}
        ]
    }"#;

    #[test]
    fn a_snapshot_reads_and_writes_back_with_its_revision() {
        let state = State::from_json(SAMPLE).unwrap();
        assert_eq!(state.speakers.len(), 2);
        assert_eq!(
            state.speakers[1].notice.as_deref(),
            Some("Not on the network")
        );
        let written: Value = serde_json::from_str(&state.to_json(7)).unwrap();
        assert_eq!(written["revision"], 7);
        assert_eq!(written["computer_name"], "Den Mac");
        assert_eq!(written["master"]["volume"], 70.0);
        assert_eq!(written["now_playing"]["source"], "Music");
        assert_eq!(written["speakers"][0]["delay_max_ms"], 3000);
        assert_eq!(written["speakers"][0]["notice"], Value::Null);
        let again = State::from_json(&state.to_json(7)).unwrap();
        assert_eq!(again, state);
    }

    // covers: S343
    #[test]
    fn a_paused_track_says_so_and_a_snapshot_without_the_field_plays() {
        let with = |playing: &str| {
            SAMPLE.replacen(
                r#""source": "Music"}"#,
                &format!(r#""source": "Music", "playing": {playing}}}"#),
                1,
            )
        };
        let written = |json: &str| -> Value {
            serde_json::from_str(&State::from_json(json).unwrap().to_json(1)).unwrap()
        };
        assert_eq!(written(&with("false"))["now_playing"]["playing"], false);
        assert_eq!(written(SAMPLE)["now_playing"]["playing"], true);
        assert!(State::from_json(&with("0")).is_err());
    }

    const SOURCES: &str = r#""sources": [
            {"id": "system", "name": "All System Audio", "kind": "system"},
            {"id": "app:music", "name": "Music", "kind": "app", "available": true, "notice": null},
            {"id": "input:usb", "name": "USB Mic", "kind": "input", "available": false,
             "notice": "Allow microphone access"}
        ],"#;

    fn with_sources(source_id: &str) -> String {
        SAMPLE.replacen(
            "\"speakers\"",
            &format!("{SOURCES} \"source_id\": {source_id}, \"speakers\""),
            1,
        )
    }

    #[test]
    fn sources_keep_their_order_kind_and_the_current_one() {
        let state = State::from_json(&with_sources("\"app:music\"")).unwrap();
        let ids: Vec<&str> = state.sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["system", "app:music", "input:usb"]);
        assert_eq!(state.sources[0].kind, SourceKind::System);
        assert!(state.sources[0].available, "available when left out");
        assert!(!state.sources[2].available);
        assert_eq!(state.source_id.as_deref(), Some("app:music"));
        assert_eq!(state.source("input:usb").unwrap().kind, SourceKind::Input);
        let written: Value = serde_json::from_str(&state.to_json(3)).unwrap();
        assert_eq!(written["source_id"], "app:music");
        assert_eq!(written["sources"][1]["kind"], "app");
        assert_eq!(written["sources"][2]["notice"], "Allow microphone access");
        assert_eq!(State::from_json(&state.to_json(3)).unwrap(), state);
    }

    #[test]
    fn a_snapshot_without_sources_or_with_an_unlisted_current_one_has_none() {
        let bare = State::from_json(SAMPLE).unwrap();
        assert!(bare.sources.is_empty());
        assert_eq!(bare.source_id, None);
        let written: Value = serde_json::from_str(&bare.to_json(1)).unwrap();
        assert_eq!(written["sources"], json!([]));
        assert_eq!(written["source_id"], Value::Null);
        for current in ["\"input:gone\"", "null"] {
            let state = State::from_json(&with_sources(current)).unwrap();
            assert_eq!(state.source_id, None, "{current}");
        }
    }

    #[test]
    fn a_bad_source_list_is_refused() {
        for bad in [
            with_sources("3"),
            with_sources("\"system\"").replace("\"kind\": \"input\"", "\"kind\": \"tv\""),
            with_sources("\"system\"").replace("\"app:music\"", "\"system\""),
            with_sources("\"system\"").replace("\"id\": \"input:usb\"", "\"id\": \"\""),
            with_sources("\"system\"").replace("\"available\": false", "\"available\": 0"),
            SAMPLE.replacen("\"speakers\"", "\"sources\": {}, \"speakers\"", 1),
        ] {
            assert!(State::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_volume_step_defaults_to_1_and_takes_above_0_up_to_100() {
        let state = State::from_json(SAMPLE).unwrap();
        assert_eq!(state.speakers[1].volume_step, 1.0);
        let stepped = SAMPLE.replace("\"volume\": 10,", "\"volume\": 10, \"volume_step\": 5,");
        let state = State::from_json(&stepped).unwrap();
        assert_eq!(state.speakers[1].volume_step, 5.0);
        let written: Value = serde_json::from_str(&state.to_json(1)).unwrap();
        assert_eq!(written["speakers"][1]["volume_step"], 5.0);
        for bad in ["0", "-1", "101", "\"5\""] {
            let json = SAMPLE.replace(
                "\"volume\": 10,",
                &format!("\"volume\": 10, \"volume_step\": {bad},"),
            );
            assert!(State::from_json(&json).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_computer_name_is_optional_text_and_never_quoted_in_an_error() {
        let unnamed = State::from_json(r#"{"master": {"volume": 50}, "speakers": []}"#).unwrap();
        assert_eq!(unnamed.computer_name, "");
        let written: Value = serde_json::from_str(&unnamed.to_json(1)).unwrap();
        assert_eq!(written["computer_name"], "");
        let error = State::from_json(
            r#"{"computer_name": ["Den Mac"], "master": {"volume": 50}, "speakers": []}"#,
        )
        .unwrap_err();
        assert!(!error.to_string().contains("Den Mac"), "{error}");
        let bad_volume = SAMPLE.replace("\"volume\": 70", "\"volume\": 700");
        let error = State::from_json(&bad_volume).unwrap_err();
        assert!(!error.to_string().contains("Den Mac"), "{error}");
    }

    #[test]
    fn a_bad_snapshot_is_refused() {
        for bad in [
            "[]",
            r#"{"master": {"volume": 101}, "speakers": []}"#,
            r#"{"master": {"volume": 50}}"#,
            r#"{"master": {"volume": 50}, "now_playing": 3, "speakers": []}"#,
            r#"{"master": {"volume": 50}, "speakers": [{"id": ""}]}"#,
        ] {
            assert!(State::from_json(bad).is_err(), "{bad}");
        }
        let twice = SAMPLE.replace("\"den\"", "\"kitchen\"");
        assert!(State::from_json(&twice).is_err());
        let upside_down = SAMPLE.replace("\"delay_min_ms\": -500", "\"delay_min_ms\": 900");
        assert!(State::from_json(&upside_down).is_err());
    }

    #[test]
    fn bodies_read_only_the_fields_they_know() {
        assert_eq!(
            speaker_change_from_body(br#"{"on": false, "extra": 1}"#),
            Ok(SpeakerChange {
                on: Some(false),
                ..SpeakerChange::default()
            })
        );
        assert_eq!(
            speaker_change_from_body(br#"{"delay_ms": 1.5}"#),
            Err(BodyError::Malformed)
        );
        assert_eq!(
            speaker_change_from_body(br#"{"on": "yes"}"#),
            Err(BodyError::Malformed)
        );
        assert_eq!(speaker_change_from_body(b"{}"), Err(BodyError::Empty));
        assert_eq!(
            speaker_change_from_body(b"not json"),
            Err(BodyError::Malformed)
        );
        assert_eq!(master_volume_from_body(br#"{"volume": 12.5}"#), Ok(12.5));
        assert_eq!(master_volume_from_body(b"{}"), Err(BodyError::Empty));
        assert_eq!(
            source_id_from_body(br#"{"id": "app:music", "x": 1}"#),
            Ok("app:music".to_owned())
        );
        assert_eq!(source_id_from_body(b"{}"), Err(BodyError::Empty));
        assert_eq!(
            source_id_from_body(br#"{"id": 7}"#),
            Err(BodyError::Malformed)
        );
        assert_eq!(
            pairing_code_from_body(br#"{"code": "012345"}"#),
            Ok("012345".to_owned())
        );
        assert_eq!(
            pairing_code_from_body(br#"{"code": 12345}"#),
            Err(BodyError::Malformed)
        );
    }

    #[test]
    fn ranges_include_both_ends() {
        let state = State::from_json(SAMPLE).unwrap();
        let den = state.speaker("den").unwrap();
        let delay = |ms| SpeakerChange {
            delay_ms: Some(ms),
            ..SpeakerChange::default()
        };
        assert_eq!(den.check(&delay(-500)), Ok(()));
        assert_eq!(den.check(&delay(500)), Ok(()));
        let outside = Err(RangeError::Delay {
            min_ms: -500,
            max_ms: 500,
        });
        assert_eq!(den.check(&delay(-501)), outside);
        assert_eq!(den.check(&delay(501)), outside);
        let volume = |v| SpeakerChange {
            volume: Some(v),
            ..SpeakerChange::default()
        };
        assert_eq!(den.check(&volume(0.0)), Ok(()));
        assert_eq!(den.check(&volume(100.0)), Ok(()));
        assert_eq!(den.check(&volume(100.5)), Err(RangeError::Volume));
        assert_eq!(check_master_volume(-0.1), Err(RangeError::Volume));
    }

    #[test]
    fn a_command_carries_only_what_was_posted() {
        let command = Command::Speaker {
            id: "den".to_owned(),
            change: SpeakerChange {
                volume: Some(30.0),
                ..SpeakerChange::default()
            },
        };
        let value: Value = serde_json::from_str(&command.to_json()).unwrap();
        assert_eq!(
            value,
            json!({"type": "speaker", "id": "den", "volume": 30.0})
        );
        let master: Value =
            serde_json::from_str(&Command::Master { volume: 5.0 }.to_json()).unwrap();
        assert_eq!(master, json!({"type": "master", "volume": 5.0}));
        let zone = Command::Zone {
            id: "z1".to_owned(),
            change: ZoneChange {
                toggle: true,
                volume: None,
            },
        };
        let value: Value = serde_json::from_str(&zone.to_json()).unwrap();
        assert_eq!(value, json!({"type": "zone", "id": "z1", "toggle": true}));
        let recall: Value = serde_json::from_str(
            &Command::Preset {
                id: "p1".to_owned(),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(
            recall,
            json!({"type": "preset", "id": "p1", "recall": true})
        );
        let source: Value = serde_json::from_str(
            &Command::Source {
                id: "input:usb".to_owned(),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(source, json!({"type": "source", "id": "input:usb"}));
    }

    fn with_zones_and_presets(zones: &str, presets: &str) -> String {
        SAMPLE.replacen(
            "\"speakers\": [",
            &format!("\"zones\": {zones}, \"presets\": {presets}, \"speakers\": ["),
            1,
        )
    }

    // covers: S256
    #[test]
    fn the_snapshot_lists_zones_with_members_and_counts() {
        let empty = State::from_json(SAMPLE).unwrap();
        assert!(empty.zones.is_empty() && empty.presets.is_empty() && !empty.calibrating);
        let written: Value = serde_json::from_str(&empty.to_json(1)).unwrap();
        assert_eq!(written["zones"], json!([]));
        assert_eq!(written["presets"], json!([]));
        assert_eq!(written["calibrating"], false);

        let json = with_zones_and_presets(
            r#"[{"id": "z1", "name": "Downstairs", "volume": 60, "members": ["den", "kitchen"],
                 "counts": {"on": 1, "total": 2, "waiting": 0}},
                {"id": "z2", "name": "Empty", "volume": 100, "members": [],
                 "counts": {"on": 0, "total": 0, "waiting": 0}}]"#,
            "[]",
        );
        let state = State::from_json(&json).unwrap();
        assert_eq!(state.zones[0].members, ["den", "kitchen"]);
        assert_eq!(
            state.zones[0].counts,
            ZoneCounts {
                on: 1,
                total: 2,
                waiting: 0,
                all: 2
            }
        );
        let written: Value = serde_json::from_str(&state.to_json(3)).unwrap();
        assert_eq!(written["zones"][0]["members"], json!(["den", "kitchen"]));
        assert_eq!(written["zones"][0]["counts"]["total"], 2);
        assert_eq!(written["zones"][1]["volume"], 100.0);
        assert_eq!(State::from_json(&state.to_json(3)).unwrap(), state);

        let zone = |members: &str, counts: &str| {
            with_zones_and_presets(
                &format!(
                    r#"[{{"id": "z1", "name": "A", "volume": 50, "members": {members}, "counts": {counts}}}]"#
                ),
                "[]",
            )
        };
        let fine = r#"{"on": 0, "total": 1, "waiting": 0}"#;
        for bad in [
            zone(r#"["gone"]"#, fine),
            zone(
                r#"["den", "den"]"#,
                r#"{"on": 0, "total": 2, "waiting": 0}"#,
            ),
            zone(r#"["den"]"#, r#"{"on": 2, "total": 1, "waiting": 0}"#),
            zone(r#"["den"]"#, r#"{"on": 0, "total": 1, "waiting": 1}"#),
            zone(r#"["den"]"#, r#"{"on": -1, "total": 1, "waiting": 0}"#),
            zone(r#"["den"]"#, r#"{"on": 0, "total": 1}"#),
            zone("\"den\"", fine),
            with_zones_and_presets("{}", "[]"),
            with_zones_and_presets(
                r#"[{"id": "", "name": "A", "volume": 50, "members": [], "counts": {"on": 0, "total": 0, "waiting": 0}}]"#,
                "[]",
            ),
            with_zones_and_presets(
                r#"[{"id": "a", "name": "A", "volume": 50, "members": ["den"], "counts": {"on": 0, "total": 1, "waiting": 0}},
                    {"id": "b", "name": "B", "volume": 50, "members": ["den"], "counts": {"on": 0, "total": 1, "waiting": 0}}]"#,
                "[]",
            ),
        ] {
            assert!(State::from_json(&bad).is_err(), "{bad}");
        }
    }

    // covers: S256
    #[test]
    fn a_zone_counts_its_absent_members_in_all_and_a_snapshot_without_it_takes_total() {
        let json = with_zones_and_presets(
            r#"[{"id": "z1", "name": "Gone", "volume": 60, "members": [],
                 "counts": {"on": 0, "total": 0, "waiting": 0, "all": 3}},
                {"id": "z2", "name": "Older", "volume": 60, "members": ["den"],
                 "counts": {"on": 0, "total": 1, "waiting": 0}}]"#,
            "[]",
        );
        let state = State::from_json(&json).unwrap();
        let written: Value = serde_json::from_str(&state.to_json(1)).unwrap();
        assert_eq!(written["zones"][0]["counts"]["all"], 3);
        assert_eq!(written["zones"][1]["counts"]["all"], 1);
        assert_eq!(State::from_json(&state.to_json(1)).unwrap(), state);
        let fewer = json.replacen(r#""waiting": 0}}]"#, r#""waiting": 0, "all": 0}}]"#, 1);
        assert!(State::from_json(&fewer).is_err(), "all below total");
    }

    // covers: S256, S253
    #[test]
    fn the_snapshot_lists_presets_with_current() {
        let json = with_zones_and_presets(
            "[]",
            r#"[{"id": "p1", "name": "Evening", "current": false},
                {"id": "p2", "name": "Party", "current": true}]"#,
        )
        .replacen(
            "\"computer_name\"",
            "\"calibrating\": true, \"computer_name\"",
            1,
        );
        let state = State::from_json(&json).unwrap();
        assert!(state.calibrating);
        assert_eq!(state.preset("p2").map(|preset| preset.current), Some(true));
        let written: Value = serde_json::from_str(&state.to_json(2)).unwrap();
        assert_eq!(
            written["presets"],
            json!([
                {"id": "p1", "name": "Evening", "current": false},
                {"id": "p2", "name": "Party", "current": true}
            ])
        );
        assert_eq!(written["calibrating"], true);
        for presets in [
            r#"[{"id": "p1", "name": "A", "current": true}, {"id": "p2", "name": "B", "current": true}]"#,
            r#"[{"id": "p1", "name": "A", "current": false}, {"id": "p1", "name": "B", "current": false}]"#,
            r#"[{"id": "", "name": "A", "current": false}]"#,
            r#"[{"id": "p1", "name": "A"}]"#,
        ] {
            let json = with_zones_and_presets("[]", presets);
            assert!(State::from_json(&json).is_err(), "{presets}");
        }
        let flag = SAMPLE.replacen(
            "\"computer_name\"",
            "\"calibrating\": 1, \"computer_name\"",
            1,
        );
        assert!(State::from_json(&flag).is_err());
    }

    // covers: S257
    #[test]
    fn zone_and_preset_bodies_read_only_the_fields_they_know() {
        assert_eq!(
            zone_change_from_body(br#"{"toggle": true, "extra": 1}"#),
            Ok(ZoneChange {
                toggle: true,
                volume: None
            })
        );
        assert_eq!(
            zone_change_from_body(br#"{"volume": 20}"#),
            Ok(ZoneChange {
                toggle: false,
                volume: Some(20.0)
            })
        );
        assert_eq!(
            zone_change_from_body(br#"{"toggle": false}"#),
            Err(BodyError::Empty)
        );
        assert_eq!(
            zone_change_from_body(br#"{"toggle": "yes"}"#),
            Err(BodyError::Malformed)
        );
        assert_eq!(preset_recall_from_body(br#"{"recall": true}"#), Ok(()));
        assert_eq!(
            preset_recall_from_body(br#"{"recall": false}"#),
            Err(BodyError::Empty)
        );
        assert_eq!(preset_recall_from_body(b"{}"), Err(BodyError::Empty));
        assert_eq!(
            preset_recall_from_body(br#"{"recall": 1}"#),
            Err(BodyError::Malformed)
        );
        let zone = Zone {
            id: "z".to_owned(),
            name: "Z".to_owned(),
            volume: 50.0,
            members: Vec::new(),
            counts: ZoneCounts::default(),
        };
        let toggle = ZoneChange {
            toggle: true,
            volume: None,
        };
        assert_eq!(zone.check(&toggle), Err(ZoneProblem::Unavailable));
        let louder = ZoneChange {
            toggle: false,
            volume: Some(100.5),
        };
        assert_eq!(
            zone.check(&louder),
            Err(ZoneProblem::Range(RangeError::Volume))
        );
    }
}
