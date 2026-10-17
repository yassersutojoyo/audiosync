//! Capture sources as the host stores them (S12) and the microphone permission
//! an input source needs (S15).

/// What the engine captures (S11).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AudioSource {
    /// Every process's audio except AudioSync's own.
    System,
    /// One application's audio, by bundle ID.
    App {
        /// The application's bundle ID.
        bundle_id: String,
    },
    /// An input device, by device UID.
    Input {
        /// The device's UID.
        uid: String,
    },
}

/// Prefix of a stored input source.
const INPUT_PREFIX: &str = "input:";

impl AudioSource {
    /// Reads the stored form (S12): `""` is the system source, `"input:<uid>"`
    /// an input device, anything else an app bundle ID; a missing value means
    /// the system source.
    pub fn from_stored(value: Option<&str>) -> AudioSource {
        match value {
            None | Some("") => AudioSource::System,
            Some(value) => match value.strip_prefix(INPUT_PREFIX) {
                Some(uid) => AudioSource::Input {
                    uid: uid.to_owned(),
                },
                None => AudioSource::App {
                    bundle_id: value.to_owned(),
                },
            },
        }
    }

    /// The stored form (S12). An app with an empty bundle ID is stored as the
    /// system source.
    pub fn stored(&self) -> String {
        match self {
            AudioSource::System => String::new(),
            AudioSource::App { bundle_id } => bundle_id.clone(),
            AudioSource::Input { uid } => format!("{INPUT_PREFIX}{uid}"),
        }
    }

    /// The bundle ID; only an app source has one (S12).
    pub fn bundle_id(&self) -> Option<&str> {
        match self {
            AudioSource::App { bundle_id } => Some(bundle_id),
            _ => None,
        }
    }

    /// Whether this source reads an input device and so needs the microphone
    /// permission (S15).
    pub fn needs_microphone(&self) -> bool {
        matches!(self, AudioSource::Input { .. })
    }
}

/// The state of the microphone permission (S15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrophonePermission {
    /// Granted.
    Authorized,
    /// Never asked.
    NotDetermined,
    /// Refused by the user.
    Denied,
    /// Blocked by policy.
    Restricted,
}

/// What a session start does about the microphone permission (S15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrophoneStep {
    /// Start capturing.
    Start,
    /// Ask for the permission, then start or fail (see [`after_microphone_request`]).
    Request,
    /// The start fails with a notice.
    Fail,
}

/// The microphone step for starting `source` (S15): sources other than an
/// input device need no permission.
pub fn microphone_step(source: &AudioSource, permission: MicrophonePermission) -> MicrophoneStep {
    if !source.needs_microphone() {
        return MicrophoneStep::Start;
    }
    match permission {
        MicrophonePermission::Authorized => MicrophoneStep::Start,
        MicrophonePermission::NotDetermined => MicrophoneStep::Request,
        MicrophonePermission::Denied | MicrophonePermission::Restricted => MicrophoneStep::Fail,
    }
}

/// The step after the permission request was answered (S15).
pub fn after_microphone_request(granted: bool) -> MicrophoneStep {
    if granted {
        MicrophoneStep::Start
    } else {
        MicrophoneStep::Fail
    }
}
