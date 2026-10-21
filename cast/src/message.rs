//! The `CastMessage` envelope (C24), the endpoint and namespace names (C26,
//! C27) and the JSON payloads they carry (C28).

use serde_json::{Map, Value};

/// The device platform's endpoint (C26).
pub const PLATFORM: &str = "receiver-0";
/// The sender's own endpoint id in every message it sends (C26).
pub const SENDER: &str = "sender-0";
/// A destination that reaches every sender connected to an endpoint (C26).
pub const BROADCAST: &str = "*";

/// The Default Media Receiver's app id (C39).
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";

/// Virtual connections (C32).
pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
/// Keep-alive (C34 to C36).
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
/// Platform status, apps and device volume (C37 to C44).
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
/// Playback (section 5).
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
/// Device authentication, which this sender never uses (C30, C31).
pub const NS_DEVICE_AUTH: &str = "urn:x-cast:com.google.cast.tp.deviceauth";

/// The namespace of a message, as far as the sender cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace {
    Connection,
    Heartbeat,
    Receiver,
    Media,
    DeviceAuth,
}

impl Namespace {
    /// The namespace named `name`, or `None` for one the sender drops (C27).
    pub fn from_name(name: &str) -> Option<Namespace> {
        match name {
            NS_CONNECTION => Some(Namespace::Connection),
            NS_HEARTBEAT => Some(Namespace::Heartbeat),
            NS_RECEIVER => Some(Namespace::Receiver),
            NS_MEDIA => Some(Namespace::Media),
            NS_DEVICE_AUTH => Some(Namespace::DeviceAuth),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Namespace::Connection => NS_CONNECTION,
            Namespace::Heartbeat => NS_HEARTBEAT,
            Namespace::Receiver => NS_RECEIVER,
            Namespace::Media => NS_MEDIA,
            Namespace::DeviceAuth => NS_DEVICE_AUTH,
        }
    }
}

/// `CastMessage.protocol_version` (C24).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum ProtocolVersion {
    Castv210 = 0,
}

/// `CastMessage.payload_type` (C24).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum PayloadType {
    String = 0,
    Binary = 1,
}

/// One message on the channel, in the proto2 layout of C24.
#[derive(Clone, PartialEq, prost::Message)]
pub struct CastMessage {
    #[prost(enumeration = "ProtocolVersion", required, tag = "1")]
    pub protocol_version: i32,
    #[prost(string, required, tag = "2")]
    pub source_id: String,
    #[prost(string, required, tag = "3")]
    pub destination_id: String,
    #[prost(string, required, tag = "4")]
    pub namespace: String,
    #[prost(enumeration = "PayloadType", required, tag = "5")]
    pub payload_type: i32,
    #[prost(string, optional, tag = "6")]
    pub payload_utf8: Option<String>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub payload_binary: Option<Vec<u8>>,
}

impl CastMessage {
    /// A message from the sender with a JSON payload (C26, C28).
    pub fn json(destination: &str, namespace: Namespace, payload: &Value) -> CastMessage {
        CastMessage {
            protocol_version: ProtocolVersion::Castv210 as i32,
            source_id: SENDER.to_owned(),
            destination_id: destination.to_owned(),
            namespace: namespace.name().to_owned(),
            payload_type: PayloadType::String as i32,
            payload_utf8: Some(payload.to_string()),
            payload_binary: None,
        }
    }

    /// The payload as a JSON object, if it is one (C28).
    pub fn json_payload(&self) -> Result<Map<String, Value>, PayloadError> {
        if self.payload_type != PayloadType::String as i32 {
            return Err(PayloadError::NotText);
        }
        let text = self.payload_utf8.as_deref().ok_or(PayloadError::NotText)?;
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(object)) => Ok(object),
            Ok(_) => Err(PayloadError::NotAnObject),
            Err(_) => Err(PayloadError::InvalidJson),
        }
    }
}

/// Why a payload was not a usable JSON object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadError {
    NotText,
    InvalidJson,
    NotAnObject,
}

/// The `type` of a JSON payload.
pub fn message_type(payload: &Map<String, Value>) -> Option<&str> {
    payload.get("type").and_then(Value::as_str)
}

/// The `requestId` of a JSON payload; 0 when it has none (C29).
pub fn request_id(payload: &Map<String, Value>) -> u32 {
    payload
        .get("requestId")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .unwrap_or(0)
}
