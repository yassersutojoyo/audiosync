//! `RECEIVER_STATUS` as the sender reads it (C37).

use serde_json::{Map, Value};

use crate::message::DEFAULT_MEDIA_RECEIVER;

/// The Backdrop app, a device's idle screen (C81).
const BACKDROP: &str = "E8C28D3C";

/// `volume.controlType` (C37, C68).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlType {
    Attenuation,
    Fixed,
    Master,
    /// A value no source names, kept as sent.
    Other(String),
}

/// The device volume of a status (C37).
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceVolume {
    /// Meaningful only with `has_level`.
    pub level: f64,
    /// Whether a status gave the level; the first one may leave it out (C105).
    pub has_level: bool,
    pub muted: bool,
    pub control_type: ControlType,
    pub step_interval: Option<f64>,
}

impl Default for DeviceVolume {
    fn default() -> DeviceVolume {
        DeviceVolume {
            level: 1.0,
            has_level: false,
            muted: false,
            control_type: ControlType::Attenuation,
            step_interval: None,
        }
    }
}

/// How far a reported level may be from a level the host sent and still
/// answer it: devices report 32-bit floats.
const ANSWER_TOLERANCE: f64 = 0.005;

fn unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The zone level of a speaker in no zone (C104).
pub const NO_ZONE_LEVEL: f64 = 1.0;

/// The level the host sends for its master, the speaker's zone
/// ([`NO_ZONE_LEVEL`] outside a zone) and its own level, all 0.0 to 1.0:
/// master × zone × own (C104). Anything not finite counts as 0.
pub fn device_level(master: f64, zone: f64, own: f64) -> f64 {
    unit(master) * unit(zone) * unit(own)
}

/// The own level a reported level means at `master` and `zone` (C104):
/// level ÷ (master × zone), clamped; `own` as it is when master × zone is 0
/// or a value isn't finite.
pub fn own_level(reported: f64, master: f64, zone: f64, own: f64) -> f64 {
    let stage = |value: f64| value.is_finite() && value > 0.0;
    if !reported.is_finite() || !stage(master) || !stage(zone) {
        return own;
    }
    unit(unit(reported) / (master.min(1.0) * zone.min(1.0)))
}

/// The own level a session starts from (C105): the one kept, whatever the
/// first status reports; without one, that status's level read as
/// [`own_level`] reads it.
pub fn start_own_level(master: f64, zone: f64, own: Option<f64>, reported: f64) -> f64 {
    match own.filter(|own| own.is_finite()) {
        Some(own) => own.clamp(0.0, 1.0),
        None => own_level(reported, master, zone, unit(reported)),
    }
}

/// Whether a reported level is the device's answer to `sent` (C104).
pub fn answers_level(sent: f64, reported: f64) -> bool {
    (sent - reported).abs() <= ANSWER_TOLERANCE
}

/// [`answers_level`], or within half the `stepInterval` of a device that
/// rounds to its step (C70).
pub fn answers_volume(sent: f64, volume: &DeviceVolume) -> bool {
    answers_level(sent, volume.level)
        || volume
            .step_interval
            .filter(|step| step.is_finite())
            .is_some_and(|step| (sent - volume.level).abs() < step / 2.0)
}

/// One entry of `applications` (C37).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Application {
    pub app_id: String,
    pub display_name: Option<String>,
    pub session_id: Option<String>,
    pub transport_id: Option<String>,
    pub namespaces: Vec<String>,
    pub status_text: Option<String>,
    pub is_idle_screen: bool,
}

impl Application {
    /// The app's `sessionId` and `transportId`, when it has both.
    pub fn session(&self) -> Option<AppSession> {
        Some(AppSession {
            session_id: self.session_id.clone()?,
            transport_id: self.transport_id.clone()?,
        })
    }
}

/// A running app the sender talks to (C42).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSession {
    pub session_id: String,
    pub transport_id: String,
}

/// The `status` object of a `RECEIVER_STATUS` (C37).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReceiverStatus {
    pub volume: DeviceVolume,
    pub applications: Vec<Application>,
    /// Fields the sender doesn't use, kept for the log.
    pub other: Vec<(String, String)>,
}

impl ReceiverStatus {
    /// Reads the `status` field of a `RECEIVER_STATUS` payload. A missing or
    /// malformed part reads as its default.
    pub fn from_payload(payload: &Map<String, Value>) -> ReceiverStatus {
        let Some(status) = payload.get("status").and_then(Value::as_object) else {
            return ReceiverStatus::default();
        };
        let mut volume = DeviceVolume::default();
        if let Some(object) = status.get("volume").and_then(Value::as_object) {
            if let Some(level) = object.get("level").and_then(Value::as_f64) {
                volume.level = level;
                volume.has_level = true;
            }
            if let Some(muted) = object.get("muted").and_then(Value::as_bool) {
                volume.muted = muted;
            }
            if let Some(kind) = object.get("controlType").and_then(Value::as_str) {
                volume.control_type = match kind {
                    "attenuation" => ControlType::Attenuation,
                    "fixed" => ControlType::Fixed,
                    "master" => ControlType::Master,
                    other => ControlType::Other(other.to_owned()),
                };
            }
            volume.step_interval = object.get("stepInterval").and_then(Value::as_f64);
        }
        let applications = status
            .get("applications")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(application).collect())
            .unwrap_or_default();
        let other = status
            .iter()
            .filter(|(key, _)| key.as_str() != "volume" && key.as_str() != "applications")
            .map(|(key, value)| (key.clone(), value.to_string()))
            .collect();
        ReceiverStatus {
            volume,
            applications,
            other,
        }
    }

    /// The running Default Media Receiver, if the status lists one (C40).
    pub fn default_media_receiver(&self) -> Option<&Application> {
        self.applications
            .iter()
            .find(|app| app.app_id == DEFAULT_MEDIA_RECEIVER)
    }

    /// Whether an app with this `sessionId` still runs (C43).
    pub fn has_session(&self, session_id: &str) -> bool {
        self.applications
            .iter()
            .any(|app| app.session_id.as_deref() == Some(session_id))
    }

    /// Whether the device runs nothing but its idle screen (C81).
    pub fn is_idle(&self) -> bool {
        self.applications
            .iter()
            .all(|app| app.is_idle_screen || app.app_id == BACKDROP)
    }
}

fn application(value: &Value) -> Option<Application> {
    let object = value.as_object()?;
    let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(Application {
        app_id: text("appId")?,
        display_name: text("displayName"),
        session_id: text("sessionId"),
        transport_id: text("transportId"),
        namespaces: object
            .get("namespaces")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        status_text: text("statusText"),
        is_idle_screen: object
            .get("isIdleScreen")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}
