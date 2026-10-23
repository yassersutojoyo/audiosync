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
            })
        });
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
            "sources": sources,
            "source_id": self.source_id,
        })
        .to_string()
    }

    pub fn speaker(&self, id: &str) -> Option<&Speaker> {
        self.speakers.iter().find(|speaker| speaker.id == id)
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
    /// Capture this source, as a pick in the app's source picker would.
    Source {
        id: String,
    },
}

impl Command {
    /// `{"type":"speaker","id":…,"on"?,"volume"?,"delay_ms"?}`,
    /// `{"type":"master","volume":…}` or `{"type":"source","id":…}`, with
    /// only the fields that were posted.
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
        let source: Value = serde_json::from_str(
            &Command::Source {
                id: "input:usb".to_owned(),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(source, json!({"type": "source", "id": "input:usb"}));
    }
}
