//! The media namespace as the sender uses it (section 5): the `LOAD`
//! payload, `MEDIA_STATUS` and the media errors (C83).

use std::fmt;

use serde_json::{Map, Value, json};

use crate::error::EndReason;

/// The longest `contentId` the device takes, in bytes (C47).
pub const MAX_CONTENT_ID: usize = 1024;

/// Track metadata for the `LOAD` (C53). Unknown fields stay `None`; none
/// is sent as a placeholder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_name: Option<String>,
    /// Artwork URLs, served by the stream server.
    pub images: Vec<String>,
}

impl Metadata {
    /// The `metadata` object: `metadataType` 3 (music track) and only the
    /// fields with a value (C53).
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("metadataType".to_owned(), json!(3));
        let fields = [
            ("title", &self.title),
            ("artist", &self.artist),
            ("albumName", &self.album_name),
        ];
        for (key, value) in fields {
            if let Some(value) = value.as_deref().filter(|value| !value.trim().is_empty()) {
                object.insert(key.to_owned(), json!(value));
            }
        }
        let images: Vec<Value> = self
            .images
            .iter()
            .filter(|url| !url.is_empty())
            .map(|url| json!({"url": url}))
            .collect();
        if !images.is_empty() {
            object.insert("images".to_owned(), Value::Array(images));
        }
        Value::Object(object)
    }
}

/// What one `LOAD` plays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadRequest {
    /// The stream URL (C55 to C57).
    pub url: String,
    /// The MIME type of the stream (C63).
    pub content_type: String,
    pub metadata: Metadata,
}

impl LoadRequest {
    /// The `LOAD` payload without its `requestId` (C46 to C48): the URL in
    /// `contentId` and no `contentUrl`, `LIVE`, `duration` -1, `autoplay`
    /// true and no `currentTime`.
    pub fn payload(&self) -> Value {
        json!({
            "type": "LOAD",
            "autoplay": true,
            "media": {
                "contentId": self.url,
                "contentType": self.content_type,
                "streamType": "LIVE",
                "duration": -1,
                "metadata": self.metadata.to_json(),
            },
        })
    }
}

/// `playerState` (C51).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerState {
    Idle,
    Playing,
    Buffering,
    Paused,
    Other(String),
}

impl PlayerState {
    fn parse(text: &str) -> PlayerState {
        match text {
            "IDLE" => PlayerState::Idle,
            "PLAYING" => PlayerState::Playing,
            "BUFFERING" => PlayerState::Buffering,
            "PAUSED" => PlayerState::Paused,
            other => PlayerState::Other(other.to_owned()),
        }
    }
}

/// `idleReason` (C51).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdleReason {
    Cancelled,
    Interrupted,
    Finished,
    Error,
    Other(String),
}

impl IdleReason {
    fn parse(text: &str) -> IdleReason {
        match text {
            "CANCELLED" => IdleReason::Cancelled,
            "INTERRUPTED" => IdleReason::Interrupted,
            "FINISHED" => IdleReason::Finished,
            "ERROR" => IdleReason::Error,
            other => IdleReason::Other(other.to_owned()),
        }
    }
}

impl fmt::Display for IdleReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdleReason::Cancelled => f.write_str("cancelled"),
            IdleReason::Interrupted => f.write_str("interrupted"),
            IdleReason::Finished => f.write_str("finished"),
            IdleReason::Error => f.write_str("a playback error"),
            IdleReason::Other(other) => f.write_str(other),
        }
    }
}

/// One entry of a `MEDIA_STATUS` (C50, C51, C73).
#[derive(Clone, Debug, PartialEq)]
pub struct MediaStatus {
    pub media_session_id: Option<i64>,
    pub player_state: PlayerState,
    pub idle_reason: Option<IdleReason>,
    /// The device's position, in seconds (C73).
    pub current_time: Option<f64>,
    /// The `contentId` of the media, when the status names it.
    pub content_id: Option<String>,
}

impl MediaStatus {
    /// The entries of a `MEDIA_STATUS` payload. `status` is a list; a
    /// single object is read as a list of one.
    pub fn list_from_payload(payload: &Map<String, Value>) -> Vec<MediaStatus> {
        match payload.get("status") {
            Some(Value::Array(list)) => list.iter().filter_map(MediaStatus::parse).collect(),
            Some(object @ Value::Object(_)) => MediaStatus::parse(object).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn parse(value: &Value) -> Option<MediaStatus> {
        let object = value.as_object()?;
        Some(MediaStatus {
            media_session_id: object.get("mediaSessionId").and_then(Value::as_i64),
            player_state: object
                .get("playerState")
                .and_then(Value::as_str)
                .map(PlayerState::parse)
                .unwrap_or(PlayerState::Idle),
            idle_reason: object
                .get("idleReason")
                .and_then(Value::as_str)
                .map(IdleReason::parse),
            current_time: object.get("currentTime").and_then(Value::as_f64),
            content_id: object
                .get("media")
                .and_then(|media| media.get("contentId"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

/// Why a `LOAD`, or the playback it started, failed (C49, C83 to C86).
/// Each reads as a reason the user can understand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    /// No app to load into (C42).
    NotRunning,
    /// A `LOAD` is still outstanding (C49).
    LoadPending,
    /// The stream URL is over [`MAX_CONTENT_ID`] bytes (C47).
    UrlTooLong,
    /// `LOAD_FAILED`, with any `detailedErrorCode` for the log (C83).
    LoadFailed { detailed_error_code: Option<i64> },
    /// `LOAD_CANCELLED`: a second `LOAD` overlapped, a fault in the sender
    /// (C49, C83).
    LoadCancelled,
    /// `INVALID_PLAYER_STATE` (C83).
    InvalidPlayerState,
    /// `INVALID_REQUEST`, a fault in the sender; `request` names the
    /// request it answered (C83).
    InvalidRequest {
        reason: String,
        request: &'static str,
    },
    /// No reply, or no `PLAYING`, in time (C49).
    Timeout,
    /// The player went `IDLE` before it played (C84) or while playing
    /// (C86).
    Idle(IdleReason),
    /// The device never fetched the stream (C56, C85).
    NoFetch,
    /// The channel ended.
    Ended(EndReason),
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::NotRunning => f.write_str("the media player is not running"),
            MediaError::LoadPending => f.write_str("the stream is still starting"),
            MediaError::UrlTooLong => f.write_str("the stream address is too long"),
            MediaError::LoadFailed {
                detailed_error_code: Some(code),
            } => write!(f, "the device could not play the stream (error {code})"),
            MediaError::LoadFailed {
                detailed_error_code: None,
            } => f.write_str("the device could not play the stream"),
            MediaError::LoadCancelled => {
                f.write_str("the device cancelled the stream because another one started")
            }
            MediaError::InvalidPlayerState => {
                f.write_str("the device's player was not in a state to play the stream")
            }
            MediaError::InvalidRequest { reason, request } => {
                write!(f, "the device rejected the {request} request ({reason})")
            }
            MediaError::Timeout => f.write_str("the device did not start playing in time"),
            MediaError::Idle(reason) => write!(f, "the device stopped playing ({reason})"),
            MediaError::NoFetch => f.write_str(
                "the device could not reach this computer; a firewall may be blocking the stream",
            ),
            MediaError::Ended(reason) => write!(f, "the connection ended: {reason}"),
        }
    }
}

impl std::error::Error for MediaError {}

/// Why a volume change was not sent (C67, C68).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeError {
    /// No status yet, or the channel is over.
    NotReady,
    /// `controlType` is `fixed`: the device's volume is locked (C68).
    Fixed,
    /// Not a number.
    Invalid,
    /// The device reports no `stepInterval` to step by (C69).
    NoStep,
}

impl fmt::Display for VolumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VolumeError::NotReady => f.write_str("the device is not ready"),
            VolumeError::Fixed => f.write_str("the device's volume can't be changed"),
            VolumeError::Invalid => f.write_str("not a volume level"),
            VolumeError::NoStep => f.write_str("the device reports no volume step"),
        }
    }
}

impl std::error::Error for VolumeError {}
