//! The interface between the UI and the audio backend.
//!
//! The UI owns one [`AudioBackend`] and calls it from its own thread: it lists
//! sources and outputs, starts and stops playback, changes output settings,
//! and calls [`AudioBackend::poll`] about twice a second to learn about device
//! changes. Which speakers to turn on, and when playback should start or stop
//! because of them, stays the UI's decision (the session rules in
//! `audiosync_core::policy::session`).

use std::fmt;

/// What AudioSync captures.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceId {
    /// Everything Windows plays, except AudioSync's own output.
    WholeSystem,
    /// One app: the process `root_pid` and every process it started (a
    /// browser's tab and GPU processes, for example).
    App {
        /// The process ID of the app's top process.
        root_pid: u32,
    },
}

/// A capture source the user can pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceInfo {
    /// What to pass to [`AudioBackend::start`].
    pub id: SourceId,
    /// The name to show.
    pub name: String,
    /// The app's executable file name, such as `spotify.exe`. It stays the
    /// same when the app restarts (its process ID doesn't), so the UI can
    /// remember a choice by it. Empty for the whole system.
    pub executable: String,
}

/// A connected output device (a render endpoint) and its settings.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
    /// The endpoint ID, the same across restarts and reboots.
    pub id: String,
    /// The name Windows shows for it.
    pub name: String,
    /// Whether it is Windows' default output. AudioSync plays to each output
    /// by ID, so this is for display only.
    pub is_default: bool,
    /// Whether the user turned it on.
    pub enabled: bool,
    /// The configured delay in milliseconds, 0 to 20 000.
    pub delay_ms: f64,
    /// The volume slider position, 0 to 100.
    pub volume: f64,
    /// Whether it plays right now.
    pub playing: bool,
}

/// Something the UI should know about, from [`AudioBackend::poll`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendEvent {
    /// Outputs were connected or disconnected, renamed, or the default output
    /// changed: list them again.
    OutputsChanged,
    /// An output's device went away while it played. The output stays on and
    /// plays again, at its current settings, once the device is back.
    OutputLost {
        /// The endpoint ID.
        id: String,
    },
    /// An output could not start, so it was turned off.
    OutputFailed {
        /// The endpoint ID.
        id: String,
        /// Why it could not start.
        error: BackendError,
    },
    /// Playback stopped on its own.
    Stopped {
        /// Why it stopped.
        reason: StopReason,
    },
}

/// Why playback stopped on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The app being captured quit.
    SourceQuit,
    /// Capture failed and could not go on.
    CaptureFailed(BackendError),
}

/// Why something failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The chosen app is AudioSync itself, or a process AudioSync started.
    OwnProcess,
    /// AudioSync runs inside the chosen app's process tree (that app started
    /// it), so capturing the tree would capture AudioSync's own output too.
    ContainsAudioSync,
    /// The chosen app is no longer running.
    SourceGone,
    /// The chosen process is not an app (the System or Idle process).
    NotAnApp,
    /// The output device is not connected, or Windows audio is not running.
    DeviceGone,
    /// The audio format can't be used, such as a rate outside 8 to 384 kHz.
    UnsupportedFormat,
    /// A Windows call failed with this HRESULT.
    System {
        /// The call that failed.
        call: &'static str,
        /// Its HRESULT.
        code: i32,
    },
    /// This build has no audio backend for the platform.
    Unsupported,
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::OwnProcess => f.write_str("AudioSync can't capture its own sound."),
            BackendError::ContainsAudioSync => f.write_str(
                "This app started AudioSync, so capturing it would capture AudioSync too. \
                 Pick All System Audio instead.",
            ),
            BackendError::SourceGone => f.write_str("The app isn't running anymore."),
            BackendError::NotAnApp => f.write_str("That process isn't an app."),
            BackendError::DeviceGone => {
                f.write_str("The device isn't connected, or Windows audio isn't running.")
            }
            BackendError::UnsupportedFormat => {
                f.write_str("The device's audio format isn't supported.")
            }
            BackendError::System { call, code } => {
                // `{:08X}` of a negative i32 prints its two's complement, the
                // way Windows writes HRESULTs.
                write!(f, "{call} failed (0x{code:08X}).")
            }
            BackendError::Unsupported => f.write_str("Audio isn't available on this platform."),
        }
    }
}

impl std::error::Error for BackendError {}

/// The audio backend, as the UI sees it.
pub trait AudioBackend {
    /// The sources to offer: the whole system first, then each app that has
    /// opened an audio stream, by name. AudioSync's own processes, and any
    /// app whose capture would include them, are left out.
    fn list_sources(&mut self) -> Result<Vec<SourceInfo>, BackendError>;

    /// The connected outputs, by name, with their settings.
    fn list_outputs(&mut self) -> Result<Vec<OutputInfo>, BackendError>;

    /// Starts capturing `source` and playing it on every enabled, connected
    /// output. A running session is stopped first.
    fn start(&mut self, source: &SourceId) -> Result<(), BackendError>;

    /// Stops capture and every output. Does nothing when nothing runs.
    fn stop(&mut self);

    /// The source being captured, or `None` when stopped.
    fn running_source(&self) -> Option<SourceId>;

    /// Frames captured since the start: 0 until the source plays something.
    fn frames_captured(&self) -> u64;

    /// Turns an output on or off. While playing, an output turned on starts
    /// at once if it is connected; if it can't start, it is turned off again
    /// and the error returned.
    fn set_output_enabled(&mut self, id: &str, enabled: bool) -> Result<(), BackendError>;

    /// Sets an output's delay in milliseconds, live. A value that isn't finite
    /// is ignored; others are clamped to 0 to 20 000.
    fn set_output_delay(&mut self, id: &str, delay_ms: f64);

    /// Sets an output's volume slider position (0 to 100, on a cubic curve),
    /// live. A value that isn't finite is ignored.
    fn set_output_volume(&mut self, id: &str, position: f64);

    /// Sets the master volume slider position (0 to 100), which scales every
    /// output, live. A value that isn't finite is ignored.
    fn set_master_volume(&mut self, position: f64);

    /// The master volume slider position.
    fn master_volume(&self) -> f64;

    /// Handles device changes, lost outputs and the end of capture, and
    /// returns what the UI should know. Call it about twice a second.
    fn poll(&mut self) -> Vec<BackendEvent>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_call_shows_its_hresult_the_windows_way() {
        let error = BackendError::System {
            call: "IAudioClient::Initialize",
            code: 0x8889_0008_u32 as i32,
        };
        assert_eq!(
            error.to_string(),
            "IAudioClient::Initialize failed (0x88890008)."
        );
    }

    #[test]
    fn every_error_has_a_sentence_for_the_user() {
        let errors = [
            BackendError::OwnProcess,
            BackendError::ContainsAudioSync,
            BackendError::SourceGone,
            BackendError::NotAnApp,
            BackendError::DeviceGone,
            BackendError::UnsupportedFormat,
            BackendError::System { call: "x", code: 1 },
            BackendError::Unsupported,
        ];
        for error in errors {
            let text = error.to_string();
            assert!(text.ends_with('.'), "{text:?} is a sentence");
        }
    }
}
