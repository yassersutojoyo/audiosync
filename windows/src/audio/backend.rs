//! The interface between the UI and the audio backend.
//!
//! The UI owns one [`AudioBackend`] and calls it from its own thread: it lists
//! sources, outputs and AirPlay speakers, starts and stops playback, changes
//! their settings, and calls [`AudioBackend::poll`] about twice a second to
//! learn about device changes and AirPlay speakers coming and going. Which speakers to turn on, and when playback should start or stop
//! because of them, stays the UI's decision (the session rules in
//! `audiosync_core::policy::session`).

use std::fmt;

use audiosync_core::eq::EqSettings;
use audiosync_core::policy::device_volume::DeviceVolume;

use super::routing::DirectPath;

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
    /// The app's name, from its executable. For [`SourceId::WholeSystem`] it
    /// is an English fallback: the UI shows its own, translated label.
    pub name: String,
    /// The app's executable file name, such as `spotify.exe`. It stays the
    /// same when the app restarts (its process ID doesn't), so the UI can
    /// remember a choice by it. Empty for the whole system.
    pub executable: String,
}

/// What kind of device an output is, as Windows describes its endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    /// The PC's own speakers or line out.
    BuiltIn,
    /// Headphones or a headset on the PC's own jack.
    Headphones,
    /// A Bluetooth device.
    Bluetooth,
    /// A USB device.
    Usb,
    /// A display or TV over HDMI.
    Hdmi,
    /// A display over DisplayPort.
    DisplayPort,
    /// A software device, such as a virtual cable.
    Virtual,
    /// Anything else (S/PDIF, a remote session), or not known.
    Other,
}

/// A connected output device (a render endpoint) and its settings.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
    /// The endpoint ID, the same across restarts and reboots.
    pub id: String,
    /// The name Windows shows for it.
    pub name: String,
    /// What kind of device it is.
    pub kind: OutputKind,
    /// Whether it is Windows' default output. AudioSync plays to each output
    /// by ID, so this is for display only.
    pub is_default: bool,
    /// Whether it is the reference output, which defines time 0 and has no
    /// delay setting (spec S174).
    pub is_reference: bool,
    /// Whether the user turned it on.
    pub enabled: bool,
    /// Its offset relative to the reference in milliseconds, −2000 to
    /// +20 000 (S175); kept, but not used, while it is the reference.
    pub delay_ms: f64,
    /// The volume slider position, 0 to 100.
    pub volume: f64,
    /// Whether it plays right now.
    pub playing: bool,
    /// The device's own volume and mute in Windows (spec S192, S194): read
    /// while the output is on, and `None` while it is off. Each part is
    /// `None` when Windows can't tell.
    pub device_volume: Option<DeviceVolume>,
}

/// Whether an AirPlay speaker can play from AudioSync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AirPlayAvailability {
    /// It can.
    Open,
    /// It asks for a password; AudioSync can't send one yet.
    PasswordNotYet,
    /// An Apple TV that asks for a PIN once: switching it on pairs it, and
    /// its credentials are kept from then on.
    NeedsPin,
    /// It needs another kind of pairing with a PIN, which AudioSync can't do
    /// yet.
    PairingNotYet,
    /// Only AirPlay 2 can play it; AudioSync speaks AirPlay 1 only so far.
    AirPlay2NotYet,
    /// It wants an encryption, codec, transport or format AudioSync can't
    /// send.
    Unsupported,
}

/// An AirPlay speaker on the network, and its settings.
#[derive(Clone, Debug, PartialEq)]
pub struct AirPlayInfo {
    /// Its ID: the device ID the speaker announces, the same across
    /// restarts.
    pub id: String,
    /// The name it announces.
    pub name: String,
    /// Whether it can play from AudioSync.
    pub availability: AirPlayAvailability,
    /// Whether the user turned it on.
    pub enabled: bool,
    /// The volume slider position, 0 to 100.
    pub volume: f64,
    /// Its own delay d_s in milliseconds, 0 to 3000.
    pub delay_ms: f64,
    /// Whether it plays right now.
    pub playing: bool,
    /// Whether AudioSync keeps credentials from pairing with it.
    pub paired: bool,
    /// Whether a pairing with it is running.
    pub pairing: bool,
}

/// The PIN prompt of a running pairing: the Apple TV shows a PIN for the
/// user to type, or will again after a wrong one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AirPlayPinPrompt {
    /// The speaker's ID.
    pub id: String,
    /// The name it announces.
    pub name: String,
    /// Where the pairing is.
    pub phase: PairingPhase,
    /// The Apple TV turned the last PIN down; it shows a new one.
    pub wrong_pin: bool,
}

/// Where a pairing is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingPhase {
    /// Asking the Apple TV to show a PIN.
    Starting,
    /// The Apple TV shows a PIN; waiting for the user to type it.
    WaitingForPin,
    /// The PIN went to the Apple TV; waiting for its answer.
    Checking,
}

/// What went wrong with an AirPlay speaker, for a notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AirPlayProblem {
    /// It didn't answer; AudioSync keeps trying.
    NotResponding,
    /// The connection died or the speaker went silent; AudioSync keeps
    /// trying.
    ConnectionLost,
    /// Another sender seems to be playing to it; AudioSync keeps trying.
    Busy,
    /// It turned the stream down, or answered in a way AudioSync doesn't
    /// understand; AudioSync keeps trying.
    Refused,
    /// It asks for a password, which AudioSync can't send yet.
    PasswordNotYet,
    /// An Apple TV that needs pairing with a PIN first (or again, after it
    /// stopped accepting AudioSync's credentials). Switching it on pairs it.
    NeedsPin,
    /// The pairing with an Apple TV failed, so it is off.
    PairingFailed,
    /// It needs another kind of pairing, which AudioSync can't do yet.
    PairingNotYet,
    /// Only AirPlay 2 can play it.
    AirPlay2NotYet,
    /// AudioSync can't play to it at all.
    Unsupported,
}

impl AirPlayProblem {
    /// Whether AudioSync keeps trying by itself; otherwise the speaker is
    /// turned off.
    pub fn retried(self) -> bool {
        matches!(
            self,
            AirPlayProblem::NotResponding
                | AirPlayProblem::ConnectionLost
                | AirPlayProblem::Busy
                | AirPlayProblem::Refused
        )
    }
}

impl AirPlayAvailability {
    /// Why a speaker that isn't open can't play, or `None` when it can.
    pub fn problem(self) -> Option<AirPlayProblem> {
        match self {
            AirPlayAvailability::Open => None,
            AirPlayAvailability::PasswordNotYet => Some(AirPlayProblem::PasswordNotYet),
            AirPlayAvailability::NeedsPin => Some(AirPlayProblem::NeedsPin),
            AirPlayAvailability::PairingNotYet => Some(AirPlayProblem::PairingNotYet),
            AirPlayAvailability::AirPlay2NotYet => Some(AirPlayProblem::AirPlay2NotYet),
            AirPlayAvailability::Unsupported => Some(AirPlayProblem::Unsupported),
        }
    }
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
    /// An output could not start because another app holds its device in
    /// exclusive mode. It stays on, is tried again every few seconds, and plays
    /// once the device is free; [`BackendEvent::OutputFree`] tells then.
    OutputBusy {
        /// The endpoint ID.
        id: String,
    },
    /// An output reported busy no longer waits for its device: it plays
    /// now, it was switched off, streaming stopped, or its device went
    /// away.
    OutputFree {
        /// The endpoint ID.
        id: String,
    },
    /// Playback stopped on its own.
    Stopped {
        /// Why it stopped.
        reason: StopReason,
    },
    /// Whether the source is heard twice: straight from Windows, undelayed,
    /// as well as through AudioSync's delayed copies. Checked every few
    /// seconds while streaming and reported when the answer changes.
    HeardTwice {
        /// The endpoints the source is heard from straight, by endpoint ID;
        /// empty once it is heard only through AudioSync again, or streaming
        /// stopped.
        paths: Vec<DirectPath>,
    },
    /// AirPlay speakers appeared, changed, went away, or started or stopped
    /// playing: list them again.
    AirPlayChanged,
    /// An AirPlay speaker plays (again).
    AirPlayPlaying {
        /// The speaker's ID.
        id: String,
    },
    /// An AirPlay speaker stopped playing, and AudioSync keeps trying to
    /// reach it. It stays on.
    AirPlayTrouble {
        /// The speaker's ID.
        id: String,
        /// What went wrong.
        problem: AirPlayProblem,
    },
    /// An AirPlay speaker can't play from AudioSync, so it was turned off.
    AirPlayFailed {
        /// The speaker's ID.
        id: String,
        /// Why.
        problem: AirPlayProblem,
    },
    /// An Apple TV the user switched on was paired, and is on now.
    AirPlayPaired {
        /// The speaker's ID.
        id: String,
    },
    /// An output's own volume or mute changed in Windows (spec S195): show
    /// it again.
    DeviceVolumeChanged {
        /// The endpoint ID.
        id: String,
    },
    /// The AirPlay sender stopped after an internal error or couldn't
    /// start, so no AirPlay speaker is listed or plays, or its discovery
    /// did, so none is listed; [`AudioBackend::retry_airplay`] starts a new
    /// sender.
    AirPlayStopped,
}

/// Why playback stopped on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The app being captured quit.
    SourceQuit,
    /// Capture failed and could not go on.
    CaptureFailed(BackendError),
}

/// Why something failed. The UI words each variant in its own, translated
/// text; [`fmt::Display`] gives English for logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The chosen app is AudioSync itself, or a process AudioSync started.
    OwnProcess,
    /// AudioSync runs inside the chosen app's process tree (that app started
    /// it), or may where process creation times can't tell, so capturing the
    /// tree could capture AudioSync's own output too.
    ContainsAudioSync,
    /// The chosen app is no longer running.
    SourceGone,
    /// The chosen process is not an app (the System or Idle process).
    NotAnApp,
    /// The output device is not connected, Windows audio is not running, or
    /// Windows is rebuilding the device's endpoint (after a wake, or a change
    /// of its format): worth another try.
    DeviceGone,
    /// Another app holds the device for itself (exclusive mode), so nothing
    /// else can play to it until that app lets go.
    DeviceBusy,
    /// The audio format can't be used, such as a rate outside 8 to 384 kHz.
    UnsupportedFormat,
    /// A Windows call failed with this HRESULT.
    System {
        /// The call that failed.
        call: &'static str,
        /// Its HRESULT.
        code: i32,
    },
}

/// A sentence in English for logs and diagnostics. The UI words errors in
/// its own, translated text; this sentence reaches the user only as the
/// detail under that text when Windows audio can't be opened at startup.
impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::OwnProcess => f.write_str("AudioSync can't capture its own sound."),
            BackendError::ContainsAudioSync => f.write_str(
                "AudioSync may run inside this app's process tree, so capturing the app \
                 could capture AudioSync too.",
            ),
            BackendError::SourceGone => f.write_str("The app isn't running anymore."),
            BackendError::NotAnApp => f.write_str("That process isn't an app."),
            BackendError::DeviceGone => {
                f.write_str("The device isn't connected, or Windows audio isn't running.")
            }
            BackendError::DeviceBusy => f.write_str("Another app is using the device exclusively."),
            BackendError::UnsupportedFormat => {
                f.write_str("The device's audio format isn't supported.")
            }
            BackendError::System { call, code } => {
                // `{:08X}` of a negative i32 prints its two's complement, the
                // way Windows writes HRESULTs.
                write!(f, "{call} failed (0x{code:08X}).")
            }
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

    /// The app that runs `executable` now, whether or not it has opened an
    /// audio stream: the app of process `root_pid` while that process still
    /// runs `executable`, else another app that runs it, or `None` when none
    /// does. The source list offers only apps with an audio stream, but a
    /// capture can start before its app plays (just opened, or paused).
    fn find_app(
        &mut self,
        root_pid: u32,
        executable: &str,
    ) -> Result<Option<SourceInfo>, BackendError>;

    /// Lists the connected outputs again, and returns them by name, with
    /// their settings. A failed listing keeps the last list.
    fn list_outputs(&mut self) -> Result<Vec<OutputInfo>, BackendError>;

    /// The connected outputs as last listed, by name, with their current
    /// settings. Unlike [`AudioBackend::list_outputs`], it doesn't ask the
    /// system again, so it is cheap enough for every refresh of the UI.
    fn outputs(&self) -> Vec<OutputInfo>;

    /// The IDs of the outputs turned on, connected or not, sorted.
    fn enabled_outputs(&self) -> Vec<String>;

    /// Starts capturing `source` and playing it on every enabled, connected
    /// output. A running session is stopped first.
    fn start(&mut self, source: &SourceId) -> Result<(), BackendError>;

    /// Stops capture and every output. Does nothing when nothing runs.
    fn stop(&mut self);

    /// The source being captured, or `None` when stopped.
    fn running_source(&self) -> Option<SourceId>;

    /// Turns an output on or off. While playing, an output turned on starts
    /// at once if it is connected; if it can't start, it is turned off again
    /// and the error returned.
    fn set_output_enabled(&mut self, id: &str, enabled: bool) -> Result<(), BackendError>;

    /// Sets an output's offset relative to the reference in milliseconds,
    /// live: every playing output whose hold-back changes is re-timed. A
    /// value that isn't finite is ignored; others are clamped to −2000 to
    /// +20 000.
    fn set_output_delay(&mut self, id: &str, delay_ms: f64);

    /// Sets an output's volume slider position (0 to 100, on a cubic curve),
    /// live. A value that isn't finite is ignored.
    fn set_output_volume(&mut self, id: &str, position: f64);

    /// Whether an output has settings of its own, connected or not: it was
    /// turned on or off, or its delay or volume set, in this run or a saved
    /// one.
    fn has_output_settings(&self, id: &str) -> bool;

    /// Gives output `to` the settings of output `from` (on or off, offset,
    /// volume) and forgets `from`'s, for a device listed under a new
    /// endpoint ID. While playing, `to` starts at once if it is on and
    /// connected; if it can't start, it is turned off again and the error
    /// returned.
    fn move_output_settings(&mut self, from: &str, to: &str) -> Result<(), BackendError>;

    /// The fix under an output whose own volume in Windows is low or muted
    /// (spec S197): sets that volume to 75 % when it is low and unmutes it
    /// when it is muted, once each. Nothing else ever writes a device volume.
    /// The output shows the result once Windows reports the change.
    fn fix_device_volume(&mut self, id: &str);

    /// Sets the master volume slider position (0 to 100), which scales every
    /// output and AirPlay speaker, live. A value that isn't finite is
    /// ignored.
    fn set_master_volume(&mut self, position: f64);

    /// The master volume slider position.
    fn master_volume(&self) -> f64;

    /// Sets the equalizer, live: it changes the captured audio once, before
    /// the ring, so every output hears the same equalized sound. The
    /// settings are kept for the next start.
    fn set_equalizer(&mut self, settings: EqSettings);

    /// The AirPlay speakers on the network, by name, with their settings;
    /// empty while there is no AirPlay sender.
    fn airplay_speakers(&self) -> Vec<AirPlayInfo>;

    /// Whether an AirPlay sender runs and browses for receivers: false
    /// without a sender, and once one stopped or its discovery did or
    /// couldn't start (reported as [`BackendEvent::AirPlayStopped`]).
    fn airplay_browsing(&self) -> bool;

    /// The IDs of the AirPlay speakers turned on, listed or not, sorted.
    fn enabled_airplay(&self) -> Vec<String>;

    /// Turns an AirPlay speaker on or off. While playing, a speaker turned on
    /// starts at once. One that can't play from AudioSync stays off, with the
    /// reason returned: [`AirPlayProblem::NeedsPin`] for an Apple TV to pair
    /// first ([`AudioBackend::start_airplay_pairing`]). Turning a speaker off
    /// also stops its pairing.
    fn set_airplay_enabled(&mut self, id: &str, enabled: bool) -> Result<(), AirPlayProblem>;

    /// Starts pairing with an Apple TV that asks for a PIN: it shows one, and
    /// [`AudioBackend::airplay_pin_prompt`] asks the UI for it. Paired, the
    /// speaker is turned on ([`BackendEvent::AirPlayPaired`]); a failure
    /// leaves it off with [`BackendEvent::AirPlayFailed`].
    fn start_airplay_pairing(&mut self, id: &str) -> Result<(), AirPlayProblem>;

    /// The PIN the user typed for the speaker's pairing. Returns whether it
    /// went to the Apple TV: only digits, while the pairing waits for one.
    fn submit_airplay_pin(&mut self, id: &str, pin: &str) -> bool;

    /// Stops the speaker's pairing; the speaker stays off.
    fn cancel_airplay_pairing(&mut self, id: &str);

    /// Forgets the speaker's pairing credentials: it is turned off, and asks
    /// for a PIN again the next time it is switched on.
    fn forget_airplay_pairing(&mut self, id: &str);

    /// The PIN prompt to show, if a pairing has one.
    fn airplay_pin_prompt(&self) -> Option<AirPlayPinPrompt>;

    /// Sets an AirPlay speaker's volume slider position (0 to 100), live. A
    /// value that isn't finite is ignored.
    fn set_airplay_volume(&mut self, id: &str, position: f64);

    /// Sets an AirPlay speaker's own delay d_s in milliseconds, live. A value
    /// that isn't finite is ignored; others are clamped to 0 to 3000.
    fn set_airplay_delay(&mut self, id: &str, delay_ms: f64);

    /// The AirPlay sender's base latency L_s in milliseconds, or `None`
    /// while there is no sender.
    fn airplay_sender_latency_ms(&self) -> Option<f64>;

    /// The user's AirPlay delay adjustment A in milliseconds: L = L_s + A
    /// while an AirPlay speaker is chosen (spec S178).
    fn airplay_adjustment(&self) -> f64;

    /// Sets the AirPlay delay adjustment A, live: local outputs are re-timed
    /// while an AirPlay speaker is chosen. A value that isn't finite is
    /// ignored; others are clamped to −2000 to +20 000.
    fn set_airplay_adjustment(&mut self, adjustment_ms: f64);

    /// Replaces an AirPlay sender that stopped (see
    /// [`BackendEvent::AirPlayStopped`]) with a new one. The speakers that
    /// were on stay on, with their settings, and play again once the new
    /// sender lists them. A new sender that can't start reports
    /// `AirPlayStopped` again.
    fn retry_airplay(&mut self);

    /// Handles device changes, lost outputs, the end of capture and what the
    /// AirPlay sender reports, and returns what the UI should know. Call it
    /// about twice a second.
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
    fn every_error_reads_as_a_sentence_in_the_log() {
        let errors = [
            BackendError::OwnProcess,
            BackendError::ContainsAudioSync,
            BackendError::SourceGone,
            BackendError::NotAnApp,
            BackendError::DeviceGone,
            BackendError::DeviceBusy,
            BackendError::UnsupportedFormat,
            BackendError::System { call: "x", code: 1 },
        ];
        for error in errors {
            let text = error.to_string();
            assert!(text.ends_with('.'), "{text:?} is a sentence");
        }
    }
}
