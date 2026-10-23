//! `RECEIVER_STATUS` as the sender reads it (C37).

use serde_json::{Map, Value};

use crate::message::DEFAULT_MEDIA_RECEIVER;

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
    pub level: f64,
    pub muted: bool,
    pub control_type: ControlType,
    pub step_interval: Option<f64>,
}

impl Default for DeviceVolume {
    fn default() -> DeviceVolume {
        DeviceVolume {
            level: 1.0,
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

/// The level the host sends for its master and the speaker's own level,
/// all 0.0 to 1.0 (C104). Anything not finite counts as 0.
pub fn device_level(master: f64, own: f64) -> f64 {
    unit(master) * unit(own)
}

/// The own level a reported level means at `master` (C104): level ÷
/// master, clamped; `own` as it is at master 0 or with a value that isn't
/// finite.
pub fn own_level(reported: f64, master: f64, own: f64) -> f64 {
    if !reported.is_finite() || !master.is_finite() || master <= 0.0 {
        return own;
    }
    unit(unit(reported) / master.min(1.0))
}

/// Whether a reported level is the device's answer to `sent` (C104).
pub fn answers_level(sent: f64, reported: f64) -> bool {
    (sent - reported).abs() <= ANSWER_TOLERANCE
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
    })
}
