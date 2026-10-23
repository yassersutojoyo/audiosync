//! The UI's [`Backend`] over the audio backend.
//!
//! The windows show what the audio backend lists, the sources to capture, the
//! connected outputs and the AirPlay speakers with their settings, and every
//! user action becomes audio backend calls, decided by the session rules of
//! `audiosync-core` (`policy::session`): switching a speaker on starts
//! streaming, switching the last one off stops it, an output that can't start
//! is turned off, a source that quits ends the session. AirPlay speakers count
//! like any other speaker; one that can't play from AudioSync (AirPlay 2
//! only) is listed with that note and stays off, with a notice when the user
//! tries it. One that asks for a password asks the user for it when they
//! switch it on (the snapshot's [`PasswordPrompt`]), never on auto-connect,
//! which posts a notice instead. An Apple TV that asks for a PIN is paired
//! when the user switches it on (never by auto-connect, which only posts a
//! notice): the
//! snapshot's [`PinPrompt`] asks the UI for the PIN, and once paired the
//! speaker is on and streaming starts like for any speaker switched on. What
//! happens on its own comes from
//! [`AudioBackend::poll`], which [`Backend::poll`] passes on, and the adapter
//! turns it into notices. Notices carry a [`NoticeMessage`]; the UI words it.
//!
//! The chosen source, groups, auto-connect marks and notices live here, not in
//! the audio backend. The double-audio notice (the source heard straight from
//! an output as well as through AudioSync) follows what the backend reports:
//! it is posted when the outputs it names change, changed in place when only
//! which of them AudioSync plays to changes, and cleared when the cause is
//! gone. Once dismissed, it stays away for the same source and those outputs
//! (or fewer of them) until that app quits.
//!
//! AirPlay discovery that runs but has found no speaker for 20 seconds is
//! no notice: the snapshot says so ([`AppState::airplay_none_found`]), and
//! the speaker list carries a note about the network and Windows Firewall.
//! It goes once a speaker is listed and comes back after another 20 seconds
//! without one; while discovery is dead, the AirPlay notice with its Retry
//! says so instead.
//!
//! The user's choices outlive a restart (`crate::settings`): the adapter
//! reads the settings file before the first listing and hands the saved
//! switches, volumes and delays to the audio backend, which keeps them by ID
//! for a device that isn't there yet; at each poll it writes the file again
//! if anything changed. Like the Mac app, a launch doesn't start streaming
//! by itself, except through an auto-connect speaker that is there. Saved
//! settings of a device that is gone stay in the file for when it is back;
//! one saved as on that no listing has shown since launch doesn't count as a
//! speaker that is on until it is listed. A device Windows lists under a new
//! endpoint ID (a USB device plugged into another USB port) takes over what
//! was set for its old ID, which is forgotten, when exactly one gone output
//! with something set has its name and kind.
//!
//! Speakers are named by the core's speaker keys: `lc:` and the endpoint ID
//! for a local output, `ap:` and the speaker's ID for an AirPlay speaker, so
//! groups and auto-connect marks hold both. A Google Cast speaker is `cc:`
//! and its device ID; it stays out of groups, since one plays at a time.
//! Sources are `system`, or `app:` with the app's process ID
//! and executable (`app:2000:spotify.exe`), so a process ID that Windows gives
//! to another app later is never taken for the chosen one.
//!
//! The debug log gets what the user asked for, what the session rules
//! decided, the notices posted and the source list whenever it changes; the
//! audio backend logs what it did itself.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use audiosync_core::clock;
use audiosync_core::eq::{EqSettings, UserCurve, sanitized_curves};
use audiosync_core::policy::delay::{native_airplay_adjustment_ms, native_airplay_latency_ms};
use audiosync_core::policy::presets::{
    self, LeftOff, Preset, RecallAction, RecallMember, RecallOther, RecallStep,
};
use audiosync_core::policy::session::{
    AutoStartGate, DropReason, Incident, SpeakerEvent, StartOrigin, StreamingAction, may_start,
    reaction, source_switch, streaming_after,
};
use audiosync_core::policy::source_quit::WhenSourceQuits;
use audiosync_core::policy::speakers::{NoticeSubject, Notices, SpeakerKey};
use audiosync_core::policy::zones::{self, MemberKind, Situation, Step, Zone, Zones};
use audiosync_core::reader::DRIFT_TOLERANCE_MS;

use crate::audio::backend::{
    AirPlayAvailability, AirPlayInfo, AirPlayProblem, AudioBackend, BackendError, BackendEvent,
    CastDiscovery, CastInfo, CastProblem, CastStatus, OutputInfo, OutputKind, SourceId, SourceInfo,
    StopReason, ZoneVolumes,
};
use crate::audio::backend::{AirPlayPinPrompt, PairingPhase};
use crate::audio::cast_address::{self, CastAddress};
use audiosync_core::calibrate_all::{
    Applied, Change, Class, EndReason, RowState, Sheet, Take, Target,
};

use crate::audio::calibrate_all::{self, AllRun, Finished, Member, Progress};
use crate::audio::calibration::{self, CalibrationRun, Failure};
use crate::audio::capture::WHOLE_SYSTEM_NAME;
use crate::audio::outputs::MicKind;
use crate::audio::routing::{DirectPath, DirectReason};
use crate::data_folder;
use crate::debug_log::DebugLog;
use crate::settings::{
    self, KeptAside, SavedCurve, SavedEqualizer, SavedGroup, SavedHidden, SavedInput, SavedPreset,
    SavedSource, SavedSpeaker, SavedZone, Settings, SettingsFile,
};
use crate::speaker_names::SpeakerNames;
use crate::ui_model::{
    self, AbsentMember, AllPhase, AllProgress, AppState, Availability, Backend, CalibrateAllState,
    CalibrationState, CastAddressCheck, CastAddressError, CastGroup, CastList, CastRow, CastState,
    HiddenSpeaker, InTarget, LeftOutRow, Notice, NoticeMessage, PasswordPrompt, PinPhase,
    PinPrompt, PresetError, PresetLeftOff, SheetRow, SheetState, Source, SourceKind, Speaker,
    SpeakerKind, ViewSettings, Waiting, ZoneError, clamp_delay_ms,
};

/// The source list is read again every this many polls, about every three
/// seconds: it takes a snapshot of the running processes, and apps that play
/// audio come and go rarely.
const SOURCE_POLLS: u32 = 6;

/// The results of the last Calibrate All, kept until closed or out of date.
struct KeptSheet {
    sheet: Sheet,
    /// The speakers its rows describe, as they were at the start.
    members: Vec<Member>,
    /// The microphone's rate the run recorded at.
    rate: u32,
    /// The microphone or the capture stopped the run.
    failure: Option<Failure>,
    /// The last Measure Again ended early, so its row stayed.
    again_ended: Option<EndReason>,
    /// What Apply wrote, kept for Undo while every value still holds.
    applied: Option<Applied>,
    undone: bool,
}

#[derive(Default)]
struct CalibrateAll {
    /// A run or a Measure Again, while it holds the outputs.
    run: Option<AllRun>,
    /// What the card shows while no run is under way.
    phase: AllPhase,
    sheet: Option<KeptSheet>,
    endings: u64,
    /// Outputs left out by kind that the user chose Include Anyway for,
    /// until the results close.
    included: BTreeSet<String>,
}

/// After this many polls, 20 seconds, of AirPlay discovery without a single
/// AirPlay speaker found, the speaker list says so and what to check: a
/// Windows Firewall that blocks AudioSync, or a network Windows counts as
/// Public while AudioSync is allowed on Private ones only, keeps the
/// receivers' answers out, and the list would just stay empty.
const NONE_FOUND_POLLS: u32 = 40;

/// How long after a recall a member that fails to start goes into the
/// preset notice (spec S266): 30 s of polls, past an AirPlay connect's
/// retries.
const RECALL_WATCH_POLLS: u32 = 60;

/// The preset notice on screen: its preset and the speaker keys it names,
/// so it goes once they are all on (spec S265).
#[derive(Debug)]
struct PresetNoticeCause {
    id: String,
    keys: Vec<String>,
}

/// A recall's members that are starting: one that fails to start goes into
/// the preset notice as a line instead of a notice of its own (spec S266).
#[derive(Debug)]
struct RecallWatch {
    preset: String,
    keys: Vec<String>,
    polls_left: u32,
}

/// The UI's ID of the whole-system source.
const SYSTEM_KEY: &str = "system";

/// The source the next start captures.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Chosen {
    WholeSystem,
    /// An app, kept when it quits and followed by its executable when it
    /// starts again with a new process ID.
    App {
        root_pid: u32,
        executable: String,
        name: String,
    },
    /// An input device, kept while it is unplugged.
    Input {
        endpoint_id: String,
        name: String,
    },
}

impl Chosen {
    fn from_source(source: &SourceInfo) -> Chosen {
        match source.id {
            SourceId::WholeSystem => Chosen::WholeSystem,
            SourceId::App { root_pid } => Chosen::App {
                root_pid,
                executable: source.executable.clone(),
                name: source.name.clone(),
            },
            SourceId::Input { ref endpoint_id } => Chosen::Input {
                endpoint_id: endpoint_id.clone(),
                name: source.name.clone(),
            },
        }
    }

    fn id(&self) -> SourceId {
        match self {
            Chosen::WholeSystem => SourceId::WholeSystem,
            Chosen::App { root_pid, .. } => SourceId::App {
                root_pid: *root_pid,
            },
            Chosen::Input { endpoint_id, .. } => SourceId::Input {
                endpoint_id: endpoint_id.clone(),
            },
        }
    }

    /// The UI's ID of this source.
    fn key(&self) -> String {
        match self {
            Chosen::WholeSystem => SYSTEM_KEY.to_owned(),
            Chosen::App {
                root_pid,
                executable,
                ..
            } => app_key(*root_pid, executable),
            Chosen::Input { endpoint_id, .. } => input_key(endpoint_id),
        }
    }
}

/// A notice as it was posted.
#[derive(Clone, Debug)]
struct Posted {
    id: u64,
    message: NoticeMessage,
    time: String,
}

/// What the double-audio notice is about: the source, by its UI ID, and the
/// outputs it names, by endpoint ID and name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeardTwiceSubject {
    source: String,
    endpoints: Vec<(String, String)>,
}

/// The UI's backend over an audio backend. See the module documentation.
pub struct AudioAdapter<A: AudioBackend> {
    audio: A,
    /// The sources as last listed, the whole system first.
    sources: Vec<SourceInfo>,
    chosen: Chosen,
    /// The endpoint IDs of the last listing, to tell which outputs appeared.
    listed: BTreeSet<String>,
    /// The last name of every output listed, for a notice about one that is
    /// gone.
    names: BTreeMap<String, String>,
    /// The last kind of every output listed, to tell a device listed under
    /// a new endpoint ID.
    kinds: BTreeMap<String, OutputKind>,
    /// The endpoint IDs a device left when what was set for it followed it
    /// to a new one ([`AudioAdapter::move_output`]), until they are listed
    /// again: the same device's old IDs, not other devices of its name.
    left_behind: BTreeSet<String>,
    /// The AirPlay speaker IDs of the last listing, to tell which appeared.
    airplay_listed: BTreeSet<String>,
    /// The last name of every AirPlay speaker listed.
    airplay_names: BTreeMap<String, String>,
    /// Polls in a row with AirPlay discovery running and no AirPlay speaker
    /// listed; from [`NONE_FOUND_POLLS`] on, the speaker list says none was
    /// found.
    airplay_quiet_polls: u32,
    /// The last name of every Google Cast speaker listed, by device ID.
    cast_names: BTreeMap<String, String>,
    /// Polls in a row with Cast discovery running and no Cast speaker
    /// listed, as for AirPlay.
    cast_quiet_polls: u32,
    /// Where the Cast speakers' offsets and formats are kept; `None` keeps
    /// them until AudioSync quits.
    cast_file: Option<PathBuf>,
    /// What that file holds as last written or read.
    cast_saved: Option<String>,
    /// The Cast speakers the user added by address, as the settings keep
    /// them (spec C18).
    cast_addresses: Vec<String>,
    /// Whether something answers at an address the user adds.
    cast_probe: fn(&CastAddress) -> bool,
    /// The speaker keys marked auto-connect.
    auto_connect: BTreeSet<String>,
    /// The speakers hidden with Hide Speaker, in the order they were hidden.
    hidden: Vec<SavedHidden>,
    /// The names the user gave speakers (spec S228).
    custom_names: SpeakerNames,
    /// The groups as saved; only the Mac still shows them (S248).
    groups: Vec<SavedGroup>,
    /// The zones (spec S245), migrated from the groups once (S248).
    zones: Zones,
    /// The speaker presets (spec S251), the overlapping groups among them
    /// once (S254).
    presets: Vec<Preset>,
    /// What the preset notice says (spec S265).
    preset_notice: Option<PresetNoticeCause>,
    /// The members the latest recall switched on, while they start.
    recall_watch: Option<RecallWatch>,
    gate: AutoStartGate,
    notices: Notices<NoticeSubject, Posted>,
    next_notice: u64,
    /// What the double-audio notice showing is about.
    heard_twice: Option<HeardTwiceSubject>,
    /// The outputs named in a double-audio notice the user dismissed, by
    /// source UI ID, until that app quits.
    heard_twice_dismissed: BTreeMap<String, BTreeSet<String>>,
    equalizer: EqSettings,
    /// The equalizer curves the user saved (spec S242).
    equalizer_curves: Vec<UserCurve>,
    /// Polls since the sources were last listed.
    polls: u32,
    /// The local time now, as a notice shows it.
    clock: fn() -> String,
    /// The debug log; it writes nothing unless `AUDIOSYNC_DEBUG=1`.
    log: DebugLog,
    /// Where the settings are kept.
    settings_file: SettingsFile,
    /// The settings as last read or written: the file holds them.
    saved: Settings,
    /// The settings file is of a format older than the local delay model:
    /// its delays are migrated once an output list is in (spec S180), and
    /// nothing is saved before.
    delays_to_migrate: bool,
    /// Save the settings at the next chance even if they look unchanged:
    /// the file is in an older format.
    must_save: bool,
    /// AirPlay speakers saved as on that the backend couldn't switch on at
    /// launch (no AirPlay sender): their saved settings stay as they are.
    airplay_unrestored: BTreeSet<String>,
    /// The last save of the settings file failed; its notice was posted
    /// then, once until a save works again.
    settings_unsaved: bool,
    /// An output listing worked at least once, so an empty list means
    /// Windows has no active output.
    outputs_known: bool,
    /// The notice that Windows has no active output was posted for the
    /// current spell without one ([`AudioAdapter::check_sound_output`]).
    no_output_noticed: bool,
    /// The Google Cast speaker whose notice says the AirPlay speakers play
    /// ahead of it, and by how many tenths of a second
    /// ([`AudioAdapter::check_airplay_ahead`]); a dismissed one comes back
    /// only when that changes.
    ahead_noticed: Option<(String, u32)>,
    /// The outputs waiting for a device another app holds
    /// ([`BackendEvent::OutputBusy`]), by endpoint ID: their rows say so
    /// rather than show them playing.
    busy: BTreeSet<String>,
    /// The AirPlay speakers AudioSync keeps trying to reach, or that wait
    /// for the one AirPlay 2 slot ([`BackendEvent::AirPlayTrouble`]), by ID:
    /// their rows say so rather than show them playing.
    reconnecting: BTreeMap<String, Waiting>,
    /// The windows' own settings.
    view: ViewSettings,
    /// What a session does when the app it captures quits (spec S227).
    when_source_quits: WhenSourceQuits,
    /// The microphone calibration running, or letting its cut-off sweep
    /// play out.
    calibration_run: Option<CalibrationRun>,
    /// What the rows show of the last calibration.
    calibration: calibration::Status,
    calibrate_all: CalibrateAll,
}

impl<A: AudioBackend> AudioAdapter<A> {
    /// The adapter over `audio`, stopped, with the sources and outputs listed
    /// now and nothing saved. `clock` gives the time a notice shows; what
    /// happens goes to `log`.
    #[cfg(test)]
    pub fn new(audio: A, clock: fn() -> String, log: DebugLog) -> AudioAdapter<A> {
        AudioAdapter::with_settings(audio, clock, log, SettingsFile::in_memory())
    }

    /// The adapter over `audio` with the settings in `settings_file`,
    /// stopped unless an auto-connect speaker is there, with the sources and
    /// outputs listed now. `clock` gives the time a notice shows; what
    /// happens goes to `log`.
    pub fn with_settings(
        audio: A,
        clock: fn() -> String,
        log: DebugLog,
        settings_file: SettingsFile,
    ) -> AudioAdapter<A> {
        let (saved, loaded) = settings_file.load();
        if settings_file.path().is_some() {
            log.log(|| format!("settings: {loaded}"));
        } else {
            log.log(|| "settings: no app data folder; kept until AudioSync quits".to_owned());
        }
        let mut adapter = AudioAdapter {
            audio,
            sources: vec![SourceInfo {
                id: SourceId::WholeSystem,
                name: WHOLE_SYSTEM_NAME.to_owned(),
                executable: String::new(),
            }],
            chosen: Chosen::WholeSystem,
            listed: BTreeSet::new(),
            names: BTreeMap::new(),
            kinds: BTreeMap::new(),
            left_behind: BTreeSet::new(),
            airplay_listed: BTreeSet::new(),
            airplay_quiet_polls: 0,
            airplay_names: BTreeMap::new(),
            cast_names: BTreeMap::new(),
            cast_quiet_polls: 0,
            cast_file: None,
            cast_saved: None,
            cast_addresses: Vec::new(),
            cast_probe: cast_address::answers,
            auto_connect: BTreeSet::new(),
            hidden: Vec::new(),
            custom_names: SpeakerNames::default(),
            groups: Vec::new(),
            zones: Zones::default(),
            presets: Vec::new(),
            preset_notice: None,
            recall_watch: None,
            gate: AutoStartGate::default(),
            notices: Notices::new(),
            next_notice: 1,
            heard_twice: None,
            heard_twice_dismissed: BTreeMap::new(),
            equalizer: EqSettings::default(),
            equalizer_curves: Vec::new(),
            polls: 0,
            clock,
            log,
            settings_file,
            saved: Settings::default(),
            delays_to_migrate: matches!(
                loaded,
                settings::Loaded::Read { version, .. } if version < settings::OFFSETS_VERSION
            ),
            must_save: false,
            airplay_unrestored: BTreeSet::new(),
            settings_unsaved: false,
            outputs_known: false,
            no_output_noticed: false,
            ahead_noticed: None,
            busy: BTreeSet::new(),
            reconnecting: BTreeMap::new(),
            view: ViewSettings::default(),
            when_source_quits: WhenSourceQuits::Stop,
            calibration_run: None,
            calibration: calibration::Status::default(),
            calibrate_all: CalibrateAll::default(),
        };
        let had_groups = matches!(
            loaded,
            settings::Loaded::Read { version, .. } if version < settings::ZONES_VERSION
        );
        adapter.restore(saved);
        // The first launch with zones says what became of the groups, once:
        // the next save marks them migrated.
        if had_groups && !adapter.zones.list().is_empty() {
            adapter.post(NoticeSubject::Zones, NoticeMessage::GroupsBecameZones);
        }
        // Settings ignored at launch: the user is told now why the defaults
        // apply, even if they change nothing.
        adapter.keep_settings_aside(false);
        adapter.list_devices();
        adapter.log.log(|| match &adapter.chosen {
            Chosen::WholeSystem => format!("source chosen: {WHOLE_SYSTEM_NAME} (the default)"),
            Chosen::App { name, .. } | Chosen::Input { name, .. } => {
                format!("source chosen: {name} ({}), as saved", adapter.chosen.key())
            }
        });
        adapter
    }

    /// Takes in the saved settings before anything is listed: the audio
    /// backend keeps each speaker's by ID, so one that isn't there yet gets
    /// them when it appears. Nothing starts streaming here; auto-connect
    /// speakers do once the first listing finds them.
    fn restore(&mut self, saved: Settings) {
        let saved = saved.with_zones_migrated().with_presets_migrated();
        self.audio.set_master_volume(saved.master_volume);
        // A hidden speaker is never switched on, not even by the saved on-state.
        self.hidden = saved.hidden_speakers.clone();
        self.when_source_quits = WhenSourceQuits::from_stored(Some(&saved.when_source_quits));
        self.audio.set_when_source_quits(self.when_source_quits);
        if !self.delays_to_migrate {
            self.audio
                .set_airplay_adjustment(saved.airplay_adjustment_ms);
        }
        for (id, speaker) in &saved.outputs {
            // An older file's trims become offsets once the reference is
            // known ([`AudioAdapter::migrate_delays_if_due`]).
            if !self.delays_to_migrate {
                self.audio.set_output_delay(id, speaker.delay_ms);
            }
            self.audio.set_output_volume(id, speaker.volume);
            if speaker.enabled && !self.is_hidden(&speaker_key(id)) {
                // Nothing streams yet, so this only marks it on.
                let _ = self.audio.set_output_enabled(id, true);
            }
        }
        for (id, speaker) in &saved.airplay {
            self.audio.set_airplay_volume(id, speaker.volume);
            self.audio.set_airplay_delay(id, speaker.delay_ms);
            if speaker.enabled
                && !self.is_hidden(&airplay_key(id))
                && self.audio.set_airplay_enabled(id, true).is_err()
            {
                self.airplay_unrestored.insert(id.clone());
            }
        }
        if !self.airplay_unrestored.is_empty() {
            let count = self.airplay_unrestored.len();
            self.log.log(|| {
                format!("settings: {count} AirPlay speaker(s) stay off without an AirPlay sender")
            });
        }
        self.auto_connect = saved.auto_connect.clone();
        self.custom_names = SpeakerNames::from_saved(&saved.speaker_names);
        self.groups = saved.groups.clone();
        self.zones = Zones::from_stored(saved.speaker_zones.iter().map(|zone| Zone {
            id: zone.id.clone(),
            name: zone.name.clone(),
            members: zone.members.clone(),
            volume: zone.volume,
        }));
        self.presets = presets::from_stored(saved.speaker_presets.iter().map(SavedPreset::preset));
        self.push_zone_volumes();
        self.equalizer = saved.equalizer.settings();
        self.audio.set_equalizer(self.equalizer);
        self.equalizer_curves = settings::clamp_curves(&saved.equalizer_curves);
        self.cast_addresses = saved.cast_addresses.clone();
        for address in saved
            .cast_addresses
            .iter()
            .filter_map(|text| CastAddress::parse(text))
        {
            self.audio.add_cast_address(address);
        }
        self.chosen = match (&saved.input_device, &saved.source) {
            (Some(input), _) => Chosen::Input {
                endpoint_id: input.id.clone(),
                name: if input.name.is_empty() {
                    input.id.clone()
                } else {
                    input.name.clone()
                },
            },
            (None, SavedSource::System) => Chosen::WholeSystem,
            (None, SavedSource::App { executable, name }) => Chosen::App {
                // No process yet: the first listing finds the app if it
                // runs, by its executable.
                root_pid: 0,
                executable: executable.clone(),
                name: if name.is_empty() {
                    executable.clone()
                } else {
                    name.clone()
                },
            },
        };
        self.view = ViewSettings {
            show_virtual_devices: saved.show_virtual_devices,
            appearance: saved.appearance,
            window: saved.window,
            remote: saved.remote,
            airplay2_experimental: saved.airplay2_experimental,
        };
        self.saved = saved;
    }

    /// The speakers' settings to save: the saved ones, updated with what the
    /// backend holds for the speakers listed now; `enabled` from `on` for
    /// every speaker except the `held` ones. Defaults aren't kept.
    fn merged_speakers(
        saved: &BTreeMap<String, SavedSpeaker>,
        listed: impl IntoIterator<Item = (String, SavedSpeaker)>,
        on: &[String],
        held: &BTreeSet<String>,
    ) -> BTreeMap<String, SavedSpeaker> {
        let mut speakers = saved.clone();
        for (id, speaker) in speakers.iter_mut() {
            if !held.contains(id) {
                speaker.enabled = on.contains(id);
            }
        }
        speakers.extend(listed);
        for id in on {
            speakers.entry(id.clone()).or_insert(SavedSpeaker {
                enabled: true,
                ..SavedSpeaker::default()
            });
        }
        speakers.retain(|_, speaker| !speaker.is_default());
        speakers
    }

    /// The settings as they are now, to compare with the file.
    fn current_settings(&self) -> Settings {
        let outputs = Self::merged_speakers(
            &self.saved.outputs,
            self.audio.outputs().into_iter().map(|output| {
                let speaker = SavedSpeaker {
                    enabled: output.enabled,
                    volume: output.volume,
                    delay_ms: output.delay_ms,
                };
                (output.id, speaker)
            }),
            &self.audio.enabled_outputs(),
            &BTreeSet::new(),
        );
        let airplay = Self::merged_speakers(
            &self.saved.airplay,
            self.audio.airplay_speakers().into_iter().map(|speaker| {
                let saved = SavedSpeaker {
                    enabled: speaker.enabled,
                    volume: speaker.volume,
                    delay_ms: speaker.delay_ms,
                };
                (speaker.id, saved)
            }),
            &self.audio.enabled_airplay(),
            &self.airplay_unrestored,
        );
        Settings {
            version: settings::VERSION,
            source: match &self.chosen {
                Chosen::WholeSystem | Chosen::Input { .. } => SavedSource::System,
                Chosen::App {
                    executable, name, ..
                } => SavedSource::App {
                    executable: executable.clone(),
                    name: name.clone(),
                },
            },
            input_device: match &self.chosen {
                Chosen::Input { endpoint_id, name } => Some(SavedInput {
                    id: endpoint_id.clone(),
                    name: name.clone(),
                }),
                _ => None,
            },
            master_volume: self.audio.master_volume(),
            outputs,
            airplay,
            auto_connect: self.auto_connect.clone(),
            speaker_names: self.custom_names.saved(),
            hidden_speakers: self.hidden.clone(),
            groups: self.groups.clone(),
            speaker_zones: self
                .zones
                .list()
                .iter()
                .cloned()
                .map(SavedZone::from)
                .collect(),
            speaker_zones_migrated: true,
            speaker_presets: self
                .presets
                .iter()
                .cloned()
                .map(SavedPreset::from)
                .collect(),
            speaker_presets_migrated: true,
            equalizer: SavedEqualizer::from(self.equalizer),
            equalizer_curves: self.equalizer_curves.iter().map(SavedCurve::from).collect(),
            show_virtual_devices: self.view.show_virtual_devices,
            appearance: self.view.appearance,
            window: self.view.window,
            airplay_adjustment_ms: self.audio.airplay_adjustment(),
            when_source_quits: self.when_source_quits.stored().to_owned(),
            remote: self.view.remote,
            cast_addresses: self.cast_addresses.clone(),
            airplay2_experimental: self.view.airplay2_experimental,
        }
        .clamped()
    }

    /// Keeps the settings file ignored at launch aside, when it can be read,
    /// and tells the user once where it is: at launch, and while it couldn't
    /// be read then, at the poll it is `due` again, every few seconds (or at
    /// the first save, [`AudioAdapter::persist`]). Returns whether the
    /// notices changed.
    fn keep_settings_aside(&mut self, due: bool) -> bool {
        let kept = if due {
            self.settings_file.keep_ignored_aside_if_due()
        } else {
            self.settings_file.keep_ignored_aside()
        };
        match kept {
            Ok(Some(kept)) => self.tell_kept_aside(&kept, ""),
            Ok(None) => false,
            Err(error) => {
                if !due {
                    self.log.log(|| {
                        format!(
                            "settings: the file ignored at launch can't be kept aside yet \
                             ({error}); tried again every few seconds"
                        )
                    });
                }
                false
            }
        }
    }

    /// Logs that the settings file was kept aside (`when`, for the log),
    /// and, for a file ignored at launch, tells the user once with the
    /// copy's name: their settings were replaced by the defaults. Returns
    /// whether a notice was posted.
    fn tell_kept_aside(&mut self, kept: &KeptAside, when: &str) -> bool {
        let file = kept.file_name();
        self.log.log(|| match kept {
            KeptAside::Newer(_) => {
                format!("settings: a newer AudioSync's file was kept as {file}{when}")
            }
            KeptAside::Ignored(_) => {
                format!("settings: the file ignored at launch was kept as {file}{when}")
            }
        });
        if let KeptAside::Ignored(_) = kept {
            self.post(
                NoticeSubject::SettingsKeptAside,
                NoticeMessage::SettingsKeptAside { file },
            );
            return true;
        }
        false
    }

    /// Writes the settings file if anything changed since it was last read
    /// or written. A failed write is logged and tried again at the next
    /// change; the settings stay in effect for this run, and the user is
    /// told once, until a save works again and clears that notice. A file
    /// ignored at launch that the save kept aside is told once too, with the
    /// copy's name. Nothing is written while an older file's delays wait for
    /// their migration. Returns whether the notices changed.
    fn persist(&mut self) -> bool {
        if self.delays_to_migrate {
            return false;
        }
        let current = self.current_settings();
        if current == self.saved && !self.must_save {
            return false;
        }
        self.must_save = false;
        let saved = self.settings_file.save(&current);
        let mut notices_changed = saved.is_ok() == self.settings_unsaved;
        if saved.is_ok() && self.settings_unsaved {
            self.notices.clear(&[NoticeSubject::SettingsFile]);
        }
        self.settings_unsaved = saved.is_err();
        match saved {
            Ok(kept) if self.settings_file.path().is_some() => {
                if let Some(kept) = kept {
                    notices_changed |= self.tell_kept_aside(&kept, " before saving over it");
                }
                self.log.log(|| "settings: saved".to_owned());
            }
            Ok(_) => {}
            Err(error) => {
                self.log.log(|| {
                    format!(
                        "settings: couldn't be saved ({error}); they last until AudioSync quits"
                    )
                });
                if notices_changed {
                    self.post(NoticeSubject::SettingsFile, NoticeMessage::SettingsNotSaved);
                }
            }
        }
        self.saved = current;
        notices_changed
    }

    /// Stands in for the check that something answers at an address.
    #[cfg(test)]
    pub fn set_cast_probe(&mut self, probe: fn(&CastAddress) -> bool) {
        self.cast_probe = probe;
    }

    /// The audio backend, to change the mock's pretend world in tests.
    #[cfg(test)]
    pub fn audio_mut(&mut self) -> &mut A {
        &mut self.audio
    }

    fn running(&self) -> bool {
        self.audio.running_source().is_some()
    }

    /// What a calibration's line about output `endpoint_id` depends on.
    fn calibration_context(&self, endpoint_id: &str) -> calibration::Context {
        let outputs = self.audio.outputs();
        calibration::Context {
            streaming: self.running(),
            reference: outputs
                .iter()
                .find(|output| output.is_reference)
                .map(|output| output.id.clone()),
            speaker_on: outputs
                .iter()
                .any(|output| output.id == endpoint_id && output.enabled),
        }
    }

    /// Shows `phase` for output `endpoint_id`'s calibration.
    fn show_calibration(&mut self, endpoint_id: &str, phase: calibration::Phase) {
        self.calibration = calibration::Status {
            id: Some(endpoint_id.to_owned()),
            phase,
            context: Some(self.calibration_context(endpoint_id)),
            holds_outputs: self
                .calibration_run
                .as_ref()
                .is_some_and(CalibrationRun::holds_outputs),
        };
    }

    /// Moves a Calibrate All or Measure Again on, and takes in how it ended.
    /// Returns whether what the card shows changed.
    fn tick_calibrate_all(&mut self) -> bool {
        let Some(run) = self.calibrate_all.run.as_mut() else {
            return false;
        };
        let mut changed = run.tick(&mut self.audio, clock::now_ns());
        let finished = run.take_finished().map(|finished| {
            (
                finished,
                run.again_row(),
                run.members().to_vec(),
                run.rate(),
            )
        });
        if !run.holds_outputs() {
            self.calibrate_all.run = None;
            changed = true;
        }
        if let Some((finished, again, members, rate)) = finished {
            self.finish_calibrate_all(finished, again, members, rate);
            changed = true;
        }
        changed
    }

    /// A whole run's results replace the card's; a Measure Again's replace
    /// its row, unless it ended early (S239).
    fn finish_calibrate_all(
        &mut self,
        finished: Finished,
        again: Option<usize>,
        members: Vec<Member>,
        rate: u32,
    ) {
        let all = &mut self.calibrate_all;
        all.endings += 1;
        let Finished { sheet, failure } = finished;
        let Some(sheet) = sheet else {
            all.phase = AllPhase::Failed(failure.unwrap_or(Failure::Unfinished));
            return;
        };
        if again.is_none() {
            all.phase = AllPhase::Idle;
            all.sheet = Some(KeptSheet {
                sheet,
                members,
                rate,
                failure,
                again_ended: None,
                applied: None,
                undone: false,
            });
            return;
        }
        all.phase = failure.map_or(AllPhase::Idle, AllPhase::Failed);
        let Some(kept) = all.sheet.as_mut() else {
            return;
        };
        let ended = sheet.ended();
        if !kept.sheet.replace(sheet) && failure.is_none() {
            kept.again_ended = Some(ended.unwrap_or(EndReason::Cancelled));
        }
    }

    /// Writes a value of the results, as the speaker's delay field or the
    /// AirPlay delay field would; `None` puts back "none stored", which
    /// counts as 0 for a local or Cast speaker.
    fn write_calibrated(&mut self, members: &[Member], target: &Target, value: Option<f64>) {
        match target {
            Target::AirPlay => {
                if let Some(ms) = value {
                    <Self as Backend>::set_airplay_delay(self, ms);
                }
            }
            Target::Speaker(key) => {
                let Some(member) = members.iter().find(|m| m.key() == *key) else {
                    return;
                };
                let id = match member.class {
                    Class::Local => speaker_key(&member.id),
                    Class::Cast => cast_key(&member.id),
                    Class::AirPlay => return,
                };
                <Self as Backend>::set_speaker_delay(self, &id, value.unwrap_or(0.0));
            }
        }
    }

    /// A speaker's stored delay as the results read it; `None` while it
    /// isn't listed.
    fn calibrated_now(&self, class: Class, id: &str) -> Option<f64> {
        match class {
            Class::Local => self
                .audio
                .outputs()
                .into_iter()
                .find(|output| output.id == id)
                .map(|output| output.delay_ms),
            Class::Cast => self
                .audio
                .cast_speakers()
                .into_iter()
                .find(|cast| cast.id == id)
                .map(|cast| cast.offset_ms),
            Class::AirPlay => self.airplay_delay_ms(),
        }
    }

    /// Results that no longer describe the stored delays go: a measured
    /// speaker's delay changed before Apply (its new value would undo that
    /// change), or a value Apply wrote changed since (Undo would undo it).
    /// Returns whether the card changed.
    fn follow_calibrate_all(&mut self) -> bool {
        if self.calibrate_all.run.is_some() {
            return false;
        }
        let Some(kept) = self.calibrate_all.sheet.as_ref() else {
            return false;
        };
        // A speaker that isn't listed can't be compared: its row stays. A
        // stored delay is a whole number, so half a millisecond is the same.
        let holds = |now: Option<f64>, then: Option<f64>| {
            now.is_none_or(|now| (now - then.unwrap_or(0.0)).abs() <= 0.5)
        };
        if let Some(applied) = &kept.applied {
            let held = applied.changes.iter().all(|change| {
                let now = match &change.target {
                    Target::AirPlay => self.airplay_delay_ms(),
                    Target::Speaker(key) => kept
                        .members
                        .iter()
                        .find(|m| m.key() == *key)
                        .and_then(|m| self.calibrated_now(m.class, &m.id)),
                };
                holds(now, Some(change.new_ms))
            });
            if held {
                return false;
            }
            self.log
                .log(|| "Calibrate All: a delay changed after Apply, Undo goes".to_owned());
            if let Some(kept) = self.calibrate_all.sheet.as_mut() {
                kept.applied = None;
            }
            return true;
        }
        if kept.sheet.applied() || kept.sheet.ended().is_some() {
            return false;
        }
        let moved = kept
            .sheet
            .rows()
            .iter()
            .zip(&kept.members)
            .filter(|(row, _)| row.state != RowState::Reference)
            .any(|(_, m)| !holds(self.calibrated_now(m.class, &m.id), m.offset_ms));
        if !moved {
            return false;
        }
        self.log
            .log(|| "Calibrate All: a delay changed before Apply, the results go".to_owned());
        self.calibrate_all.sheet = None;
        self.calibrate_all.phase = AllPhase::Stale;
        self.calibrate_all.endings += 1;
        true
    }

    fn member_name(&self, member: &Member) -> String {
        match member.class {
            Class::Local => self.output_name(&member.id),
            Class::AirPlay => self.airplay_name(&member.id),
            Class::Cast => self.cast_name(&member.id),
        }
    }

    fn calibrate_all_state(&self) -> CalibrateAllState {
        let all = &self.calibrate_all;
        let running = all.run.as_ref().filter(|run| run.is_running());
        let phase = match running {
            Some(run) => AllPhase::Running(self.all_progress(run)),
            None => all.phase.clone(),
        };
        let busy = all.run.is_some();
        let lineup = calibrate_all::lineup(&self.audio);
        let left_out = lineup
            .plan(&all.included)
            .1
            .into_iter()
            .map(|left| LeftOutRow {
                name: self.output_name(&left.id),
                id: left.id,
                kind: left.kind,
                included: left.included,
            })
            .collect();
        CalibrateAllState {
            phase,
            // Why it can't start now (S235).
            blocked: calibrate_all::blocked(&lineup, &all.included),
            left_out,
            other_running: self.calibration_run.is_some(),
            sheet: all.sheet.as_ref().map(|kept| self.sheet_state(kept, busy)),
            endings: all.endings,
        }
    }

    fn all_progress(&self, run: &AllRun) -> AllProgress {
        match run.progress() {
            Progress::Preparing | Progress::Ended => AllProgress::Preparing,
            Progress::Measuring {
                row,
                take,
                searching,
            } => AllProgress::Measuring {
                name: self.member_name(&run.members()[row]),
                reference: take == Take::Reference,
                take: match take {
                    Take::Sweep(n) => n,
                    Take::Reference => 1,
                },
                searching,
            },
            Progress::Estimating { .. } => AllProgress::Estimating,
        }
    }

    /// The AirPlay delay is set through the sender's latency, unknown while
    /// AirPlay doesn't play: Apply and Undo wait rather than leave it out.
    fn airplay_waits(&self, kept: &KeptSheet) -> bool {
        if self.audio.airplay_sender_latency_ms().is_some() {
            return false;
        }
        let airplay = |changes: &[Change]| changes.iter().any(|c| c.target == Target::AirPlay);
        match &kept.applied {
            Some(applied) => airplay(&applied.changes),
            None => kept
                .sheet
                .changes()
                .is_ok_and(|applied| airplay(&applied.changes)),
        }
    }

    fn calibrate_all_waits(&self) -> bool {
        let waits = self
            .calibrate_all
            .sheet
            .as_ref()
            .is_some_and(|kept| self.airplay_waits(kept));
        if waits {
            self.log
                .log(|| "Calibrate All: the AirPlay delay waits for AirPlay to play".to_owned());
        }
        waits
    }

    fn sheet_state(&self, kept: &KeptSheet, busy: bool) -> SheetState {
        let sheet = &kept.sheet;
        let waits = self.airplay_waits(kept);
        let open = sheet.ended().is_none() && !sheet.applied();
        SheetState {
            rows: sheet
                .rows()
                .iter()
                .zip(&kept.members)
                .map(|(row, member)| SheetRow {
                    name: self.member_name(member),
                    class: row.class,
                    old_ms: row.old_ms,
                    state: row.state,
                    can_measure_again: open && row.state != RowState::Reference,
                })
                .collect(),
            airplay: sheet.airplay().copied(),
            ended: sheet.ended(),
            failure: kept.failure,
            again_ended: kept.again_ended,
            can_apply: !busy && !waits && sheet.changes().is_ok(),
            airplay_waits: waits,
            applied: sheet.applied(),
            can_undo: !busy && !waits && kept.applied.is_some(),
            undone: kept.undone,
        }
    }

    /// An ended calibration's line goes once streaming, the reference or
    /// its speaker's switch changed. Returns whether it went.
    fn follow_calibration_context(&mut self) -> bool {
        let Some(id) = self.calibration.id.clone() else {
            return false;
        };
        let next = self.calibration.following(&self.calibration_context(&id));
        let changed = next != self.calibration;
        self.calibration = next;
        changed
    }

    /// Speakers turned on, local, AirPlay and Cast, connected or not (S162).
    fn enabled_count(&self) -> usize {
        self.enabled_local() + self.enabled_network()
    }

    /// AirPlay and Google Cast speakers turned on: the session rules count
    /// them alike.
    fn enabled_network(&self) -> usize {
        let cast = self
            .audio
            .cast_speakers()
            .iter()
            .filter(|speaker| speaker.enabled)
            .count();
        self.audio.enabled_airplay().len() + cast
    }

    /// Local outputs turned on that were listed at some point in this run,
    /// connected now or not (S162). One saved as on that no listing showed
    /// since launch doesn't count: the user can't see it or switch it off,
    /// so it would keep streaming on and the notice asking for a speaker
    /// away (the old endpoint ID of a device that moved to another USB port,
    /// say). It stays on, and counts once it is listed.
    fn enabled_local(&self) -> usize {
        self.audio
            .enabled_outputs()
            .iter()
            .filter(|id| self.names.contains_key(*id))
            .count()
    }

    /// The streaming policy after `event` (S162), from the speakers on.
    fn streaming_after(&self, event: SpeakerEvent) -> StreamingAction {
        streaming_after(
            event,
            self.running(),
            self.enabled_local(),
            self.enabled_network(),
        )
    }

    fn is_airplay_enabled(&self, id: &str) -> bool {
        self.audio.enabled_airplay().iter().any(|on| on == id)
    }

    /// The name an AirPlay speaker shows: the user's, else the one last
    /// listed.
    fn airplay_name(&self, id: &str) -> String {
        let system = self.airplay_names.get(id).map_or(id, String::as_str);
        self.custom_names.shown(&airplay_key(id), system)
    }

    fn is_enabled(&self, endpoint_id: &str) -> bool {
        self.audio
            .enabled_outputs()
            .iter()
            .any(|id| id == endpoint_id)
    }

    /// The name an output shows: the user's, else the one last listed.
    fn output_name(&self, endpoint_id: &str) -> String {
        let system = self
            .names
            .get(endpoint_id)
            .map_or(endpoint_id, String::as_str);
        self.custom_names.shown(&speaker_key(endpoint_id), system)
    }

    fn chosen_name(&self) -> String {
        match &self.chosen {
            Chosen::WholeSystem => WHOLE_SYSTEM_NAME.to_owned(),
            Chosen::App { name, .. } | Chosen::Input { name, .. } => name.clone(),
        }
    }

    /// Posts a notice, replacing an older one about the same subject.
    fn post(&mut self, subject: NoticeSubject, message: NoticeMessage) {
        if self.fold_into_preset_notice(&subject, &message) {
            return;
        }
        self.log
            .log(|| format!("notice about {subject:?}: {message:?}"));
        let id = self.next_notice;
        self.next_notice += 1;
        let time = (self.clock)();
        self.notices.post(subject, Posted { id, message, time });
    }

    /// Clears the notice about `subject` if `kind` holds for what it says.
    fn clear_if(&mut self, subject: NoticeSubject, kind: fn(&NoticeMessage) -> bool) {
        if self.notices.get(&subject).is_some_and(|p| kind(&p.message)) {
            self.notices.clear(&[subject]);
        }
    }

    /// Whether AirPlay discovery has run for 20 seconds without finding a
    /// speaker. Never while it is dead: the AirPlay notice and its Retry
    /// say so then.
    fn airplay_none_found(&self) -> bool {
        self.airplay_quiet_polls >= NONE_FOUND_POLLS
    }

    /// Whether AirPlay discovery runs with no speaker listed, short of the
    /// 20 seconds after which [`Self::airplay_none_found`] holds.
    fn airplay_searching(&self) -> bool {
        (1..NONE_FOUND_POLLS).contains(&self.airplay_quiet_polls)
    }

    /// The speaker list's note about Google Cast discovery.
    fn cast_list(&self) -> CastList {
        match self.audio.cast_discovery() {
            CastDiscovery::Off => CastList::Quiet,
            CastDiscovery::Failed => CastList::Failed,
            CastDiscovery::Browsing if !self.audio.cast_speakers().is_empty() => CastList::Quiet,
            CastDiscovery::Browsing if self.cast_quiet_polls >= NONE_FOUND_POLLS => {
                CastList::NoneFound
            }
            CastDiscovery::Browsing => CastList::Searching,
        }
    }

    /// Counts one more poll of Cast discovery without a Cast speaker, or
    /// starts over; returns whether the speaker list's note changed.
    fn count_cast_quiet(&mut self) -> bool {
        let shown = self.cast_list();
        let browsing = self.audio.cast_discovery() == CastDiscovery::Browsing;
        if browsing && self.audio.cast_speakers().is_empty() {
            self.cast_quiet_polls = self.cast_quiet_polls.saturating_add(1);
        } else {
            self.cast_quiet_polls = 0;
        }
        let shows = self.cast_list();
        if shows != shown {
            self.log
                .log(|| format!("Cast: the speaker list's note is now {shows:?}"));
        }
        shows != shown
    }

    /// Keeps the Cast speakers' offsets and formats in `path` from now on.
    #[cfg_attr(
        not(windows),
        expect(dead_code, reason = "only the Windows app keeps a file")
    )]
    pub fn set_cast_file(&mut self, path: Option<PathBuf>) {
        self.cast_file = path;
        self.cast_saved = self.audio.cast_memory_json();
    }

    /// Writes the Cast speakers' offsets and formats to their file when they
    /// changed since the last write. One that can't be written gets the
    /// settings' notice: the change lasts until AudioSync quits.
    fn save_cast_memory(&mut self) {
        let (Some(path), Some(json)) = (self.cast_file.as_ref(), self.audio.cast_memory_json())
        else {
            return;
        };
        if self.cast_saved.as_ref() == Some(&json) {
            return;
        }
        match data_folder::write_atomically(path, json.as_bytes()) {
            Ok(()) => self.cast_saved = Some(json),
            Err(error) => {
                self.log
                    .log(|| format!("Cast: the speakers' file couldn't be written: {error}"));
                self.post(NoticeSubject::SettingsFile, NoticeMessage::SettingsNotSaved);
            }
        }
    }

    /// The name a Cast speaker shows: the user's, else the one last listed.
    fn cast_name(&self, id: &str) -> String {
        let system = self.cast_names.get(id).map_or(id, String::as_str);
        self.custom_names.shown(&cast_key(id), system)
    }

    /// The user switched a Cast speaker on or off. One that can't play
    /// stays off, with a notice saying why; a second one is refused while
    /// one is on.
    fn set_cast_enabled(&mut self, id: &str, on: bool) {
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!("the user switched {} {state}", self.cast_name(id))
        });
        if on {
            self.notices.clear(&[NoticeSubject::Cast(id.to_owned())]);
        }
        match self.audio.set_cast_enabled(id, on) {
            Err(problem) => self.cast_cant_play(id, problem),
            Ok(()) if on => self.start_if_switched_on(),
            Ok(()) => self.stop_if_idle(),
        }
        self.take_events();
    }

    /// Whether another app took Cast speaker `id` over, as its notice says:
    /// a zone click leaves it off (spec S246).
    fn cast_taken_over(&self, id: &str) -> bool {
        self.notices.iter().any(|(subject, posted)| {
            *subject == NoticeSubject::Cast(id.to_owned())
                && matches!(
                    posted.message,
                    NoticeMessage::CastCantPlay {
                        problem: ui_model::CastProblem::TakenOver,
                        ..
                    }
                )
        })
    }

    /// Adds a zone named `typed` (S150) holding `members`, each leaving the
    /// zone that held it; returns its name.
    fn add_zone(&mut self, typed: &str, members: &[String]) -> String {
        let next = self
            .zones
            .list()
            .iter()
            .filter_map(|zone| zone.id.strip_prefix("zone:")?.parse::<u64>().ok())
            .max()
            .map_or(1, |last| last.saturating_add(1));
        let id = format!("zone:{next}");
        let name = self
            .zones
            .new_zone(&id, typed, |n| format!("Zone {n}"))
            .name
            .clone();
        for member in members {
            self.zones.move_speaker(member, &id);
        }
        self.push_zone_volumes();
        name
    }

    /// Switches one zone member on or off as its own switch would, without
    /// starting or stopping streaming; a new attempt clears its notice.
    fn switch_zone_member(&mut self, key: &SpeakerKey, on: bool) {
        match key {
            SpeakerKey::Local(endpoint_id) => {
                if on {
                    self.notices.clear(&[speaker_subject(endpoint_id)]);
                }
                if self.audio.set_output_enabled(endpoint_id, on).is_err() {
                    self.output_failed(endpoint_id, true);
                }
            }
            SpeakerKey::AirPlay(id) => {
                if on {
                    self.notices.clear(&[airplay_subject(id)]);
                }
                if let Err(problem) = self.switch_airplay_by_user(id, on) {
                    self.airplay_switch_failed(id, problem);
                }
            }
            SpeakerKey::Cast(id) => {
                if on {
                    self.notices.clear(&[NoticeSubject::Cast(id.clone())]);
                }
                if let Err(problem) = self.audio.set_cast_enabled(id, on) {
                    self.cast_cant_play(id, problem);
                }
            }
        }
    }

    /// The ID of the preset the speakers play exactly now, the first in
    /// list order (spec S253).
    #[cfg(test)]
    pub(crate) fn current_preset(&self) -> Option<String> {
        ui_model::current_preset(&self.presets, &self.snapshot())
    }

    /// Recalls preset `id` as the core's plan says (spec S252), a full
    /// scene, and posts the preset notice or clears it (S265). Returns each
    /// stored member left off, by its shown name, with the reason; `None`
    /// for an ID no preset has.
    pub(crate) fn recall_preset(&mut self, id: &str) -> Option<Vec<(String, LeftOff)>> {
        let preset = self.presets.iter().find(|preset| preset.id == id)?.clone();
        let state = self.snapshot();
        let mut members: Vec<(SpeakerKey, Option<f64>, String, RecallMember)> = Vec::new();
        for member in &preset.members {
            let Some(key) = SpeakerKey::parse(&member.key) else {
                continue;
            };
            let listed = state.speaker(&member.key);
            let hidden = state
                .hidden
                .iter()
                .find(|entry| entry.id == member.key && entry.present.is_some());
            let kind = match (&key, listed.map(|speaker| speaker.kind)) {
                (SpeakerKey::Local(_), _) => MemberKind::Local,
                (SpeakerKey::Cast(_), _) => MemberKind::Cast,
                (SpeakerKey::AirPlay(_), Some(SpeakerKind::AirPlay2)) => MemberKind::AirPlay2,
                (SpeakerKey::AirPlay(id), _) if is_airplay_target(id) => MemberKind::AirPlay2,
                (SpeakerKey::AirPlay(_), _) => MemberKind::AirPlay,
            };
            // A member its stereo pair or home theater holds has no row of
            // its own, but is there (spec S247).
            let in_target = matches!(&key, SpeakerKey::AirPlay(id)
                if self.audio.airplay_holding_target(id).is_some());
            let name = listed
                .map(|speaker| speaker.name.clone())
                .or_else(|| hidden.map(|entry| entry.name.clone()))
                .unwrap_or_else(|| match &key {
                    SpeakerKey::AirPlay(id) if in_target => {
                        let system = self.audio.airplay_listed_name(id);
                        self.custom_names
                            .shown(&member.key, system.as_deref().unwrap_or(id))
                    }
                    SpeakerKey::AirPlay(id) => self.airplay_name(id),
                    _ => self.custom_names.shown(&member.key, key.id()),
                });
            let facts = RecallMember {
                kind,
                found: listed.is_some() || hidden.is_some() || in_target,
                hidden: hidden.is_some(),
                on: listed.is_some_and(|speaker| speaker.enabled),
                busy: matches!(&key, SpeakerKey::Cast(id) if self.cast_taken_over(id)),
                needs_airplay2: listed.is_none()
                    && hidden.is_none()
                    && !self.view.airplay2_experimental
                    && matches!(&key, SpeakerKey::AirPlay(id) if is_airplay_target(id)),
                in_target,
                fixed_volume: listed
                    .and_then(|speaker| speaker.cast.as_ref())
                    .is_some_and(|cast| cast.volume_fixed),
                has_volume: member.volume.is_some(),
            };
            members.push((key, member.volume, name, facts));
        }
        let others: Vec<&Speaker> = state
            .speakers
            .iter()
            .filter(|speaker| !preset.members.iter().any(|member| member.key == speaker.id))
            .collect();
        let other_facts: Vec<RecallOther> = others
            .iter()
            .map(|speaker| RecallOther {
                on: speaker.enabled,
                reference: speaker.is_reference(),
            })
            .collect();
        let facts: Vec<RecallMember> = members.iter().map(|(.., facts)| *facts).collect();
        let plan = presets::recall_plan(&facts, &other_facts, state.running);
        let left_off: Vec<(String, LeftOff)> = members
            .iter()
            .zip(&plan.steps)
            .filter_map(|((_, _, name, _), step)| match step {
                RecallStep::LeftOff(reason) => Some((name.clone(), *reason)),
                _ => None,
            })
            .collect();
        let shown: Vec<(String, PresetLeftOff)> = members
            .iter()
            .zip(&plan.steps)
            .filter_map(|((key, _, name, _), step)| match step {
                RecallStep::LeftOff(reason) => {
                    Some((name.clone(), self.left_off_reason(key, *reason)))
                }
                _ => None,
            })
            .collect();
        let keys_where = |wanted: fn(&RecallStep) -> bool| -> Vec<String> {
            members
                .iter()
                .zip(&plan.steps)
                .filter(|(_, step)| wanted(step))
                .map(|((key, ..), _)| key.to_string())
                .collect()
        };
        let left_keys = keys_where(|step| matches!(step, RecallStep::LeftOff(_)));
        let starting = keys_where(|step| matches!(step, RecallStep::On | RecallStep::Waiting));
        let nothing_changed = plan.action == RecallAction::Nothing;
        match ui_model::preset_notice(&preset.name, shown, nothing_changed) {
            Some(message) => {
                self.post(NoticeSubject::Presets, message);
                self.preset_notice = Some(PresetNoticeCause {
                    id: preset.id.clone(),
                    keys: left_keys,
                });
            }
            None => self.clear_preset_notice(),
        }
        // Set before the members switch: a start can fail at once.
        self.recall_watch = (!nothing_changed).then(|| RecallWatch {
            preset: preset.id.clone(),
            keys: starting,
            polls_left: RECALL_WATCH_POLLS,
        });
        if plan.action == RecallAction::Nothing {
            self.log.log(|| {
                format!(
                    "preset \"{}\": no stored speaker can play, nothing changes",
                    preset.name
                )
            });
            return Some(left_off);
        }
        self.log
            .log(|| format!("the user recalled preset \"{}\"", preset.name));
        // Before any member goes on, so a speaker that starts plays at its zone level.
        for (zone, volume) in presets::recall_zone_volumes(&preset, &self.zones) {
            self.zones.set_volume(zone, volume);
        }
        self.push_zone_volumes();
        for (speaker, off) in others.iter().zip(&plan.switch_off) {
            if let (true, Some(key)) = (*off, SpeakerKey::parse(&speaker.id)) {
                self.switch_zone_member(&key, false);
            }
        }
        let running = self.running();
        for ((key, volume, _, facts), (step, set_volume)) in
            members.iter().zip(plan.steps.iter().zip(&plan.set_volume))
        {
            if matches!(step, RecallStep::On | RecallStep::Waiting) && (!running || !facts.on) {
                self.switch_zone_member(key, true);
            }
            if let (true, Some(volume)) = (*set_volume, volume) {
                self.set_speaker_volume(&key.to_string(), *volume);
            }
        }
        self.audio.set_master_volume(preset.master);
        // The preamp and the equalizer switch are standing trims outside
        // every preset.
        self.equalizer.gains_db = preset.gains_db;
        self.audio.set_equalizer(self.equalizer);
        self.start_if_switched_on();
        self.take_events();
        Some(left_off)
    }

    /// Why member `key` stayed off, as the preset notice words it, with the
    /// shown name of the target it plays as part of.
    fn left_off_reason(&self, key: &SpeakerKey, reason: LeftOff) -> PresetLeftOff {
        match reason {
            LeftOff::NotFound => PresetLeftOff::NotFound,
            LeftOff::Hidden => PresetLeftOff::Hidden,
            LeftOff::NeedsAirPlay2 => PresetLeftOff::NeedsAirPlay2,
            LeftOff::Busy => PresetLeftOff::Busy,
            LeftOff::InTarget => {
                let target = match key {
                    SpeakerKey::AirPlay(id) => self.audio.airplay_holding_target(id),
                    _ => None,
                };
                PresetLeftOff::InTarget(target.map_or_else(String::new, |target| {
                    self.custom_names
                        .shown(&airplay_key(&target.id), &target.name)
                }))
            }
        }
    }

    /// Takes the preset notice down.
    fn clear_preset_notice(&mut self) {
        self.notices.clear(&[NoticeSubject::Presets]);
        self.preset_notice = None;
    }

    /// The preset notice goes once its preset is current again or every
    /// member it names is on (spec S265); the recall's watch runs out.
    fn follow_preset_notice(&mut self) -> bool {
        if let Some(watch) = &mut self.recall_watch {
            watch.polls_left = watch.polls_left.saturating_sub(1);
            if watch.polls_left == 0 || watch.keys.is_empty() {
                self.recall_watch = None;
            }
        }
        let Some(cause) = &self.preset_notice else {
            return false;
        };
        if self.notices.get(&NoticeSubject::Presets).is_none() {
            self.preset_notice = None;
            return false;
        }
        let state = self.snapshot();
        let on = |key: &String| state.speaker(key).is_some_and(|speaker| speaker.enabled);
        if state.current_preset.as_deref() == Some(cause.id.as_str()) || cause.keys.iter().all(on) {
            self.clear_preset_notice();
            return true;
        }
        false
    }

    /// A member the latest recall switched on that fails to start: its
    /// line goes into the preset notice, and no notice of its own posts
    /// (spec S266). Whether `message` about `subject` went there.
    fn fold_into_preset_notice(
        &mut self,
        subject: &NoticeSubject,
        message: &NoticeMessage,
    ) -> bool {
        let (name, reason) = match message {
            NoticeMessage::CastCantPlay { speaker, problem } => (
                speaker,
                if *problem == ui_model::CastProblem::TakenOver {
                    PresetLeftOff::Busy
                } else {
                    PresetLeftOff::CantPlay
                },
            ),
            NoticeMessage::SpeakerFailed { speaker }
            | NoticeMessage::AirPlayCantPlay { speaker, .. }
            | NoticeMessage::AirPlayMemberNeedsPassword {
                target: speaker, ..
            } => (speaker, PresetLeftOff::CantPlay),
            _ => return false,
        };
        let key = match subject {
            NoticeSubject::Speaker(key) => key.to_string(),
            NoticeSubject::Cast(id) => SpeakerKey::Cast(id.clone()).to_string(),
            _ => return false,
        };
        let Some(watch) = &mut self.recall_watch else {
            return false;
        };
        let Some(index) = watch.keys.iter().position(|watched| *watched == key) else {
            return false;
        };
        watch.keys.remove(index);
        let id = watch.preset.clone();
        let Some(preset) = self.presets.iter().find(|preset| preset.id == id) else {
            return false;
        };
        let mut notice = NoticeMessage::PresetLeftOff {
            preset: preset.name.clone(),
            left_off: Vec::new(),
            nothing_changed: false,
        };
        let mut keys = Vec::new();
        if let Some(cause) = self.preset_notice.take().filter(|cause| cause.id == id)
            && let Some(posted) = self.notices.get(&NoticeSubject::Presets)
        {
            notice = posted.message.clone();
            keys = cause.keys;
        }
        if let NoticeMessage::PresetLeftOff { left_off, .. } = &mut notice {
            left_off.push((name.clone(), reason));
        }
        keys.push(key);
        self.log
            .log(|| format!("preset {id}: a member failed to start; the preset notice says so"));
        self.post(NoticeSubject::Presets, notice);
        self.preset_notice = Some(PresetNoticeCause { id, keys });
        true
    }

    /// What plays now as preset `id` named `name`.
    fn capture_preset(&self, id: &str, name: &str) -> Result<Preset, PresetError> {
        ui_model::capture_preset(id, name, &self.snapshot()).ok_or(PresetError::NoSpeakerOn)
    }

    /// Tells why a Cast speaker can't play; it is off.
    fn cast_cant_play(&mut self, id: &str, problem: CastProblem) {
        let speaker = self.cast_name(id);
        self.post(
            NoticeSubject::Cast(id.to_owned()),
            NoticeMessage::CastCantPlay {
                speaker,
                problem: ui_cast_problem(problem),
            },
        );
        self.stop_if_idle();
    }

    /// A Google Cast speaker's row.
    fn cast_speaker(&self, speaker: CastInfo) -> Speaker {
        let id = cast_key(&speaker.id);
        Speaker {
            name: self.custom_names.shown(&id, &speaker.name),
            system_name: speaker.name,
            id,
            kind: SpeakerKind::Cast,
            enabled: speaker.enabled,
            volume: speaker.volume,
            delay_ms: Some(speaker.offset_ms),
            auto_connect: false,
            availability: Availability::Open,
            paired: false,
            device_volume: None,
            waiting: (speaker.enabled && speaker.status == CastStatus::Reconnecting)
                .then_some(Waiting::Reconnecting),
            cast: Some(CastRow {
                state: match speaker.status {
                    CastStatus::Idle => CastState::Idle,
                    CastStatus::Connecting => CastState::Connecting,
                    CastStatus::Playing => CastState::Playing,
                    CastStatus::Reconnecting => CastState::Reconnecting,
                },
                muted: speaker.muted,
                volume_fixed: speaker.volume_fixed,
                volume_step: speaker.volume_step,
                group: speaker.group.map(|group| CastGroup {
                    members: group.members,
                }),
                added_by_address: speaker.address.is_some(),
            }),
            group: None,
            mic_left_out: None,
            in_target: None,
        }
    }

    /// Counts one more poll with AirPlay discovery running and no AirPlay
    /// speaker listed, or starts over; returns whether the speaker list's
    /// note that none was found showed or went.
    fn count_airplay_quiet(&mut self) -> bool {
        let shown = self.airplay_none_found();
        let searched = self.airplay_searching();
        let browsing = self.audio.airplay_browsing();
        if browsing && self.airplay_listed.is_empty() {
            self.airplay_quiet_polls = self.airplay_quiet_polls.saturating_add(1);
        } else {
            self.airplay_quiet_polls = 0;
        }
        let shows = self.airplay_none_found();
        if shows != shown {
            self.log.log(|| {
                if shows {
                    "AirPlay: discovery has found no receiver in 20 s; the speaker list asks to \
                     check the network and Windows Firewall"
                        .to_owned()
                } else if browsing {
                    "AirPlay: a receiver is listed; the no-receiver note goes".to_owned()
                } else {
                    "AirPlay: discovery stopped; the no-receiver note goes".to_owned()
                }
            });
        }
        shows != shown || self.airplay_searching() != searched
    }

    /// Lists the outputs and the sources again.
    fn list_devices(&mut self) {
        // A failed listing keeps the last list; the backend tries again at
        // its next poll.
        // An older file's delays are migrated before auto-connect outputs
        // start, so they start at their migrated hold-backs.
        if self.audio.list_outputs().is_ok() {
            self.outputs_known = true;
            self.migrate_delays_if_due();
        }
        self.take_events();
        self.outputs_listed();
        self.airplay_listed();
        self.refresh_sources();
        self.check_sound_output();
    }

    /// Posts the notice that the AirPlay speakers play ahead of the Google
    /// Cast speaker that is on, while the sender's delay limit keeps them
    /// from waiting for it (C77, P99), and clears it once they keep up.
    /// Returns whether the notices changed.
    fn check_airplay_ahead(&mut self) -> bool {
        let ahead_ms = self.audio.airplay_ahead_ms();
        let cast = self
            .audio
            .cast_speakers()
            .into_iter()
            .find(|speaker| speaker.enabled)
            .filter(|_| ahead_ms >= f64::from(DRIFT_TOLERANCE_MS));
        let tenths = ((ahead_ms / 100.0).round() as u32).max(1);
        let now = cast.as_ref().map(|cast| (cast.id.clone(), tenths));
        if now == self.ahead_noticed {
            return false;
        }
        if let Some((noticed, _)) = self.ahead_noticed.take() {
            self.clear_if(NoticeSubject::Cast(noticed), |message| {
                matches!(message, NoticeMessage::AirPlayAheadOfCast { .. })
            });
        }
        if let Some(cast) = cast {
            self.post(
                NoticeSubject::Cast(cast.id),
                NoticeMessage::AirPlayAheadOfCast {
                    speaker: cast.name,
                    tenths,
                },
            );
        }
        self.ahead_noticed = now;
        true
    }

    /// Posts the notice that Windows has no active sound output while none
    /// is listed and an AirPlay speaker is on (apps then have nowhere to
    /// play, so nothing reaches the speaker), once each time that starts,
    /// and clears it once it ends. Returns whether the notices changed.
    fn check_sound_output(&mut self) -> bool {
        let none = self.outputs_known
            && self.audio.outputs().is_empty()
            && !self.audio.enabled_airplay().is_empty();
        if none == self.no_output_noticed {
            return false;
        }
        self.no_output_noticed = none;
        if none {
            self.post(NoticeSubject::SoundOutput, NoticeMessage::NoSoundOutput);
        } else {
            self.notices.clear(&[NoticeSubject::SoundOutput]);
        }
        true
    }

    /// Takes in the outputs as last listed: remembers their names and kinds,
    /// clears the notice about a lost output that is back, hands what was
    /// set for a device that moved to its new endpoint ID, and switches on
    /// the auto-connect speakers that appeared.
    fn outputs_listed(&mut self) {
        let outputs = self.audio.outputs();
        let mut listed = BTreeSet::new();
        let mut appeared = Vec::new();
        let mut fresh = BTreeSet::new();
        for output in &outputs {
            if !self.listed.contains(&output.id) {
                appeared.push(output.id.clone());
                if !self.names.contains_key(&output.id) || self.left_behind.contains(&output.id) {
                    fresh.insert(output.id.clone());
                }
            }
            self.names.insert(output.id.clone(), output.name.clone());
            self.kinds.insert(output.id.clone(), output.kind);
            listed.insert(output.id.clone());
        }
        self.listed = listed;
        for id in &appeared {
            self.clear_if(speaker_subject(id), |message| {
                matches!(message, NoticeMessage::SpeakerLost { .. })
            });
        }
        // Listed again, an ID left behind is an output of its own once more.
        for id in &appeared {
            self.left_behind.remove(id);
        }
        self.follow_moved_outputs(&outputs, &appeared, &fresh);
        self.auto_connect(&appeared);
    }

    /// Whether the user set anything for output `endpoint_id`: settings, an
    /// auto-connect mark, a group, a zone or Hide Speaker.
    fn has_choices(&self, endpoint_id: &str) -> bool {
        let key = speaker_key(endpoint_id);
        self.audio.has_output_settings(endpoint_id)
            || self.auto_connect.contains(&key)
            || self.groups.iter().any(|group| group.members.contains(&key))
            || self.zones.zone_of(&key).is_some()
            || self.is_hidden(&key)
    }

    /// Hands what the user set for an output that is gone to one that
    /// appeared under a new endpoint ID with nothing set, when both are most
    /// likely the same device: Windows lists a USB device plugged into
    /// another USB port under another endpoint ID. The new one must be
    /// `fresh`: an endpoint ID not listed before in this run, or one a
    /// device left behind when its settings followed it to another port (it
    /// came back). It must be the only output that appeared with its name
    /// and kind (which follows its form factor and bus). And exactly one
    /// output with that name and kind, listed earlier in this run and not
    /// now, with or without anything set, must be gone, and have something
    /// set: with two gone, the new ID could be either device. An ID a
    /// device left behind isn't another device, so it doesn't count. Names
    /// are compared without the number Windows may put before a device's
    /// name in another port or for a second one of a model
    /// ([`unnumbered`]).
    fn follow_moved_outputs(
        &mut self,
        outputs: &[OutputInfo],
        appeared: &[String],
        fresh: &BTreeSet<String>,
    ) {
        let new: Vec<&OutputInfo> = outputs
            .iter()
            .filter(|output| appeared.contains(&output.id))
            .collect();
        for output in &new {
            let own_name = unnumbered(&output.name);
            let same = |name: &str, kind: Option<&OutputKind>| {
                kind == Some(&output.kind) && unnumbered(name) == own_name
            };
            let twins = new
                .iter()
                .filter(|other| same(&other.name, Some(&other.kind)))
                .count();
            if twins > 1 || !fresh.contains(&output.id) || self.has_choices(&output.id) {
                continue;
            }
            let gone: Vec<String> = self
                .names
                .iter()
                .filter(|(id, name)| {
                    !self.listed.contains(*id)
                        && !self.left_behind.contains(*id)
                        && same(name, self.kinds.get(*id))
                })
                .map(|(id, _)| id.clone())
                .collect();
            if let [from] = gone.as_slice()
                && self.has_choices(from)
            {
                self.move_output(from, &output.id);
            }
        }
    }

    /// Moves what the user set for output `from` to output `to`: see
    /// [`AudioAdapter::follow_moved_outputs`]. `from` is forgotten, here and
    /// in the settings file, and its notice goes.
    fn move_output(&mut self, from: &str, to: &str) {
        self.log.log(|| {
            format!(
                "{} is listed under a new endpoint ID ({to}, was {from}): what was set for it \
                 follows it",
                self.output_name(to)
            )
        });
        let result = self.audio.move_output_settings(from, to);
        let (from_key, to_key) = (speaker_key(from), speaker_key(to));
        if self.auto_connect.remove(&from_key) {
            self.auto_connect.insert(to_key.clone());
        }
        self.custom_names.move_key(&from_key, &to_key);
        if let Some(entry) = self.hidden.iter_mut().find(|entry| entry.key == from_key) {
            entry.key = to_key.clone();
        }
        for group in &mut self.groups {
            if let Some(place) = group.members.iter().position(|key| *key == from_key) {
                if group.members.contains(&to_key) {
                    group.members.remove(place);
                } else {
                    group.members[place] = to_key.clone();
                }
            }
        }
        self.zones.rename_key(&from_key, &to_key);
        for preset in &mut self.presets {
            preset.rename_key(&from_key, &to_key);
        }
        self.push_zone_volumes();
        if self.saved.outputs.remove(from).is_some() {
            self.must_save = true;
        }
        self.left_behind.insert(from.to_owned());
        self.busy.remove(from);
        self.notices.clear(&[speaker_subject(from)]);
        if result.is_err() {
            self.output_failed(to, false);
        }
    }

    /// Takes in the AirPlay speakers as last listed: remembers their names,
    /// and switches on the auto-connect speakers that appeared.
    fn airplay_listed(&mut self) {
        let mut listed = BTreeSet::new();
        let mut appeared = Vec::new();
        for speaker in self.audio.airplay_speakers() {
            if !self.airplay_listed.contains(&speaker.id) {
                appeared.push(speaker.id.clone());
            }
            self.airplay_names.insert(speaker.id.clone(), speaker.name);
            listed.insert(speaker.id);
        }
        self.airplay_listed = listed;
        let hits: Vec<String> = appeared
            .into_iter()
            .filter(|id| {
                let key = airplay_key(id);
                self.auto_connect.contains(&key) && !self.is_hidden(&key)
            })
            .collect();
        if hits.is_empty() {
            return;
        }
        self.log.log(|| {
            let names: Vec<String> = hits.iter().map(|id| self.airplay_name(id)).collect();
            format!("auto-connect: switching on {}", names.join(", "))
        });
        for id in &hits {
            if !self.is_airplay_enabled(id)
                && let Err(problem) = self.audio.set_airplay_enabled(id, true)
            {
                self.airplay_cant_play(id, problem);
            }
        }
        if !self.running() && self.enabled_count() > 0 {
            self.start_session(StartOrigin::AutoConnect);
        }
    }

    /// The zone members neither `speakers` nor `hidden` holds, by the name
    /// the user gave them, else the one last listed.
    fn absent_members(&self, speakers: &[Speaker], hidden: &[HiddenSpeaker]) -> Vec<AbsentMember> {
        let known = |key: &String| {
            speakers.iter().any(|speaker| speaker.id == *key)
                || hidden.iter().any(|entry| entry.id == *key)
        };
        self.zones
            .list()
            .iter()
            .flat_map(|zone| &zone.members)
            .filter(|key| !known(key))
            .map(|key| {
                let (last, kind) = match SpeakerKey::parse(key) {
                    Some(SpeakerKey::Local(id)) => (
                        self.names.get(&id),
                        self.kinds
                            .get(&id)
                            .map_or(SpeakerKind::Other, |kind| speaker_kind(*kind)),
                    ),
                    Some(SpeakerKey::AirPlay(id)) => {
                        (self.airplay_names.get(&id), SpeakerKind::AirPlay)
                    }
                    Some(SpeakerKey::Cast(id)) => (self.cast_names.get(&id), SpeakerKind::Cast),
                    None => (None, SpeakerKind::Other),
                };
                AbsentMember {
                    id: key.clone(),
                    name: self
                        .custom_names
                        .shown(key, last.map_or("", String::as_str)),
                    kind,
                }
            })
            .collect()
    }

    /// Whether speaker `key` is hidden with Hide Speaker.
    fn is_hidden(&self, key: &str) -> bool {
        self.hidden.iter().any(|entry| entry.key == key)
    }

    /// `speakers` without the hidden ones, and the Hidden Speakers entries:
    /// a listed one by its name now, sorted by name, case-insensitively,
    /// then by ID.
    fn without_hidden(&self, speakers: Vec<Speaker>) -> (Vec<Speaker>, Vec<HiddenSpeaker>) {
        let (hidden_rows, listed): (Vec<Speaker>, Vec<Speaker>) = speakers
            .into_iter()
            .partition(|speaker| self.is_hidden(&speaker.id));
        let mut hidden: Vec<HiddenSpeaker> = self
            .hidden
            .iter()
            .map(|entry| {
                let row = hidden_rows.iter().find(|row| row.id == entry.key);
                HiddenSpeaker {
                    id: entry.key.clone(),
                    name: row.map_or_else(|| entry.name.clone(), |row| row.name.clone()),
                    kind_text: entry.kind.clone(),
                    present: row.map(|row| row.kind),
                }
            })
            .collect();
        hidden.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        (listed, hidden)
    }

    /// Switches on the auto-connect speakers among `appeared`, then starts
    /// streaming once for all of them, unless the user switched streaming off
    /// or an automatic start failed (S120).
    fn auto_connect(&mut self, appeared: &[String]) {
        let hits: Vec<&String> = appeared
            .iter()
            .filter(|id| {
                let key = speaker_key(id);
                self.auto_connect.contains(&key) && !self.is_hidden(&key)
            })
            .collect();
        if hits.is_empty() {
            return;
        }
        self.log.log(|| {
            let names: Vec<String> = hits.iter().map(|id| self.output_name(id)).collect();
            format!("auto-connect: switching on {}", names.join(", "))
        });
        for id in hits {
            if !self.is_enabled(id) && self.audio.set_output_enabled(id, true).is_err() {
                self.output_failed(id, false);
            }
        }
        if !self.running() {
            self.start_session(StartOrigin::AutoConnect);
        }
    }

    /// Lists the sources again, and follows the chosen app to its new process
    /// if it started again. Returns whether the list or the choice changed. A
    /// failed listing keeps the last list.
    fn refresh_sources(&mut self) -> bool {
        let Ok(sources) = self.audio.list_sources() else {
            return false;
        };
        let mut chosen = self.chosen.clone();
        if let Chosen::App {
            root_pid,
            executable,
            name,
        } = &mut chosen
        {
            let same_app = |source: &&SourceInfo| source.executable == *executable;
            let found = sources
                .iter()
                .filter(same_app)
                .find(|source| {
                    source.id
                        == SourceId::App {
                            root_pid: *root_pid,
                        }
                })
                .or_else(|| sources.iter().find(same_app));
            if let Some(SourceInfo {
                id: SourceId::App { root_pid: pid },
                name: listed_name,
                ..
            }) = found
            {
                *root_pid = *pid;
                *name = listed_name.clone();
            }
        }
        if let Chosen::Input { endpoint_id, name } = &mut chosen
            && let Some(listed) = sources.iter().find(|source| {
                matches!(&source.id, SourceId::Input { endpoint_id: id } if id == endpoint_id)
            })
        {
            name.clone_from(&listed.name);
        }
        if sources != self.sources {
            self.log.log(|| {
                let listed: Vec<String> = sources.iter().map(describe_source).collect();
                format!("sources listed: {}", listed.join("; "))
            });
        }
        if chosen.key() != self.chosen.key() {
            self.log
                .log(|| format!("the chosen app now runs as {}", chosen.key()));
        }
        let changed = sources != self.sources || chosen != self.chosen;
        self.sources = sources;
        self.chosen = chosen;
        changed
    }

    /// The source to start: the whole system, the chosen app where it runs
    /// now, or the chosen input device while it is connected. The app needn't
    /// play yet: capture hears it once it does.
    fn chosen_source(&mut self) -> Result<SourceId, BackendError> {
        if self.chosen == Chosen::WholeSystem {
            return Ok(SourceId::WholeSystem);
        }
        self.refresh_sources();
        let key = self.chosen.key();
        if matches!(self.chosen, Chosen::Input { .. })
            && !self.sources.iter().any(|source| source_key(source) == key)
        {
            return Err(BackendError::InputGone);
        }
        if let Chosen::App {
            root_pid,
            executable,
            ..
        } = &self.chosen
            && !self.sources.iter().any(|source| source_key(source) == key)
        {
            // Not listed: it has no audio session right now, or it quit and
            // its process ID may belong to another program by now. The
            // process list tells which.
            let app = self
                .audio
                .find_app(*root_pid, executable)?
                .ok_or(BackendError::SourceGone)?;
            self.chosen = Chosen::from_source(&app);
            self.log.log(|| {
                format!(
                    "the chosen app runs as {}, without an audio stream yet",
                    self.chosen.key()
                )
            });
        }
        Ok(self.chosen.id())
    }

    /// Starts streaming the chosen source to the enabled outputs (S116, S119,
    /// S120). A user's start clears the notices about the last attempt.
    /// Returns whether streaming started.
    fn start_session(&mut self, origin: StartOrigin) -> bool {
        if origin == StartOrigin::User {
            self.gate.user_acted();
            self.notices.clear(&[
                NoticeSubject::Source,
                NoticeSubject::Start,
                NoticeSubject::Capture,
            ]);
        } else if !self.gate.allows(origin) {
            self.log.log(|| {
                format!(
                    "start ({origin:?}) held back: streaming was switched off, \
                     or an automatic start failed"
                )
            });
            return false;
        }
        if !may_start(self.enabled_count()) {
            self.log
                .log(|| format!("start ({origin:?}) refused: no speaker is on"));
            self.post(NoticeSubject::Start, NoticeMessage::NoSpeaker);
            return false;
        }
        self.log.log(|| {
            format!(
                "start ({origin:?}): {}, {} speaker(s) on",
                self.chosen_name(),
                self.enabled_count()
            )
        });
        let started = self
            .chosen_source()
            .and_then(|source| self.audio.start(&source));
        match started {
            Ok(()) => {
                self.notices
                    .clear(&[NoticeSubject::Start, NoticeSubject::Source]);
                // Outputs that couldn't start are reported at the next poll;
                // take them now.
                self.take_events();
                true
            }
            Err(error) => {
                self.log
                    .log(|| format!("start ({origin:?}) failed: {error}"));
                self.gate.start_failed(origin);
                self.audio.stop();
                let message = self.start_problem(&error);
                self.post(NoticeSubject::Start, message);
                false
            }
        }
    }

    /// What a notice says about a start that failed with `error`.
    fn start_problem(&self, error: &BackendError) -> NoticeMessage {
        let app = matches!(self.chosen, Chosen::App { .. });
        match error {
            BackendError::InputGone => NoticeMessage::InputNotConnected {
                source: self.chosen_name(),
            },
            BackendError::MicrophoneBlocked => NoticeMessage::MicrophoneBlocked {
                source: self.chosen_name(),
            },
            BackendError::SourceGone if app => NoticeMessage::SourceNotRunning {
                source: self.chosen_name(),
            },
            BackendError::OwnProcess | BackendError::ContainsAudioSync if app => {
                NoticeMessage::SourceRefused {
                    source: self.chosen_name(),
                }
            }
            _ => NoticeMessage::CaptureFailed,
        }
    }

    /// An output couldn't start, and the audio backend turned it off: tells
    /// the user, and stops streaming when nothing else is on (S69, S136).
    fn output_failed(&mut self, endpoint_id: &str, by_user: bool) {
        let reaction = reaction(&Incident::OutputStartFailed {
            uid: endpoint_id.to_owned(),
            by_user,
            enabled_left: self.enabled_count(),
        });
        if let Some(subject) = reaction.notice {
            let speaker = self.output_name(endpoint_id);
            self.post(subject, NoticeMessage::SpeakerFailed { speaker });
        }
        if reaction.stop_session {
            self.log.log(|| {
                format!(
                    "streaming stops: {} couldn't start and no other speaker is on",
                    self.output_name(endpoint_id)
                )
            });
            self.audio.stop();
        }
    }

    /// An AirPlay speaker can't play from AudioSync, so it is off: tells the
    /// user, and stops streaming when nothing else is on (S136).
    fn airplay_cant_play(&mut self, id: &str, problem: AirPlayProblem) {
        let message = NoticeMessage::AirPlayCantPlay {
            speaker: self.airplay_name(id),
            problem: ui_problem(problem),
        };
        self.airplay_off(id, problem, message);
    }

    /// As [`Self::airplay_cant_play`], with the notice `message`.
    fn airplay_off(&mut self, id: &str, problem: AirPlayProblem, message: NoticeMessage) {
        let reason = match problem {
            AirPlayProblem::NeedsPassword
            | AirPlayProblem::PairingNotYet
            | AirPlayProblem::NeedsPin
            | AirPlayProblem::PairingFailed => DropReason::NeedsCode,
            _ => DropReason::Unreachable,
        };
        let reaction = reaction(&Incident::SpeakerDropped {
            id: id.to_owned(),
            reason,
            enabled_left: self.enabled_count(),
        });
        if let Some(subject) = reaction.notice {
            self.post(subject, message);
        }
        if reaction.stop_session && self.running() {
            self.log.log(|| {
                format!(
                    "streaming stops: {} can't play and no other speaker is on",
                    self.airplay_name(id)
                )
            });
            self.audio.stop();
        }
    }

    /// The user switched an AirPlay speaker on or off. One that can't play
    /// stays off, with a notice saying why.
    fn set_airplay_enabled(&mut self, id: &str, on: bool) {
        if on
            && !self
                .audio
                .airplay_speakers()
                .iter()
                .any(|speaker| speaker.id == id)
        {
            self.log
                .log(|| format!("AirPlay speaker ({id}) isn't listed; nothing to switch on"));
            return;
        }
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!("the user switched {} {state}", self.airplay_name(id))
        });
        if on {
            // A new attempt.
            self.notices.clear(&[airplay_subject(id)]);
        }
        if let Err(problem) = self.switch_airplay_by_user(id, on) {
            self.airplay_switch_failed(id, problem);
            return;
        }
        if on {
            self.start_if_switched_on();
        } else {
            self.stop_if_idle();
        }
        self.take_events();
    }

    /// Switches an AirPlay speaker on or off for the user: one switched on
    /// starts an attempt, which may prompt for a password
    /// ([`AudioBackend::begin_airplay_attempt`]).
    fn switch_airplay_by_user(&mut self, id: &str, on: bool) -> Result<(), AirPlayProblem> {
        self.audio.set_airplay_enabled(id, on)?;
        if on {
            self.audio.begin_airplay_attempt(id);
        }
        Ok(())
    }

    /// An AirPlay speaker the user switched on didn't turn on: an Apple TV
    /// that asks for a PIN is paired (it turns on once paired), one that asks
    /// for a password prompts for it (it turns on once typed); any other
    /// stays off with a notice.
    fn airplay_switch_failed(&mut self, id: &str, problem: AirPlayProblem) {
        if problem == AirPlayProblem::NeedsPassword {
            self.log.log(|| {
                format!(
                    "{} asks for a password: the prompt opens",
                    self.airplay_name(id)
                )
            });
            if let Err(problem) = self.audio.ask_airplay_password(id) {
                self.airplay_cant_play(id, problem);
            }
            return;
        }
        if problem == AirPlayProblem::NeedsPin {
            self.log.log(|| {
                format!(
                    "{} needs pairing with a PIN: the pairing starts",
                    self.airplay_name(id)
                )
            });
            if let Err(problem) = self.audio.start_airplay_pairing(id) {
                self.airplay_cant_play(id, problem);
            }
            return;
        }
        if problem == AirPlayProblem::PlaysInGroup
            && let Some(target) = self.audio.airplay_holding_target(id)
        {
            let message = NoticeMessage::AirPlayPlaysInGroup {
                speaker: self.airplay_name(id),
                target: target.name,
            };
            self.airplay_off(id, problem, message);
            return;
        }
        self.airplay_cant_play(id, problem);
    }

    /// Stops streaming when a speaker was switched off and none is left on
    /// (S118, S162).
    fn stop_if_idle(&mut self) {
        let action = self.streaming_after(SpeakerEvent::SwitchedOff);
        if action == StreamingAction::Stop {
            self.log
                .log(|| "streaming stops: no speaker is on anymore".to_owned());
            self.audio.stop();
        }
    }

    /// Starts streaming when a speaker was switched on while nothing streams
    /// (S119, S162).
    fn start_if_switched_on(&mut self) {
        let action = self.streaming_after(SpeakerEvent::SwitchedOn);
        if action == StreamingAction::Start {
            self.start_session(StartOrigin::User);
        }
    }

    /// Takes in what the audio backend reports. Returns whether it reported
    /// anything.
    fn take_events(&mut self) -> bool {
        let events = self.audio.poll();
        let any = !events.is_empty();
        for event in events {
            self.handle(event);
        }
        self.forget_stale_waits();
        any
    }

    /// Only a speaker that is on while streaming is on waits: turned off, or
    /// streaming stopped, its next start is a new attempt that hasn't waited
    /// yet.
    fn forget_stale_waits(&mut self) {
        if !self.running() {
            self.busy.clear();
            self.reconnecting.clear();
            return;
        }
        let outputs = self.audio.enabled_outputs();
        self.busy.retain(|id| outputs.contains(id));
        let airplay = self.audio.enabled_airplay();
        self.reconnecting.retain(|id, _| airplay.contains(id));
    }

    fn handle(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::OutputsChanged => {
                self.outputs_known = true;
                self.migrate_delays_if_due();
                self.outputs_listed();
            }
            // It stays on, and plays again once its device is back. A device
            // still listed only lost its stream (after a format change, say):
            // the next poll opens it again, so that needs no notice.
            BackendEvent::OutputLost { id } => {
                if !self.audio.outputs().iter().any(|output| output.id == id) {
                    let speaker = self.output_name(&id);
                    self.post(speaker_subject(&id), NoticeMessage::SpeakerLost { speaker });
                }
            }
            BackendEvent::OutputFailed { id, .. } => {
                self.busy.remove(&id);
                self.output_failed(&id, false);
            }
            // It stays on, and plays once the other app lets go of its
            // device; the notice then clears itself, and its row waits
            // meanwhile.
            BackendEvent::OutputBusy { id } => {
                self.busy.insert(id.clone());
                let speaker = self.output_name(&id);
                self.post(speaker_subject(&id), NoticeMessage::SpeakerBusy { speaker });
            }
            BackendEvent::OutputFree { id } => {
                self.busy.remove(&id);
                self.clear_if(speaker_subject(&id), |message| {
                    matches!(message, NoticeMessage::SpeakerBusy { .. })
                });
            }
            BackendEvent::Stopped { reason } => {
                let message = match reason {
                    StopReason::SourceQuit => {
                        // A dismissed double-audio notice was about this run
                        // of the app.
                        self.heard_twice_dismissed.remove(&self.chosen.key());
                        NoticeMessage::SourceQuit {
                            source: self.chosen_name(),
                        }
                    }
                    StopReason::CaptureFailed(BackendError::InputGone) => {
                        NoticeMessage::InputDisconnected {
                            source: self.chosen_name(),
                        }
                    }
                    StopReason::CaptureFailed(BackendError::MicrophoneBlocked) => {
                        NoticeMessage::MicrophoneBlocked {
                            source: self.chosen_name(),
                        }
                    }
                    StopReason::CaptureFailed(_) => NoticeMessage::CaptureLost,
                };
                self.post(NoticeSubject::Source, message);
                self.refresh_sources();
            }
            // The quit app's choice gives way to the whole system, as a
            // source the user picked would (S141, S227).
            BackendEvent::SwitchedToSystem => {
                let source = self.chosen_name();
                self.heard_twice_dismissed.remove(&self.chosen.key());
                self.chosen = Chosen::WholeSystem;
                self.notices.clear(&[NoticeSubject::Capture]);
                self.post(
                    NoticeSubject::Source,
                    NoticeMessage::SourceSwitched { source },
                );
                self.refresh_sources();
            }
            BackendEvent::HeardTwice { paths } => self.heard_twice_changed(paths),
            BackendEvent::AirPlayChanged => self.airplay_listed(),
            BackendEvent::CastChanged => {
                for speaker in self.audio.cast_speakers() {
                    self.cast_names.insert(speaker.id, speaker.name);
                }
            }
            BackendEvent::CastPlaying { id } => {
                self.notices.clear(&[NoticeSubject::Cast(id)]);
            }
            BackendEvent::CastFailed { id, problem } => self.cast_cant_play(&id, problem),
            BackendEvent::CastMemoryChanged => self.save_cast_memory(),
            BackendEvent::AirPlayPlaying { id } => {
                self.reconnecting.remove(&id);
                self.clear_if(airplay_subject(&id), |message| {
                    matches!(message, NoticeMessage::AirPlayRetrying { .. })
                });
            }
            BackendEvent::AirPlayTrouble { id, problem } => {
                let waiting = if problem == AirPlayProblem::OneAirPlay2Speaker {
                    Waiting::ForAirPlay2Slot
                } else {
                    Waiting::Reconnecting
                };
                self.reconnecting.insert(id.clone(), waiting);
                let speaker = self.airplay_name(&id);
                self.post(
                    airplay_subject(&id),
                    NoticeMessage::AirPlayRetrying {
                        speaker,
                        problem: ui_problem(problem),
                    },
                );
            }
            BackendEvent::AirPlayFailed { id, problem } => {
                self.reconnecting.remove(&id);
                self.airplay_cant_play(&id, problem);
            }
            BackendEvent::AirPlayMemberNeedsPassword { id, member } => {
                self.reconnecting.remove(&id);
                let message = NoticeMessage::AirPlayMemberNeedsPassword {
                    target: self.airplay_name(&id),
                    member,
                };
                self.airplay_off(&id, AirPlayProblem::NeedsPassword, message);
            }
            BackendEvent::AirPlayPaired { id } => {
                self.reconnecting.remove(&id);
                self.log
                    .log(|| format!("{} is paired and on", self.airplay_name(&id)));
                self.notices.clear(&[airplay_subject(&id)]);
                self.start_if_switched_on();
            }
            // Its Retry starts a new sender (`retry_airplay`).
            BackendEvent::AirPlayStopped => {
                self.reconnecting.clear();
                self.post(NoticeSubject::AirPlay, NoticeMessage::AirPlayStopped);
            }
            BackendEvent::AirPlayPtpFree => {
                self.notices.clear(&[NoticeSubject::AirPlayPtp]);
            }
            BackendEvent::AirPlayPtpPortsTaken => {
                self.post(
                    NoticeSubject::AirPlayPtp,
                    NoticeMessage::AirPlayPtpPortsTaken,
                );
            }
            // The next snapshot shows it; no notice, no focus change (S198).
            BackendEvent::DeviceVolumeChanged { .. } => {}
            // Told once while the writes keep failing (the backend reports
            // only the changes); a write that works clears it.
            BackendEvent::PairingStore { saved: false } => {
                self.post(NoticeSubject::PairingsFile, NoticeMessage::PairingsNotSaved);
            }
            BackendEvent::PairingStore { saved: true } => {
                self.notices.clear(&[NoticeSubject::PairingsFile]);
            }
            // The pairings in it were missing since launch: told once, with
            // where the old file is.
            BackendEvent::PairingsKeptAside { file } => {
                self.post(
                    NoticeSubject::PairingsKeptAside,
                    NoticeMessage::PairingsKeptAside { file },
                );
            }
        }
    }

    /// Takes in where the source is heard straight now: see the module
    /// documentation.
    fn heard_twice_changed(&mut self, paths: Vec<DirectPath>) {
        let subject = HeardTwiceSubject {
            source: self.chosen.key(),
            endpoints: paths
                .iter()
                .map(|path| (path.endpoint_id.clone(), path.endpoint_name.clone()))
                .collect(),
        };
        let dismissed = !paths.is_empty()
            && self
                .heard_twice_dismissed
                .get(&subject.source)
                .is_some_and(|dismissed| {
                    paths
                        .iter()
                        .all(|path| dismissed.contains(&path.endpoint_id))
                });
        if paths.is_empty() || dismissed {
            self.heard_twice = None;
            if self.notices.clear(&[NoticeSubject::DoubleAudio]) {
                self.log.log(|| {
                    "notice about DoubleAudio cleared: the source is heard only through \
                     AudioSync"
                        .to_owned()
                });
            }
            if dismissed {
                self.log.log(|| {
                    format!(
                        "no notice about DoubleAudio: the user dismissed it for {:?}",
                        subject.endpoints
                    )
                });
            }
            return;
        }
        let shown = |path: &DirectPath| {
            self.custom_names
                .shown(&speaker_key(&path.endpoint_id), &path.endpoint_name)
        };
        let names = |plays_here: bool| -> Vec<String> {
            paths
                .iter()
                .filter(|path| path.audiosync_plays_here == plays_here)
                .map(shown)
                .collect()
        };
        let message = NoticeMessage::HeardTwice {
            source: match &self.chosen {
                Chosen::WholeSystem => None,
                Chosen::App { name, .. } | Chosen::Input { name, .. } => Some(name.clone()),
            },
            mute: names(false),
            shared: names(true),
            default_output: paths
                .iter()
                .find(|path| path.reason == DirectReason::DefaultEndpoint)
                .map(shown),
        };
        let showing = self.heard_twice.as_ref() == Some(&subject)
            && self.notices.get(&NoticeSubject::DoubleAudio).is_some();
        if !showing {
            self.post(NoticeSubject::DoubleAudio, message);
        } else if let Some(posted) = self.notices.get_mut(&NoticeSubject::DoubleAudio)
            && posted.message != message
        {
            // The same outputs: only which of them AudioSync plays to
            // changed, so the notice changes where it is.
            self.log
                .log(|| format!("notice about DoubleAudio changed: {message:?}"));
            posted.message = message;
        }
        self.heard_twice = Some(subject);
    }

    /// Remembers that the user dismissed the double-audio notice showing.
    fn remember_heard_twice_dismissal(&mut self) {
        let Some(subject) = self.heard_twice.take() else {
            return;
        };
        self.log.log(|| {
            format!(
                "notice about DoubleAudio dismissed: it stays away for {} and {:?}",
                subject.source, subject.endpoints
            )
        });
        self.heard_twice_dismissed
            .entry(subject.source)
            .or_default()
            .extend(subject.endpoints.into_iter().map(|(id, _)| id));
    }

    /// The sources as the UI lists them: what is listed, and the chosen app
    /// after them while it isn't running, so the choice still shows.
    fn source_rows(&self) -> Vec<Source> {
        let mut rows: Vec<Source> = self
            .sources
            .iter()
            .map(|source| Source {
                id: source_key(source),
                name: source.name.clone(),
                kind: match source.id {
                    SourceId::WholeSystem => SourceKind::System,
                    SourceId::App { .. } => SourceKind::App,
                    SourceId::Input { .. } => SourceKind::Input,
                },
            })
            .collect();
        let chosen = self.chosen.key();
        if !rows.iter().any(|row| row.id == chosen) {
            let kind = match self.chosen {
                Chosen::Input { .. } => SourceKind::Input,
                _ => SourceKind::App,
            };
            // Apps come before input devices, as in the listing.
            let at = match kind {
                SourceKind::Input => rows.len(),
                _ => rows
                    .iter()
                    .position(|row| row.kind == SourceKind::Input)
                    .unwrap_or(rows.len()),
            };
            rows.insert(
                at,
                Source {
                    id: chosen,
                    name: self.chosen_name(),
                    kind,
                },
            );
        }
        rows
    }

    /// An AirPlay speaker's row. `prompted`: the speaker whose pairing has
    /// the PIN prompt; another one pairing meanwhile waits for it.
    fn airplay_speaker(&self, speaker: AirPlayInfo, prompted: Option<&str>) -> Speaker {
        let waiting = prompted.is_some_and(|prompted| prompted != speaker.id);
        let id = airplay_key(&speaker.id);
        let in_target = self
            .audio
            .airplay_holding_target(&speaker.id)
            .map(|target| InTarget {
                id: airplay_key(&target.id),
                name: target.name,
            });
        Speaker {
            auto_connect: self.auto_connect.contains(&id),
            name: self.custom_names.shown(&id, &speaker.name),
            system_name: speaker.name,
            id,
            kind: if speaker.airplay2 {
                SpeakerKind::AirPlay2
            } else {
                SpeakerKind::AirPlay
            },
            enabled: speaker.enabled,
            volume: speaker.volume,
            delay_ms: None,
            availability: if speaker.pairing && waiting {
                Availability::WaitingToPair
            } else if speaker.pairing {
                Availability::Pairing
            } else {
                availability(speaker.availability)
            },
            paired: speaker.paired,
            // An AirPlay speaker's volume is its own slider (S192).
            device_volume: None,
            waiting: self
                .reconnecting
                .get(&speaker.id)
                .copied()
                .filter(|_| speaker.enabled),
            cast: None,
            group: speaker.group,
            mic_left_out: None,
            in_target,
        }
    }

    /// A pairing's PIN prompt as the UI shows it.
    fn pin_prompt(&self, prompt: AirPlayPinPrompt) -> PinPrompt {
        let speaker_id = airplay_key(&prompt.id);
        PinPrompt {
            name: self.custom_names.shown(&speaker_id, &prompt.name),
            speaker_id,
            phase: match prompt.phase {
                PairingPhase::Starting => PinPhase::Starting,
                PairingPhase::WaitingForPin => PinPhase::Ready,
                PairingPhase::Checking => PinPhase::Checking,
            },
            wrong_pin: prompt.wrong_pin,
        }
    }

    fn speaker(&self, output: OutputInfo) -> Speaker {
        let id = speaker_key(&output.id);
        let mic_left_out = left_out(&output);
        Speaker {
            auto_connect: self.auto_connect.contains(&id),
            name: self.custom_names.shown(&id, &output.name),
            system_name: output.name,
            id,
            kind: speaker_kind(output.kind),
            enabled: output.enabled,
            volume: output.volume,
            // The reference has no delay setting (spec S181).
            delay_ms: (!output.is_reference).then_some(output.delay_ms),
            availability: Availability::Open,
            paired: false,
            // Read only while the output is on (S192).
            device_volume: output
                .device_volume
                .and_then(|device_volume| device_volume.hint()),
            waiting: (output.enabled && self.busy.contains(&output.id))
                .then_some(Waiting::ForDevice),
            cast: None,
            group: None,
            mic_left_out,
            in_target: None,
        }
    }

    /// The AirPlay delay field's value: L = L_s + A, while an AirPlay sender
    /// runs (spec S178, S181).
    fn airplay_delay_ms(&self) -> Option<f64> {
        self.audio
            .airplay_sender_latency_ms()
            .map(|sender| native_airplay_latency_ms(sender, self.audio.airplay_adjustment()))
    }

    /// The one-time migration of a format 1 settings file's delays (spec
    /// S180), once an output list is in: the saved trims become offsets
    /// relative to the reference, the reference's trim becomes the AirPlay
    /// delay adjustment, and the file is saved in the new format.
    fn migrate_delays_if_due(&mut self) {
        if !self.delays_to_migrate {
            return;
        }
        self.delays_to_migrate = false;
        let reference = self
            .audio
            .outputs()
            .into_iter()
            .find(|output| output.is_reference);
        let mut migrated = self.saved.clone();
        settings::migrate_delays(&mut migrated, reference.as_ref().map(|r| r.id.as_str()));
        for (id, speaker) in &migrated.outputs {
            self.audio.set_output_delay(id, speaker.delay_ms);
        }
        self.audio
            .set_airplay_adjustment(migrated.airplay_adjustment_ms);
        self.log.log(|| {
            let relative_to = reference.as_ref().map_or_else(
                || "no reference output".to_owned(),
                |r| format!("{} ({})", r.name, r.id),
            );
            format!(
                "settings: {} output delay(s) migrated to offsets relative to {relative_to}; \
                 AirPlay delay adjustment {} ms",
                migrated.outputs.len(),
                migrated.airplay_adjustment_ms
            )
        });
        self.saved = migrated;
        self.must_save = true;
        self.persist();
    }

    /// Hands every zone member's zone volume to the backend (S249), by kind
    /// under the backend's own IDs; a key this build can't parse plays
    /// nowhere and is left out.
    fn push_zone_volumes(&mut self) {
        let mut volumes = ZoneVolumes::default();
        for zone in self.zones.list() {
            for member in &zone.members {
                let (map, id) = match SpeakerKey::parse(member) {
                    Some(SpeakerKey::Local(id)) => (&mut volumes.outputs, id),
                    Some(SpeakerKey::AirPlay(id)) => (&mut volumes.airplay, id),
                    Some(SpeakerKey::Cast(id)) => (&mut volumes.cast, id),
                    None => continue,
                };
                map.insert(id, zone.volume);
            }
        }
        self.audio.set_zone_volumes(volumes);
    }
}

impl<A: AudioBackend> Backend for AudioAdapter<A> {
    fn snapshot(&self) -> AppState {
        let reference_name = self
            .audio
            .outputs()
            .into_iter()
            .find(|output| output.is_reference)
            .map(|output| {
                self.custom_names
                    .shown(&speaker_key(&output.id), &output.name)
            });
        let pin_prompt = self.audio.airplay_pin_prompt();
        let prompted = pin_prompt.as_ref().map(|prompt| prompt.id.as_str());
        let (speakers, hidden) = self.without_hidden(
            self.audio
                .outputs()
                .into_iter()
                .map(|output| self.speaker(output))
                .chain(
                    self.audio
                        .airplay_speakers()
                        .into_iter()
                        .map(|speaker| self.airplay_speaker(speaker, prompted)),
                )
                .chain(
                    self.audio
                        .cast_speakers()
                        .into_iter()
                        .map(|speaker| self.cast_speaker(speaker)),
                )
                .collect(),
        );
        let absent = self.absent_members(&speakers, &hidden);
        let mut state = AppState {
            reference_name,
            airplay_delay_ms: self.airplay_delay_ms(),
            airplay_none_found: self.airplay_none_found(),
            airplay_searching: self.airplay_searching(),
            running: self.running(),
            sources: self.source_rows(),
            source_id: self.chosen.key(),
            master_volume: self.audio.master_volume(),
            speakers,
            absent,
            hidden,
            zones: self.zones.list().to_vec(),
            equalizer: self.equalizer,
            equalizer_curves: self.equalizer_curves.clone(),
            presets: Vec::new(),
            current_preset: None,
            notices: self
                .notices
                .iter()
                .map(|(subject, posted)| Notice {
                    id: posted.id,
                    subject: format!("{subject:?}"),
                    message: posted.message.clone(),
                    time: posted.time.clone(),
                })
                .collect(),
            pin_prompt: pin_prompt.map(|prompt| self.pin_prompt(prompt)),
            password_prompt: self.audio.airplay_password_prompt().map(|prompt| {
                let speaker_id = airplay_key(&prompt.id);
                PasswordPrompt {
                    name: self.custom_names.shown(&speaker_id, &prompt.name),
                    speaker_id,
                    wrong: prompt.wrong,
                }
            }),
            when_source_quits: self.when_source_quits,
            cast_list: self.cast_list(),
            cast_by_address: self.audio.cast_discovery() != CastDiscovery::Off,
            calibration: CalibrationState {
                speaker_id: self.calibration.id.as_deref().map(speaker_key),
                phase: self.calibration.phase,
                busy: self.calibration_run.is_some() || self.calibrate_all.run.is_some(),
            },
            calibrate_all: self.calibrate_all_state(),
        };
        ui_model::show_presets(&mut state, &self.presets);
        state
    }

    fn set_running(&mut self, on: bool) {
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!("the user switched streaming {state}")
        });
        if !on {
            if self.running() {
                self.gate.user_switched_off();
                self.audio.stop();
            }
            self.forget_stale_waits();
            return;
        }
        let action = self.streaming_after(SpeakerEvent::StartRequested);
        match action {
            // Without a speaker on, the start asks for one.
            StreamingAction::Start | StreamingAction::AskForSpeaker => {
                self.start_session(StartOrigin::User);
            }
            StreamingAction::Stop => self.audio.stop(),
            StreamingAction::Nothing => {}
        }
    }

    /// A new source clears the old one's notices; while streaming, capture
    /// starts again from it (S141, S163).
    fn select_source(&mut self, id: &str) {
        let Some(picked) = self
            .sources
            .iter()
            .find(|source| source_key(source) == id)
            .map(Chosen::from_source)
        else {
            return;
        };
        if picked.key() == self.chosen.key() {
            return;
        }
        self.log.log(|| {
            let name = match &picked {
                Chosen::WholeSystem => WHOLE_SYSTEM_NAME,
                Chosen::App { name, .. } | Chosen::Input { name, .. } => name,
            };
            format!("source chosen: {name} ({})", picked.key())
        });
        self.chosen = picked;
        self.notices
            .clear(&[NoticeSubject::Source, NoticeSubject::Capture]);
        // A start ends the running session by itself and keeps the AirPlay
        // speakers in the stream, restarting only their timeline (S141); a
        // stop would tear them down and connect them again.
        let switch = source_switch(self.running());
        let started = switch
            .start
            .is_some_and(|origin| self.start_session(origin));
        if switch.stop_keeping_airplay && !started && self.running() {
            self.audio.stop();
        }
    }

    fn set_master_volume(&mut self, position: f64) {
        self.audio.set_master_volume(position);
    }

    fn set_speaker_enabled(&mut self, id: &str, on: bool) {
        // The user switches it now: a failure is about that, not the recall.
        if let Some(watch) = &mut self.recall_watch {
            watch.keys.retain(|watched| watched != id);
        }
        let endpoint_id = match SpeakerKey::parse(id) {
            Some(SpeakerKey::Local(endpoint_id)) => endpoint_id,
            Some(SpeakerKey::AirPlay(airplay_id)) => {
                self.set_airplay_enabled(&airplay_id, on);
                return;
            }
            Some(SpeakerKey::Cast(cast_id)) => {
                self.set_cast_enabled(&cast_id, on);
                return;
            }
            None => return,
        };
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!(
                "the user switched {} {state}",
                self.output_name(&endpoint_id)
            )
        });
        if on {
            // A new attempt.
            self.notices.clear(&[speaker_subject(&endpoint_id)]);
        }
        let result = self.audio.set_output_enabled(&endpoint_id, on);
        if result.is_err() {
            self.output_failed(&endpoint_id, true);
        }
        if on {
            self.start_if_switched_on();
        } else {
            self.stop_if_idle();
        }
        self.take_events();
    }

    fn set_speaker_volume(&mut self, id: &str, position: f64) {
        match SpeakerKey::parse(id) {
            Some(SpeakerKey::Local(endpoint_id)) => {
                self.audio.set_output_volume(&endpoint_id, position);
            }
            Some(SpeakerKey::AirPlay(airplay_id)) => {
                self.audio.set_airplay_volume(&airplay_id, position);
            }
            Some(SpeakerKey::Cast(cast_id)) => self.audio.set_cast_volume(&cast_id, position),
            None => {}
        }
    }

    /// A local output's delay, an AirPlay speaker's own delay d_s, or a
    /// Cast speaker's measured offset (never negative).
    fn set_speaker_delay(&mut self, id: &str, ms: f64) {
        match SpeakerKey::parse(id) {
            Some(SpeakerKey::Local(endpoint_id)) => {
                let ms = clamp_delay_ms(ms);
                // A step at the range's end sets the offset it already has,
                // which mustn't take the calibration's line away.
                let changed = !self
                    .audio
                    .outputs()
                    .iter()
                    .any(|output| output.id == endpoint_id && output.delay_ms == ms);
                self.audio.set_output_delay(&endpoint_id, ms);
                if changed {
                    self.calibration = self.calibration.offset_changed(&endpoint_id);
                }
            }
            Some(SpeakerKey::AirPlay(airplay_id)) => {
                self.audio
                    .set_airplay_delay(&airplay_id, clamp_delay_ms(ms));
            }
            Some(SpeakerKey::Cast(cast_id)) => {
                self.audio
                    .set_cast_offset(&cast_id, clamp_delay_ms(ms).max(0.0));
            }
            None => {}
        }
    }

    fn set_speaker_muted(&mut self, id: &str, muted: bool) {
        if let Some(cast_id) = cast_id(id) {
            self.log.log(|| {
                let state = if muted { "muted" } else { "unmuted" };
                format!("the user {state} {}", self.cast_name(cast_id))
            });
            self.audio.set_cast_muted(cast_id, muted);
        }
    }

    fn resync_speaker(&mut self, id: &str) {
        if let Some(cast_id) = cast_id(id) {
            self.audio.resync_cast(cast_id);
        }
    }

    fn check_cast_address(&mut self, text: &str) -> Result<CastAddressCheck, CastAddressError> {
        let Some(address) = CastAddress::parse(text) else {
            self.log
                .log(|| "Cast: Add by Address turned down what was typed".to_owned());
            return Err(CastAddressError::Invalid);
        };
        Ok(CastAddressCheck {
            address,
            answers: self.cast_probe,
        })
    }

    fn add_checked_cast_address(
        &mut self,
        address: CastAddress,
        answered: bool,
    ) -> Result<(), CastAddressError> {
        if !answered {
            self.log
                .log(|| format!("Cast: nothing answers at {}", address.text()));
            return Err(CastAddressError::NotFound);
        }
        cast_address::add(&mut self.cast_addresses, &address);
        self.audio.add_cast_address(address);
        self.take_events();
        Ok(())
    }

    fn remove_cast_address(&mut self, id: &str) {
        let Some(text) = cast_id(id).and_then(|cast_id| {
            self.audio
                .cast_speakers()
                .into_iter()
                .find(|speaker| speaker.id == cast_id)?
                .address
        }) else {
            return;
        };
        let Some(address) = CastAddress::parse(&text) else {
            return;
        };
        cast_address::remove(&mut self.cast_addresses, &address);
        self.audio.remove_cast_address(&address);
        self.take_events();
        self.stop_if_idle();
    }

    /// Stores the adjustment that makes L what the user entered, with the
    /// sender's latency of this moment (spec S178, S181).
    fn set_airplay_delay(&mut self, ms: f64) {
        let Some(sender) = self.audio.airplay_sender_latency_ms() else {
            return;
        };
        if let Some(adjustment) = native_airplay_adjustment_ms(ms, sender) {
            self.audio.set_airplay_adjustment(adjustment);
        }
    }

    fn rename_speaker(&mut self, id: &str, name: &str) {
        let system = match SpeakerKey::parse(id) {
            Some(SpeakerKey::Local(endpoint_id)) => self.names.get(&endpoint_id).cloned(),
            Some(SpeakerKey::AirPlay(airplay_id)) => self.airplay_names.get(&airplay_id).cloned(),
            Some(SpeakerKey::Cast(cast_id)) => self.cast_names.get(&cast_id).cloned(),
            None => None,
        };
        // A speaker never listed has no system name to come back to.
        let Some(system) = system else {
            return;
        };
        if self.custom_names.rename(id, name, &system) {
            let shown = self.custom_names.shown(id, &system);
            self.log
                .log(|| format!("speaker {id} ({system}) is shown as {shown}"));
        }
    }

    fn hide_speaker(&mut self, id: &str, kind: &str) {
        let state = self.snapshot();
        let Some(speaker) = state.speaker(id) else {
            return;
        };
        let reference = self
            .audio
            .outputs()
            .into_iter()
            .any(|output| output.is_reference && speaker_key(&output.id) == id);
        if reference {
            return;
        }
        let was_on = speaker.enabled;
        let name = speaker.name.clone();
        if state
            .pin_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.speaker_id == id)
        {
            self.cancel_pairing(id);
        }
        if state
            .password_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.speaker_id == id)
        {
            self.cancel_password(id);
        }
        if self
            .snapshot()
            .speaker(id)
            .is_some_and(|speaker| speaker.enabled)
        {
            self.set_speaker_enabled(id, false);
        }
        // Its row is gone, and so is what it said.
        let subject = match SpeakerKey::parse(id) {
            Some(SpeakerKey::Local(endpoint_id)) => Some(speaker_subject(&endpoint_id)),
            Some(SpeakerKey::AirPlay(airplay_id)) => Some(airplay_subject(&airplay_id)),
            Some(SpeakerKey::Cast(cast_id)) => Some(NoticeSubject::Cast(cast_id)),
            None => None,
        };
        if let Some(subject) = subject {
            self.notices.clear(&[subject]);
        }
        self.hidden.push(SavedHidden {
            key: id.to_owned(),
            name,
            kind: kind.to_owned(),
        });
        self.log.log(|| {
            let state = if was_on { "on" } else { "off" };
            format!("speaker {id} hidden; it was {state}")
        });
    }

    fn unhide_speaker(&mut self, id: &str) {
        let before = self.hidden.len();
        self.hidden.retain(|entry| entry.key != id);
        if self.hidden.len() < before {
            self.log.log(|| format!("speaker {id} shown again"));
        }
    }

    fn set_auto_connect(&mut self, id: &str, on: bool) {
        self.log.log(|| {
            let state = if on { "on" } else { "off" };
            format!("auto-connect {state} for {id}")
        });
        if on {
            self.auto_connect.insert(id.to_owned());
        } else {
            self.auto_connect.remove(id);
        }
    }

    /// Present members only; absent ones are left alone.
    /// The core's plan for a click on zone `id` (spec S246, S247), carried
    /// out: present members only, at most one Cast speaker, every AirPlay 2
    /// member on even when it waits for the AirPlay 2 place.
    fn toggle_zone(&mut self, id: &str) {
        let state = self.snapshot();
        let Some(zone) = self.zones.get(id).cloned() else {
            return;
        };
        let in_zone = |key: &str| zone.members.iter().any(|member| member == key);
        let elsewhere = |kind: SpeakerKind| {
            state.speakers.iter().filter(move |speaker| {
                speaker.kind == kind && speaker.enabled && !in_zone(&speaker.id)
            })
        };
        let situation = Situation {
            streaming: state.running,
            cast_on_elsewhere: elsewhere(SpeakerKind::Cast).next().is_some(),
            airplay2_playing_elsewhere: state.running
                && elsewhere(SpeakerKind::AirPlay2).any(|speaker| speaker.waiting.is_none()),
        };
        let members: Vec<(SpeakerKey, bool, zones::Member)> = zone
            .members
            .iter()
            .filter_map(|text| {
                let key = SpeakerKey::parse(text)?;
                let listed = state.speaker(text);
                let kind = match (&key, listed.map(|speaker| speaker.kind)) {
                    (SpeakerKey::Local(_), _) => MemberKind::Local,
                    (SpeakerKey::Cast(_), _) => MemberKind::Cast,
                    (SpeakerKey::AirPlay(_), Some(SpeakerKind::AirPlay2)) => MemberKind::AirPlay2,
                    (SpeakerKey::AirPlay(id), _) if is_airplay_target(id) => MemberKind::AirPlay2,
                    (SpeakerKey::AirPlay(_), _) => MemberKind::AirPlay,
                };
                let on = listed.is_some_and(|speaker| speaker.enabled);
                let member = zones::Member {
                    kind,
                    listed: listed.is_some(),
                    on,
                    busy: matches!(&key, SpeakerKey::Cast(id) if self.cast_taken_over(id)),
                    needs_airplay2: listed.is_none()
                        && !self.view.airplay2_experimental
                        && matches!(&key, SpeakerKey::AirPlay(id) if is_airplay_target(id)),
                    in_target: matches!(&key, SpeakerKey::AirPlay(id)
                        if self.audio.airplay_holding_target(id).is_some()),
                };
                Some((key, on, member))
            })
            .collect();
        let facts: Vec<zones::Member> = members.iter().map(|(_, _, member)| *member).collect();
        let plan = zones::click_plan(&facts, situation);
        let turn_on = match plan.action {
            zones::ClickAction::Nothing => return,
            zones::ClickAction::TurnOn { .. } => true,
            zones::ClickAction::TurnOff => false,
        };
        self.log.log(|| {
            let switched = if turn_on { "on" } else { "off" };
            format!("the user switched zone \"{}\" {switched}", zone.name)
        });
        let running = self.running();
        for ((key, on, _), step) in members.iter().zip(&plan.steps) {
            let switch_on = match step {
                // While streaming is off every member starts a new attempt.
                Step::On | Step::Waiting if !running || !on => true,
                Step::Off | Step::CastLeftOff(_) if *on => false,
                Step::CastLeftOff(reason) => {
                    self.log
                        .log(|| format!("zone \"{}\": {key} stays off ({reason:?})", zone.name));
                    continue;
                }
                _ => continue,
            };
            self.switch_zone_member(key, switch_on);
        }
        if turn_on {
            self.start_if_switched_on();
        } else {
            self.stop_if_idle();
        }
        self.take_events();
    }

    fn new_zone(&mut self, name: &str) -> Result<(), ZoneError> {
        let members: Vec<String> = self
            .snapshot()
            .speakers
            .into_iter()
            .filter(|speaker| speaker.enabled)
            .map(|speaker| speaker.id)
            .collect();
        if members.is_empty() {
            return Err(ZoneError::NoActiveSpeakers);
        }
        let name = self.add_zone(name, &members);
        self.log
            .log(|| format!("zone \"{name}\" made of {} speaker(s)", members.len()));
        Ok(())
    }

    fn new_zone_with(&mut self, speaker_id: &str, name: &str) {
        let name = self.add_zone(name, &[speaker_id.to_owned()]);
        self.log
            .log(|| format!("zone \"{name}\" made of {speaker_id}"));
    }

    fn rename_zone(&mut self, id: &str, name: &str) {
        self.zones.rename(id, name);
    }

    fn delete_zone(&mut self, id: &str) {
        if self.zones.delete(id) {
            self.log.log(|| format!("zone {id} deleted"));
            self.push_zone_volumes();
        }
    }

    fn move_to_zone(&mut self, speaker_id: &str, zone_id: &str) {
        if self.zones.move_speaker(speaker_id, zone_id) {
            self.log
                .log(|| format!("{speaker_id} moved to zone {zone_id}"));
            self.push_zone_volumes();
        }
    }

    fn remove_from_zone(&mut self, speaker_id: &str) {
        if self.zones.remove_speaker(speaker_id) {
            self.log.log(|| format!("{speaker_id} left its zone"));
            self.push_zone_volumes();
        }
    }

    fn set_zone_volume(&mut self, id: &str, volume: f64) {
        if self.zones.set_volume(id, volume) {
            self.push_zone_volumes();
        }
    }

    fn use_target_instead(&mut self, speaker_id: &str) {
        let Some(target) = self
            .snapshot()
            .speaker(speaker_id)
            .and_then(|speaker| speaker.in_target.clone())
        else {
            return;
        };
        self.log
            .log(|| format!("zone: {} takes the place of {speaker_id}", target.id));
        self.zones.rename_key(speaker_id, &target.id);
        self.push_zone_volumes();
    }

    fn set_equalizer(&mut self, settings: EqSettings) {
        self.equalizer = settings.quantized();
        self.audio.set_equalizer(self.equalizer);
    }

    fn set_equalizer_curves(&mut self, curves: Vec<UserCurve>) {
        self.equalizer_curves = sanitized_curves(curves);
    }

    fn save_preset(&mut self, name: &str) -> Result<(), PresetError> {
        if self.presets.len() >= ui_model::MAX_PRESETS {
            return Err(PresetError::Full);
        }
        let id = ui_model::new_preset_id(&self.presets);
        let preset = self.capture_preset(&id, name)?;
        self.log.log(|| {
            format!(
                "preset \"{}\" saved with {} speaker(s)",
                preset.name,
                preset.members.len()
            )
        });
        self.presets.push(preset);
        Ok(())
    }

    fn rename_preset(&mut self, id: &str, name: &str) {
        let Some(preset) = self.presets.iter_mut().find(|preset| preset.id == id) else {
            return;
        };
        preset.name = presets::renamed_preset(&preset.name, name);
        let name = preset.name.clone();
        // The notice names the preset, so it follows the new name in place.
        if self
            .preset_notice
            .as_ref()
            .is_some_and(|cause| cause.id == id)
            && let Some(posted) = self.notices.get_mut(&NoticeSubject::Presets)
            && let NoticeMessage::PresetLeftOff { preset, .. } = &mut posted.message
        {
            *preset = name;
        }
    }

    fn update_preset(&mut self, id: &str) -> Result<(), PresetError> {
        let Some(index) = self.presets.iter().position(|preset| preset.id == id) else {
            return Ok(());
        };
        let name = self.presets[index].name.clone();
        self.presets[index] = self.capture_preset(id, &name)?;
        self.log.log(|| format!("preset \"{name}\" updated"));
        Ok(())
    }

    fn delete_preset(&mut self, id: &str) {
        self.presets.retain(|preset| preset.id != id);
        self.log.log(|| format!("preset {id} deleted"));
        if self
            .preset_notice
            .as_ref()
            .is_some_and(|cause| cause.id == id)
        {
            self.clear_preset_notice();
        }
    }

    fn recall_preset(&mut self, id: &str) {
        let _ = AudioAdapter::recall_preset(self, id);
    }

    fn dismiss_notice(&mut self, id: u64) {
        let subject = self
            .notices
            .iter()
            .find(|(_, posted)| posted.id == id)
            .map(|(subject, _)| subject.clone());
        if let Some(subject) = subject {
            if subject == NoticeSubject::DoubleAudio {
                self.remember_heard_twice_dismissal();
            }
            self.notices.clear(&[subject]);
        }
    }

    fn dismiss_all_notices(&mut self) {
        if self.notices.get(&NoticeSubject::DoubleAudio).is_some() {
            self.remember_heard_twice_dismissal();
        }
        self.notices.clear_all();
    }

    fn refresh_devices(&mut self) {
        self.log
            .log(|| "the user asked to list the devices again".to_owned());
        self.list_devices();
    }

    /// Only a local output has a device volume to fix; the audio backend
    /// logs what it wrote (S201), and the row changes once Windows reports
    /// the change.
    fn fix_device_volume(&mut self, id: &str) {
        let Some(SpeakerKey::Local(endpoint_id)) = SpeakerKey::parse(id) else {
            return;
        };
        self.audio.fix_device_volume(&endpoint_id);
        self.take_events();
    }

    /// The speakers that were on come back as they do after a launch: the
    /// backend kept their settings, and the auto-connect ones switch on as
    /// the new sender lists them.
    fn retry_airplay(&mut self) {
        self.log
            .log(|| "the user asked to start AirPlay again (Retry)".to_owned());
        self.notices.clear(&[NoticeSubject::AirPlay]);
        self.audio.retry_airplay();
        self.take_events();
    }

    /// The PIN itself is never logged.
    fn submit_pin(&mut self, id: &str, pin: &str) {
        let Some(SpeakerKey::AirPlay(airplay_id)) = SpeakerKey::parse(id) else {
            return;
        };
        let sent = self.audio.submit_airplay_pin(&airplay_id, pin);
        self.log.log(|| {
            let what = if sent { "sent" } else { "not taken" };
            format!(
                "the user typed the PIN for {}: {what}",
                self.airplay_name(&airplay_id)
            )
        });
    }

    fn cancel_pairing(&mut self, id: &str) {
        let Some(SpeakerKey::AirPlay(airplay_id)) = SpeakerKey::parse(id) else {
            return;
        };
        self.log.log(|| {
            format!(
                "the user cancelled the pairing with {}",
                self.airplay_name(&airplay_id)
            )
        });
        self.audio.cancel_airplay_pairing(&airplay_id);
        self.take_events();
    }

    /// The password itself is never logged.
    fn submit_password(&mut self, id: &str, password: &str) {
        let Some(SpeakerKey::AirPlay(airplay_id)) = SpeakerKey::parse(id) else {
            return;
        };
        let taken = self.audio.submit_airplay_password(&airplay_id, password);
        self.log.log(|| {
            let what = if taken { "taken" } else { "not taken" };
            format!(
                "the user typed the password for {}: {what}",
                self.airplay_name(&airplay_id)
            )
        });
        if taken {
            self.notices.clear(&[airplay_subject(&airplay_id)]);
            self.start_if_switched_on();
        }
        self.take_events();
    }

    fn cancel_password(&mut self, id: &str) {
        let Some(SpeakerKey::AirPlay(airplay_id)) = SpeakerKey::parse(id) else {
            return;
        };
        self.log.log(|| {
            format!(
                "the user cancelled the password prompt of {}",
                self.airplay_name(&airplay_id)
            )
        });
        self.audio.cancel_airplay_password(&airplay_id);
        self.stop_if_idle();
        self.take_events();
    }

    fn forget_pairing(&mut self, id: &str) {
        let Some(SpeakerKey::AirPlay(airplay_id)) = SpeakerKey::parse(id) else {
            return;
        };
        self.log.log(|| {
            format!(
                "the user forgot the pairing with {}",
                self.airplay_name(&airplay_id)
            )
        });
        self.audio.forget_airplay_pairing(&airplay_id);
        self.notices.clear(&[airplay_subject(&airplay_id)]);
        self.stop_if_idle();
        self.take_events();
    }

    /// Also writes the settings file when anything changed since the last
    /// poll, so a slider drag is saved about twice a second at most.
    fn poll(&mut self) -> bool {
        let mut changed = self.take_events();
        changed |= self.check_sound_output();
        changed |= self.check_airplay_ahead();
        self.polls += 1;
        if self.polls >= SOURCE_POLLS {
            self.polls = 0;
            changed |= self.refresh_sources();
        }
        changed |= self.count_airplay_quiet();
        changed |= self.count_cast_quiet();
        changed |= self.keep_settings_aside(true);
        changed |= self.tick_calibration();
        changed |= self.follow_calibration_context();
        changed |= self.follow_calibrate_all();
        changed |= self.follow_preset_notice();
        changed |= self.persist();
        changed
    }

    fn start_calibration(&mut self, id: &str) {
        let Some(SpeakerKey::Local(endpoint_id)) = SpeakerKey::parse(id) else {
            return;
        };
        if self.calibration_run.is_some() || self.calibrate_all.run.is_some() {
            return;
        }
        let outputs = self.audio.outputs();
        let output = outputs.iter().find(|output| output.id == endpoint_id);
        if output.and_then(left_out).is_some() {
            return;
        }
        let started = CalibrationRun::start(
            &mut self.audio,
            &endpoint_id,
            clock::now_ns(),
            self.log.clone(),
        );
        match started {
            Ok(run) => {
                let phase = run.phase();
                self.calibration_run = Some(run);
                self.show_calibration(&endpoint_id, phase);
            }
            Err(failure) => {
                self.show_calibration(&endpoint_id, calibration::Phase::Failed(failure))
            }
        }
    }

    fn cancel_calibration(&mut self) {
        if let Some(run) = self.calibration_run.as_mut() {
            run.cancel(&mut self.audio, clock::now_ns());
        }
        let _ = self.tick_calibration();
    }

    fn start_calibrate_all(&mut self) {
        if self.calibration_run.is_some() || self.calibrate_all.run.is_some() {
            return;
        }
        let started = AllRun::start(
            &mut self.audio,
            &self.calibrate_all.included,
            clock::now_ns(),
            self.log.clone(),
        );
        match started {
            Ok(run) => {
                self.calibrate_all.phase = AllPhase::Idle;
                self.calibrate_all.sheet = None;
                self.calibrate_all.run = Some(run);
            }
            Err(failure) => {
                self.calibrate_all.phase = AllPhase::Failed(failure);
                self.calibrate_all.endings += 1;
            }
        }
    }

    fn cancel_calibrate_all(&mut self) {
        if let Some(run) = self.calibrate_all.run.as_mut() {
            run.cancel(&mut self.audio, clock::now_ns());
        }
        let _ = self.tick_calibrate_all();
    }

    fn measure_again(&mut self, row: usize) {
        if self.calibration_run.is_some() || self.calibrate_all.run.is_some() {
            return;
        }
        let name = self
            .calibrate_all
            .sheet
            .as_ref()
            .and_then(|kept| kept.members.get(row))
            .map(|member| self.member_name(member));
        let Some(kept) = self.calibrate_all.sheet.as_mut() else {
            return;
        };
        let started = AllRun::again(
            &mut self.audio,
            &kept.sheet,
            &kept.members,
            kept.rate,
            row,
            clock::now_ns(),
            self.log.clone(),
        );
        match started {
            Ok(run) => {
                kept.again_ended = None;
                self.calibrate_all.phase = AllPhase::Idle;
                self.calibrate_all.run = Some(run);
            }
            Err(failure) => {
                self.calibrate_all.phase = match (failure, name) {
                    (Failure::NotPlaying, Some(name)) => AllPhase::CannotMeasureAgain(name),
                    (failure, _) => AllPhase::Failed(failure),
                };
                self.calibrate_all.endings += 1;
            }
        }
    }

    fn apply_calibrate_all(&mut self) {
        if self.calibrate_all.run.is_some() || self.calibrate_all_waits() {
            return;
        }
        let Some(kept) = self.calibrate_all.sheet.as_mut() else {
            return;
        };
        let applied = match kept.sheet.apply() {
            Ok(applied) => applied,
            Err(refusal) => {
                self.log
                    .log(|| format!("Calibrate All: Apply refused: {refusal:?}"));
                return;
            }
        };
        let members = kept.members.clone();
        self.log
            .log(|| format!("Calibrate All applied: {:?}", applied.changes));
        for change in &applied.changes {
            self.write_calibrated(&members, &change.target, Some(change.new_ms));
        }
        if let Some(kept) = self.calibrate_all.sheet.as_mut() {
            kept.applied = Some(applied);
        }
        self.calibrate_all.endings += 1;
    }

    fn undo_calibrate_all(&mut self) {
        if self.calibrate_all.run.is_some() || self.calibrate_all_waits() {
            return;
        }
        let Some(kept) = self.calibrate_all.sheet.as_mut() else {
            return;
        };
        let Some(applied) = kept.applied.take() else {
            return;
        };
        kept.undone = true;
        let members = kept.members.clone();
        let previous = applied.undo.undo();
        self.log
            .log(|| format!("Calibrate All undone: {previous:?}"));
        for (target, value) in &previous {
            self.write_calibrated(&members, target, *value);
        }
        self.calibrate_all.endings += 1;
    }

    fn include_in_calibration(&mut self, id: &str, included: bool) {
        if self.calibrate_all.run.is_some() {
            return;
        }
        let ids = &mut self.calibrate_all.included;
        if included {
            ids.insert(id.to_owned());
        } else {
            ids.remove(id);
        }
    }

    fn close_calibrate_all(&mut self) {
        if self.calibrate_all.run.is_none() {
            self.calibrate_all = CalibrateAll {
                endings: self.calibrate_all.endings,
                ..CalibrateAll::default()
            };
        }
    }

    fn tick_calibration(&mut self) -> bool {
        let all = self.tick_calibrate_all();
        let Some(run) = self.calibration_run.as_mut() else {
            return all;
        };
        let changed = run.tick(&mut self.audio, clock::now_ns());
        let chosen = run.chosen().to_owned();
        let phase = run.phase();
        if !run.holds_outputs() {
            self.calibration_run = None;
        }
        let before = self.calibration.clone();
        if before.phase != phase || before.id.as_deref() != Some(chosen.as_str()) {
            self.show_calibration(&chosen, phase);
        } else {
            self.calibration.holds_outputs = self.calibration_run.is_some();
        }
        all || changed || before != self.calibration
    }

    fn view_settings(&self) -> ViewSettings {
        self.view
    }

    fn set_view_settings(&mut self, settings: ViewSettings) {
        self.view = settings;
    }

    fn save_settings(&mut self) {
        let _ = self.persist();
        // Before the backend drops and the Cast host shuts down, so the last
        // offset change is kept even when its event was never polled.
        self.save_cast_memory();
    }

    fn set_when_source_quits(&mut self, setting: WhenSourceQuits) {
        self.log.log(|| {
            format!(
                "when the source app quits: {} (was {})",
                setting.stored(),
                self.when_source_quits.stored()
            )
        });
        self.when_source_quits = setting;
        self.audio.set_when_source_quits(setting);
    }
}

/// The kind a local output shows as; "This PC" lists the built-in ones.
fn speaker_kind(kind: OutputKind) -> SpeakerKind {
    match kind {
        OutputKind::BuiltIn => SpeakerKind::BuiltIn,
        OutputKind::Headphones => SpeakerKind::Headphones,
        OutputKind::Bluetooth => SpeakerKind::Bluetooth,
        OutputKind::Usb => SpeakerKind::Usb,
        OutputKind::UsbHeadphones => SpeakerKind::UsbHeadphones,
        OutputKind::UsbSpeakers => SpeakerKind::UsbSpeakers,
        OutputKind::Hdmi => SpeakerKind::Hdmi,
        OutputKind::DisplayPort => SpeakerKind::DisplayPort,
        OutputKind::Virtual => SpeakerKind::Virtual,
        OutputKind::Other => SpeakerKind::Other,
    }
}

/// An output's name without the number Windows may put before the device's
/// name inside its parentheses (digits, then "- "), as it does for a USB
/// device in another port or a second device of the same model:
/// "Speakers (2- USB DAC)" gives "Speakers (USB DAC)".
fn unnumbered(name: &str) -> String {
    let mut plain = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(open) = rest.find('(') {
        plain.push_str(&rest[..=open]);
        rest = &rest[open + 1..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0
            && let Some(after) = rest[digits..].strip_prefix("- ")
        {
            rest = after;
        }
    }
    plain.push_str(rest);
    plain
}

/// The kind microphone calibration leaves `output` out for, if any. The
/// reference is never left out: it is what the others are measured against.
fn left_out(output: &OutputInfo) -> Option<MicKind> {
    let kind = output.traits.mic_kind();
    (kind != MicKind::Speaker && !output.is_reference).then_some(kind)
}

/// The UI's ID of a local output.
fn speaker_key(endpoint_id: &str) -> String {
    SpeakerKey::Local(endpoint_id.to_owned()).to_string()
}

/// The UI's ID of an AirPlay speaker.
fn airplay_key(id: &str) -> String {
    SpeakerKey::AirPlay(id.to_owned()).to_string()
}

/// The UI's ID of a Google Cast speaker.
fn cast_key(id: &str) -> String {
    SpeakerKey::Cast(id.to_owned()).to_string()
}

/// The device ID in a Cast speaker's UI ID; `None` for any other speaker.
fn cast_id(key: &str) -> Option<&str> {
    SpeakerKey::cast_id(key)
}

fn ui_cast_problem(problem: CastProblem) -> ui_model::CastProblem {
    use ui_model::CastProblem as P;
    match problem {
        CastProblem::Busy => P::Busy,
        CastProblem::NotResponding => P::NotResponding,
        CastProblem::Refused => P::Refused,
        CastProblem::Blocked => P::Blocked,
        CastProblem::TakenOver => P::TakenOver,
        CastProblem::ConnectionLost => P::ConnectionLost,
    }
}

/// Whether an AirPlay ID is a stereo pair's or home theater's, which plays
/// over AirPlay 2 only (spec S149, P176).
fn is_airplay_target(id: &str) -> bool {
    id.starts_with("pair:") || id.starts_with("theater:")
}

/// What a notice about one local output is about.
fn speaker_subject(endpoint_id: &str) -> NoticeSubject {
    NoticeSubject::Speaker(SpeakerKey::Local(endpoint_id.to_owned()))
}

/// What a notice about one AirPlay speaker is about.
fn airplay_subject(id: &str) -> NoticeSubject {
    NoticeSubject::Speaker(SpeakerKey::AirPlay(id.to_owned()))
}

/// How an AirPlay speaker's row shows whether it can play.
fn availability(availability: AirPlayAvailability) -> Availability {
    match availability {
        AirPlayAvailability::Open => Availability::Open,
        AirPlayAvailability::NeedsPassword => Availability::NeedsCode,
        AirPlayAvailability::NeedsPin => Availability::NeedsCode,
        AirPlayAvailability::PairingNotYet => Availability::PairingNotYet,
        AirPlayAvailability::AirPlay2NotYet => Availability::AirPlay2NotYet,
        AirPlayAvailability::Unsupported => Availability::Unsupported,
    }
}

/// An AirPlay problem as a notice words it.
fn ui_problem(problem: AirPlayProblem) -> ui_model::AirPlayProblem {
    use ui_model::AirPlayProblem as P;
    match problem {
        AirPlayProblem::NotResponding => P::NotResponding,
        AirPlayProblem::ConnectionLost => P::ConnectionLost,
        AirPlayProblem::Busy => P::Busy,
        AirPlayProblem::Refused => P::Refused,
        AirPlayProblem::NeedsPassword => P::NeedsPassword,
        AirPlayProblem::NeedsPin => P::NeedsPin,
        AirPlayProblem::PairingFailed => P::PairingFailed,
        AirPlayProblem::PairingNotYet => P::PairingNotYet,
        AirPlayProblem::AirPlay2NotYet => P::AirPlay2NotYet,
        AirPlayProblem::TimingUnavailable => P::TimingUnavailable,
        AirPlayProblem::OneAirPlay2Speaker => P::OneAirPlay2Speaker,
        AirPlayProblem::PlaysThroughSpeakers => P::PlaysThroughSpeakers,
        AirPlayProblem::AppleTvPlays => P::AppleTvPlays,
        AirPlayProblem::PlaysInGroup => P::PlaysInGroup,
        AirPlayProblem::Unsupported => P::Unsupported,
    }
}

/// A listed source as the debug log names it.
fn describe_source(source: &SourceInfo) -> String {
    match &source.id {
        SourceId::WholeSystem => source.name.clone(),
        SourceId::App { root_pid } => format!(
            "{} ({}, process {root_pid})",
            source.name, source.executable
        ),
        SourceId::Input { endpoint_id } => format!("{} (input device {endpoint_id})", source.name),
    }
}

/// The UI's ID of a listed source.
fn source_key(source: &SourceInfo) -> String {
    match &source.id {
        SourceId::WholeSystem => SYSTEM_KEY.to_owned(),
        SourceId::App { root_pid } => app_key(*root_pid, &source.executable),
        SourceId::Input { endpoint_id } => input_key(endpoint_id),
    }
}

/// The UI's ID of an app source. Executable names can't hold a colon.
fn app_key(root_pid: u32, executable: &str) -> String {
    format!("app:{root_pid}:{executable}")
}

/// The UI's ID of an input device source.
fn input_key(endpoint_id: &str) -> String {
    format!("input:{endpoint_id}")
}

#[cfg(test)]
mod device_volume_tests;

#[cfg(test)]
mod tests {
    use audiosync_core::volume::{DEFAULT_LOCAL_POSITION, output_gain, zoned_output_gain};

    use audiosync_airplay::discovery::Device;
    use audiosync_airplay::error::{ErrorKind, SessionError};
    use audiosync_airplay::pairing::PairingEvent;
    use audiosync_airplay::sender::{SenderEvent, SpeakerState as AirPlayState};
    use audiosync_cast::discovery::DEVICE_PORT;
    use audiosync_cast::error::ConnectError;
    use audiosync_cast::group::GroupMember;
    use audiosync_cast::host::{AttemptId, DeviceMemory, HostEvent};
    use audiosync_cast::session::{SessionEnd, SessionError as CastSessionError, SessionEvent};

    use super::*;
    use crate::audio::airplay::StartedSender;
    use crate::audio::airplay::tests::{
        APPLE_TV_TXT, Call, FakeAirPlay, OPEN_TXT, airplay2_tv, device, id_of,
    };
    use crate::audio::capture::{CaptureTarget, LoopbackMode};
    use crate::audio::cast::tests::{
        ATTIC as CAST_ATTIC, FakeCast, KITCHEN as CAST_KITCHEN, LIVING as CAST_LIVING,
        id as cast_device,
    };
    use crate::audio::mock::{MOCK_OWN_PID, MockBackend};
    use crate::audio::pairings::PairingStore;
    use crate::audio::pairings::tests::credentials;
    use crate::audio::routing::HOLD_CHECKS;
    use crate::debug_log::{FILE_NAME, TEST_FLUSH, assert_in_order, read_messages, test_dir};
    use crate::ui_model::{
        CastMembers, CastProblem as UiCastProblem, MAX_DELAY_MS, MIN_DELAY_MS, SpeakerState,
    };

    const SPEAKERS: &str = "speakers";
    const HEADPHONES: &str = "headphones";
    const PLAYER_PID: u32 = 2000;

    /// Two outputs, both off, and one app, as at startup.
    fn adapter() -> AudioAdapter<MockBackend> {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        audio.add_app(PLAYER_PID, "player.exe");
        AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default())
    }

    /// The UI's ID of output `endpoint_id`.
    fn key(endpoint_id: &str) -> String {
        format!("lc:{endpoint_id}")
    }

    fn player() -> String {
        format!("app:{PLAYER_PID}:player.exe")
    }

    fn turn_on(adapter: &mut AudioAdapter<MockBackend>, endpoint_id: &str) {
        adapter.set_speaker_enabled(&key(endpoint_id), true);
    }

    fn messages(adapter: &AudioAdapter<MockBackend>) -> Vec<NoticeMessage> {
        adapter
            .snapshot()
            .notices
            .into_iter()
            .map(|notice| notice.message)
            .collect()
    }

    fn enabled(adapter: &AudioAdapter<MockBackend>, endpoint_id: &str) -> bool {
        adapter
            .snapshot()
            .speaker(&key(endpoint_id))
            .is_some_and(|speaker| speaker.enabled)
    }

    fn playing(adapter: &AudioAdapter<MockBackend>) -> Vec<String> {
        adapter.audio.playing_ids()
    }

    fn captures(adapter: &AudioAdapter<MockBackend>) -> Vec<(LoopbackMode, u32)> {
        adapter
            .audio
            .driver()
            .captures_started()
            .iter()
            .filter_map(|target| match target {
                CaptureTarget::Loopback(activation) => {
                    Some((activation.mode, activation.target_pid))
                }
                CaptureTarget::Input { .. } => None,
            })
            .collect()
    }

    /// The input devices captured, by endpoint ID, in order.
    fn input_captures(adapter: &AudioAdapter<MockBackend>) -> Vec<String> {
        adapter
            .audio
            .driver()
            .captures_started()
            .iter()
            .filter_map(|target| match target {
                CaptureTarget::Input { endpoint_id } => Some(endpoint_id.clone()),
                CaptureTarget::Loopback(_) => None,
            })
            .collect()
    }

    const MIC: &str = "{0.0.1.00000000}.{mic}";

    fn mic() -> String {
        format!("input:{MIC}")
    }

    #[test]
    fn input_devices_are_listed_after_the_apps_and_can_be_streamed() {
        let mut adapter = adapter();
        adapter
            .audio
            .connect_input(MIC, "Microphone (USB Audio)", "USB");
        adapter
            .audio
            .connect_input("hf", "Headset (Hands-Free)", "BTHHFENUM");
        adapter.refresh_sources();
        let state = adapter.snapshot();
        let sources: Vec<(&str, &str, SourceKind)> = state
            .sources
            .iter()
            .map(|s| (s.id.as_str(), s.name.as_str(), s.kind))
            .collect();
        assert_eq!(
            sources,
            [
                ("system", "All System Audio", SourceKind::System),
                ("app:2000:player.exe", "Player", SourceKind::App),
                (mic().as_str(), "Microphone (USB Audio)", SourceKind::Input),
            ]
        );
        turn_on(&mut adapter, SPEAKERS);
        adapter.select_source(&mic());
        adapter.set_running(true);
        assert!(adapter.snapshot().running);
        assert_eq!(input_captures(&adapter), [MIC]);
        assert_eq!(playing(&adapter), [SPEAKERS]);
    }

    #[test]
    fn an_input_device_choice_is_saved_and_restored_while_it_is_unplugged() {
        let mut adapter = adapter();
        adapter
            .audio
            .connect_input(MIC, "Microphone (USB Audio)", "USB");
        adapter.refresh_sources();
        adapter.select_source(&mic());
        let saved = adapter.current_settings();
        assert_eq!(saved.source, SavedSource::System);
        assert_eq!(
            saved.input_device,
            Some(SavedInput {
                id: MIC.to_owned(),
                name: "Microphone (USB Audio)".to_owned(),
            })
        );

        // At the next launch the device is unplugged: the choice stays, and
        // shows under its saved name, after the apps.
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.add_app(PLAYER_PID, "player.exe");
        let mut restored = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        restored.restore(saved);
        restored.refresh_sources();
        let state = restored.snapshot();
        assert_eq!(state.source_id, mic());
        let last = state.sources.last().expect("listed");
        assert_eq!(
            (last.id.as_str(), last.name.as_str(), last.kind),
            (mic().as_str(), "Microphone (USB Audio)", SourceKind::Input)
        );

        // Streaming doesn't start without it, and says so.
        turn_on(&mut restored, SPEAKERS);
        restored.set_running(true);
        assert!(!restored.snapshot().running);
        assert_eq!(
            messages(&restored),
            [NoticeMessage::InputNotConnected {
                source: "Microphone (USB Audio)".to_owned()
            }]
        );
        assert!(input_captures(&restored).is_empty());

        // Plugged in again, it streams.
        restored
            .audio
            .connect_input(MIC, "Microphone (USB Audio)", "USB");
        restored.set_running(true);
        assert!(restored.snapshot().running);
        assert_eq!(input_captures(&restored), [MIC]);
    }

    #[test]
    fn a_microphone_blocked_by_windows_privacy_settings_gets_its_own_notice() {
        let mut adapter = adapter();
        adapter
            .audio
            .connect_input(MIC, "Microphone (USB Audio)", "USB");
        adapter.refresh_sources();
        adapter.select_source(&mic());
        adapter
            .audio
            .refuse_captures(Some(BackendError::MicrophoneBlocked));
        // Switching a speaker on starts streaming.
        turn_on(&mut adapter, SPEAKERS);
        assert!(!adapter.snapshot().running);
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::MicrophoneBlocked {
                source: "Microphone (USB Audio)".to_owned()
            }]
        );
    }

    #[test]
    fn an_input_device_unplugged_while_streaming_stops_streaming_with_a_notice() {
        let mut adapter = adapter();
        adapter
            .audio
            .connect_input(MIC, "Microphone (USB Audio)", "USB");
        adapter.refresh_sources();
        turn_on(&mut adapter, SPEAKERS);
        adapter.select_source(&mic());
        adapter.set_running(true);
        assert!(adapter.snapshot().running);
        adapter.audio.disconnect_input(MIC);
        for _ in 0..20 {
            adapter.poll();
        }
        let state = adapter.snapshot();
        assert!(!state.running);
        assert_eq!(state.source_id, mic(), "the choice stays");
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::InputDisconnected {
                source: "Microphone (USB Audio)".to_owned()
            }]
        );
    }

    #[test]
    fn the_snapshot_shows_the_sources_and_outputs_listed() {
        let state = adapter().snapshot();
        assert!(!state.running);
        let sources: Vec<(&str, &str, SourceKind)> = state
            .sources
            .iter()
            .map(|s| (s.id.as_str(), s.name.as_str(), s.kind))
            .collect();
        assert_eq!(
            sources,
            [
                ("system", "All System Audio", SourceKind::System),
                ("app:2000:player.exe", "Player", SourceKind::App),
            ]
        );
        assert_eq!(state.source_id, "system");
        let ids: Vec<&str> = state.speakers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["lc:headphones", "lc:speakers"]);
        let speakers = state.speaker("lc:speakers").unwrap();
        assert_eq!(speakers.name, "Speakers");
        assert!(!speakers.enabled && !speakers.auto_connect);
        assert_eq!(speakers.delay_ms, Some(0.0), "local outputs have a delay");
        assert_eq!(speakers.volume, DEFAULT_LOCAL_POSITION);
        assert_eq!(speakers.availability, Availability::Open);
        assert!(state.notices.is_empty() && state.zones.is_empty());
    }

    #[test]
    fn built_in_outputs_are_listed_under_this_pc() {
        let mut audio = MockBackend::empty();
        audio.connect_output_as("tv", "TV", OutputKind::Hdmi);
        audio.connect_output_as("jack", "Headphones", OutputKind::Headphones);
        audio.connect_output_as("internal", "Speakers", OutputKind::BuiltIn);
        audio.connect_output_as("bt", "Buds", OutputKind::Bluetooth);
        let state = AudioAdapter::new(audio, String::new, DebugLog::default()).snapshot();
        let (this_pc, connected) = state.layout();
        let rows = |list: &[&Speaker]| -> Vec<(String, SpeakerKind)> {
            list.iter().map(|s| (s.name.clone(), s.kind)).collect()
        };
        assert_eq!(
            rows(&this_pc),
            [
                ("Headphones".to_owned(), SpeakerKind::Headphones),
                ("Speakers".to_owned(), SpeakerKind::BuiltIn)
            ]
        );
        assert_eq!(
            rows(&connected),
            [
                ("Buds".to_owned(), SpeakerKind::Bluetooth),
                ("TV".to_owned(), SpeakerKind::Hdmi)
            ]
        );
    }

    #[test]
    fn a_usb_device_keeps_its_form_and_stays_under_connected_speakers() {
        let mut audio = MockBackend::empty();
        audio.connect_output_as("headset", "Headset", OutputKind::UsbHeadphones);
        audio.connect_output_as("desk", "Desk Speakers", OutputKind::UsbSpeakers);
        audio.connect_output_as("dac", "DAC", OutputKind::Usb);
        let state = AudioAdapter::new(audio, String::new, DebugLog::default()).snapshot();
        let (this_pc, connected) = state.layout();
        assert!(this_pc.is_empty(), "{this_pc:?}");
        let rows: Vec<(String, SpeakerKind)> =
            connected.iter().map(|s| (s.name.clone(), s.kind)).collect();
        assert_eq!(
            rows,
            [
                ("DAC".to_owned(), SpeakerKind::Usb),
                ("Desk Speakers".to_owned(), SpeakerKind::UsbSpeakers),
                ("Headset".to_owned(), SpeakerKind::UsbHeadphones)
            ]
        );
    }

    #[test]
    fn turning_a_speaker_on_starts_streaming_to_it() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        let state = adapter.snapshot();
        assert!(state.running);
        let speaker = state.speaker(&key(SPEAKERS)).unwrap();
        assert_eq!(state.speaker_state(speaker), SpeakerState::Playing);
        assert_eq!(playing(&adapter), [SPEAKERS]);
        assert_eq!(
            captures(&adapter),
            [(LoopbackMode::ExcludeTree, MOCK_OWN_PID)],
            "the whole system, without AudioSync"
        );
        turn_on(&mut adapter, HEADPHONES);
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
        assert_eq!(captures(&adapter).len(), 1, "one session");
    }

    #[test]
    fn turning_the_last_speaker_off_stops_streaming() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_enabled(&key(SPEAKERS), false);
        assert!(adapter.snapshot().running);
        assert_eq!(playing(&adapter), [HEADPHONES]);
        adapter.set_speaker_enabled(&key(HEADPHONES), false);
        assert!(!adapter.snapshot().running);
        assert!(playing(&adapter).is_empty());
    }

    #[test]
    fn streaming_needs_a_speaker_that_is_on() {
        let mut adapter = adapter();
        adapter.set_running(true);
        assert!(!adapter.snapshot().running);
        assert_eq!(messages(&adapter), [NoticeMessage::NoSpeaker]);
        assert_eq!(adapter.snapshot().notices[0].time, "09.41");
        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        assert!(messages(&adapter).is_empty(), "the start clears it");
    }

    #[test]
    fn switching_streaming_off_keeps_the_speakers_on() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_running(false);
        let state = adapter.snapshot();
        assert!(!state.running);
        let speaker = state.speaker(&key(SPEAKERS)).unwrap();
        assert_eq!(state.speaker_state(speaker), SpeakerState::WillPlay);
        adapter.set_running(true);
        assert_eq!(playing(&adapter), [SPEAKERS]);
    }

    #[test]
    fn volumes_delays_and_the_master_reach_the_audio_backend() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_volume(&key(SPEAKERS), 30.0);
        adapter.set_speaker_delay(&key(SPEAKERS), 120.4);
        adapter.set_master_volume(50.0);
        let state = adapter.snapshot();
        let speaker = state.speaker(&key(SPEAKERS)).unwrap();
        assert_eq!(speaker.volume, 30.0);
        assert_eq!(speaker.delay_ms, Some(120.0), "whole milliseconds");
        assert_eq!(state.master_volume, 50.0);
        let params = adapter.audio.playing_params(SPEAKERS).unwrap();
        assert_eq!(
            params.delay_frames, 5_760,
            "120 ms after the headphones at 0, at 48 kHz"
        );
        assert_eq!(params.gain, output_gain(30.0, 50.0) as f32);

        adapter.set_speaker_enabled(&key(HEADPHONES), false);
        adapter.set_speaker_delay(&key(HEADPHONES), 99_999.0);
        let state = adapter.snapshot();
        assert_eq!(
            state.speaker(&key(HEADPHONES)).unwrap().delay_ms,
            Some(MAX_DELAY_MS),
            "a delay set while off is kept, within the limit"
        );
        adapter.set_speaker_delay(&key(HEADPHONES), -99_999.0);
        let state = adapter.snapshot();
        assert_eq!(
            state.speaker(&key(HEADPHONES)).unwrap().delay_ms,
            Some(MIN_DELAY_MS),
            "and down to −2000 ms for a speaker that sounds late"
        );
    }

    #[test]
    fn a_zone_switches_its_present_members_together() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.new_zone(" Desk ").unwrap();
        let zone = adapter.snapshot().zones[0].clone();
        assert_eq!(zone.name, "Desk");
        let mut members = zone.members.clone();
        members.sort();
        assert_eq!(members, [key(HEADPHONES), key(SPEAKERS)]);

        adapter.toggle_zone(&zone.id);
        let state = adapter.snapshot();
        assert!(!state.running, "its speakers were the last ones on");
        assert_eq!(state.zone_state(&zone), SpeakerState::Off);

        adapter.toggle_zone(&zone.id);
        let state = adapter.snapshot();
        assert!(state.running);
        assert_eq!(state.zone_state(&zone), SpeakerState::Playing);
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);

        // An absent member is left alone.
        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        adapter.toggle_zone(&zone.id);
        assert!(!enabled(&adapter, SPEAKERS));
        assert_eq!(adapter.audio.enabled_outputs(), [HEADPHONES]);
        assert!(adapter.snapshot().running, "the absent member is still on");
    }

    #[test]
    fn a_new_zone_needs_a_speaker_that_is_on_and_names_can_change() {
        let mut adapter = adapter();
        assert_eq!(adapter.new_zone("Office"), Err(ZoneError::NoActiveSpeakers));
        turn_on(&mut adapter, HEADPHONES);
        adapter.new_zone("Office").unwrap();
        adapter.new_zone("Den").unwrap();
        let zones = adapter.snapshot().zones;
        let ids: Vec<&str> = zones.iter().map(|z| z.id.as_str()).collect();
        assert_eq!(ids, ["zone:1", "zone:2"]);
        assert!(zones[0].members.is_empty(), "the headphones moved to Den");
        assert_eq!(zones[1].members, [key(HEADPHONES)]);
        adapter.rename_zone("zone:1", "  ");
        adapter.rename_zone("zone:2", " Study ");
        let names: Vec<String> = adapter
            .snapshot()
            .zones
            .into_iter()
            .map(|z| z.name)
            .collect();
        assert_eq!(names, ["Office", "Study"], "a blank name keeps the old one");
        adapter.delete_zone("zone:1");
        assert_eq!(adapter.snapshot().zones.len(), 1);
    }

    #[test]
    fn a_speaker_moves_between_zones_from_its_menu() {
        let mut adapter = adapter();
        turn_on(&mut adapter, HEADPHONES);
        adapter.new_zone("Desk").unwrap();
        adapter.new_zone_with(&key(SPEAKERS), "  ");
        let zones = adapter.snapshot().zones;
        assert_eq!(zones[1].name, "Zone 2", "a blank name is numbered");
        assert_eq!(zones[1].members, [key(SPEAKERS)]);
        adapter.move_to_zone(&key(SPEAKERS), "zone:1");
        let zones = adapter.snapshot().zones;
        assert_eq!(zones[0].members, [key(HEADPHONES), key(SPEAKERS)]);
        assert!(zones[1].members.is_empty());
        adapter.remove_from_zone(&key(HEADPHONES));
        adapter.set_zone_volume("zone:1", 140.0);
        let zone = adapter.snapshot().zones[0].clone();
        assert_eq!(zone.members, [key(SPEAKERS)]);
        assert_eq!(zone.volume, 100.0, "kept within 0 to 100");
        assert_eq!(adapter.current_settings().speaker_zones[0].volume, 100.0);
    }

    #[test]
    fn an_app_that_quits_stops_streaming_and_stays_chosen() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        turn_on(&mut adapter, SPEAKERS);
        assert_eq!(
            captures(&adapter),
            [(LoopbackMode::IncludeTree, PLAYER_PID)]
        );

        adapter.audio_mut().quit_app(PLAYER_PID);
        assert!(adapter.poll());
        let state = adapter.snapshot();
        assert!(!state.running);
        let quit = NoticeMessage::SourceQuit {
            source: "Player".into(),
        };
        assert_eq!(messages(&adapter), [quit]);
        assert_eq!(state.source_id, player(), "the choice stays");
        assert!(
            state
                .sources
                .iter()
                .any(|s| s.id == player() && s.name == "Player"),
            "and shows: {:?}",
            state.sources
        );
        assert!(enabled(&adapter, SPEAKERS), "the speakers stay on");

        adapter.set_running(true);
        assert!(!adapter.snapshot().running);
        let not_running = NoticeMessage::SourceNotRunning {
            source: "Player".into(),
        };
        assert_eq!(messages(&adapter), [not_running]);
        assert_eq!(captures(&adapter).len(), 1, "nothing started");
    }

    #[test]
    fn an_app_that_quits_gives_way_to_the_whole_system_when_the_setting_says_so() {
        let mut adapter = adapter();
        adapter.set_when_source_quits(WhenSourceQuits::SwitchToSystem);
        adapter.select_source(&player());
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);

        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.poll();
        adapter.poll();
        let state = adapter.snapshot();
        assert!(state.running, "streaming goes on");
        assert_eq!(
            captures(&adapter),
            [
                (LoopbackMode::IncludeTree, PLAYER_PID),
                (LoopbackMode::ExcludeTree, MOCK_OWN_PID),
            ]
        );
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
        let switched = NoticeMessage::SourceSwitched {
            source: "Player".into(),
        };
        assert_eq!(messages(&adapter), [switched]);
        assert_eq!(state.source_id, "system");
        let saved = adapter.current_settings();
        assert_eq!(saved.source, SavedSource::System, "the switch is kept");
        assert_eq!(saved.when_source_quits, "system");

        // The app starting again changes nothing (S227 c).
        adapter.audio_mut().add_app(2100, "player.exe");
        adapter.poll();
        assert_eq!(adapter.snapshot().source_id, "system");
        assert_eq!(captures(&adapter).len(), 2);
    }

    #[test]
    fn the_setting_for_a_quitting_app_is_saved_and_restored() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.snapshot().when_source_quits,
            WhenSourceQuits::Stop,
            "stop by default"
        );
        adapter.set_when_source_quits(WhenSourceQuits::SwitchToSystem);
        let saved = adapter.current_settings();
        let mut restored = AudioAdapter::new(
            MockBackend::empty(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        restored.restore(saved);
        assert_eq!(
            restored.snapshot().when_source_quits,
            WhenSourceQuits::SwitchToSystem
        );
    }

    #[test]
    fn an_app_that_started_again_is_found_by_its_executable() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.audio_mut().add_app(2100, "player.exe");
        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        assert_eq!(captures(&adapter), [(LoopbackMode::IncludeTree, 2100)]);
        assert_eq!(adapter.snapshot().source_id, "app:2100:player.exe");
    }

    #[test]
    fn another_app_with_the_process_id_of_one_that_quit_isnt_captured() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.audio_mut().add_app(PLAYER_PID, "other.exe");
        turn_on(&mut adapter, SPEAKERS);
        assert!(!adapter.snapshot().running);
        assert!(captures(&adapter).is_empty());
        let not_running = NoticeMessage::SourceNotRunning {
            source: "Player".into(),
        };
        assert_eq!(messages(&adapter), [not_running]);
        let state = adapter.snapshot();
        let ids: Vec<&str> = state.sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["system", "app:2000:other.exe", "app:2000:player.exe"]);
        assert_eq!(state.source_id, player());
    }

    #[test]
    fn an_app_without_an_audio_session_is_still_captured() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        // Paused, it closed its stream, so the source list leaves it out.
        adapter.audio_mut().close_audio_session(PLAYER_PID);
        adapter.refresh_devices();
        assert_eq!(adapter.snapshot().source_id, player(), "the choice stays");

        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        assert_eq!(
            captures(&adapter),
            [(LoopbackMode::IncludeTree, PLAYER_PID)]
        );
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
    }

    #[test]
    fn an_app_opened_again_is_captured_before_it_plays() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.audio_mut().add_silent_app(2100, "player.exe");
        turn_on(&mut adapter, SPEAKERS);
        let state = adapter.snapshot();
        assert!(state.running);
        assert_eq!(captures(&adapter), [(LoopbackMode::IncludeTree, 2100)]);
        assert_eq!(state.source_id, "app:2100:player.exe");
        assert!(
            state
                .sources
                .iter()
                .any(|s| s.id == state.source_id && s.name == "Player"),
            "it shows: {:?}",
            state.sources
        );
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
    }

    #[test]
    fn a_new_source_while_streaming_starts_capture_again() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.select_source(&player());
        assert!(adapter.snapshot().running);
        assert_eq!(
            captures(&adapter),
            [
                (LoopbackMode::ExcludeTree, MOCK_OWN_PID),
                (LoopbackMode::IncludeTree, PLAYER_PID)
            ]
        );
        adapter.select_source(&player());
        assert_eq!(
            captures(&adapter).len(),
            2,
            "the same source changes nothing"
        );
    }

    #[test]
    fn capture_that_fails_while_streaming_stops_it_with_a_notice() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.audio_mut().fail_capture(BackendError::System {
            call: "IAudioCaptureClient::GetBuffer",
            code: 1,
        });
        assert!(adapter.poll());
        assert!(!adapter.snapshot().running);
        assert_eq!(messages(&adapter), [NoticeMessage::CaptureLost]);
    }

    #[test]
    fn capture_lost_to_a_windows_audio_restart_comes_back_without_a_notice() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.audio_mut().fail_capture(BackendError::DeviceGone);
        for _ in 0..3 {
            adapter.poll();
            assert!(adapter.snapshot().running, "streaming stays on");
        }
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        assert_eq!(adapter.audio.driver().captures_started().len(), 2);
        assert_eq!(playing(&adapter), [SPEAKERS]);
    }

    #[test]
    fn capture_that_cant_start_leaves_streaming_off_with_a_notice() {
        let mut adapter = adapter();
        adapter
            .audio_mut()
            .refuse_captures(Some(BackendError::DeviceGone));
        turn_on(&mut adapter, SPEAKERS);
        assert!(!adapter.snapshot().running);
        assert!(enabled(&adapter, SPEAKERS), "the speaker stays on");
        assert_eq!(messages(&adapter), [NoticeMessage::CaptureFailed]);
    }

    #[test]
    fn start_problems_name_the_chosen_app() {
        let mut adapter = adapter();
        adapter.select_source(&player());
        let player = || "Player".to_owned();
        assert_eq!(
            adapter.start_problem(&BackendError::ContainsAudioSync),
            NoticeMessage::SourceRefused { source: player() }
        );
        assert_eq!(
            adapter.start_problem(&BackendError::OwnProcess),
            NoticeMessage::SourceRefused { source: player() }
        );
        assert_eq!(
            adapter.start_problem(&BackendError::SourceGone),
            NoticeMessage::SourceNotRunning { source: player() }
        );
        assert_eq!(
            adapter.start_problem(&BackendError::UnsupportedFormat),
            NoticeMessage::CaptureFailed
        );
    }

    #[test]
    fn an_output_that_cant_start_is_turned_off_with_a_notice() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter
            .audio_mut()
            .fail_output(HEADPHONES, Some(BackendError::UnsupportedFormat));
        turn_on(&mut adapter, HEADPHONES);
        assert!(!enabled(&adapter, HEADPHONES));
        assert!(adapter.snapshot().running, "the speakers play on");
        let failed = NoticeMessage::SpeakerFailed {
            speaker: "Headphones".into(),
        };
        assert_eq!(messages(&adapter), [failed]);

        // Turning it on again is a new attempt.
        adapter.audio_mut().fail_output(HEADPHONES, None);
        turn_on(&mut adapter, HEADPHONES);
        assert!(messages(&adapter).is_empty());
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn streaming_stops_when_the_only_output_cant_start() {
        let mut adapter = adapter();
        adapter
            .audio_mut()
            .fail_output(SPEAKERS, Some(BackendError::UnsupportedFormat));
        turn_on(&mut adapter, SPEAKERS);
        assert!(!adapter.snapshot().running);
        assert!(!enabled(&adapter, SPEAKERS));
        let failed = NoticeMessage::SpeakerFailed {
            speaker: "Speakers".into(),
        };
        assert_eq!(messages(&adapter), [failed]);
    }

    #[test]
    fn an_unplugged_output_is_reported_until_it_is_back() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.audio_mut().disconnect_output(HEADPHONES);
        assert!(adapter.poll());
        let state = adapter.snapshot();
        assert!(state.running, "the speakers play on");
        assert!(state.speaker(&key(HEADPHONES)).is_none(), "not listed");
        let lost = NoticeMessage::SpeakerLost {
            speaker: "Headphones".into(),
        };
        assert_eq!(messages(&adapter), [lost]);

        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        assert!(adapter.poll());
        assert!(messages(&adapter).is_empty(), "its cause is gone");
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn an_output_a_refresh_no_longer_lists_is_reported() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        // Its stream doesn't notice; the listing that the refresh asks for
        // does.
        adapter.audio_mut().disconnect_output_unnoticed(HEADPHONES);
        adapter.refresh_devices();
        let lost = NoticeMessage::SpeakerLost {
            speaker: "Headphones".into(),
        };
        assert_eq!(messages(&adapter), [lost]);
        assert_eq!(playing(&adapter), [SPEAKERS]);
    }

    #[test]
    fn a_stream_that_resets_while_its_device_stays_gets_no_notice() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        // A format change: the stream ends, and opening it again fails for a
        // moment as if the device were gone.
        adapter.audio_mut().end_renderer(SPEAKERS);
        adapter
            .audio_mut()
            .fail_output(SPEAKERS, Some(BackendError::DeviceGone));
        adapter.poll();
        assert!(playing(&adapter).is_empty());
        assert!(enabled(&adapter, SPEAKERS), "it stays on");
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));

        adapter.audio_mut().fail_output(SPEAKERS, None);
        adapter.poll();
        adapter.poll();
        assert_eq!(playing(&adapter), [SPEAKERS]);
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
    }

    #[test]
    fn a_speaker_whose_stream_dies_before_its_device_leaves_the_list_is_reported() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        // The stream dies and can't open again while Windows still lists the
        // device: no notice yet, as for a format change.
        adapter.audio_mut().end_renderer(HEADPHONES);
        adapter
            .audio_mut()
            .fail_output(HEADPHONES, Some(BackendError::DeviceGone));
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));

        // Then Windows drops it from the list.
        adapter.audio_mut().disconnect_output_unnoticed(HEADPHONES);
        assert!(adapter.poll());
        assert!(adapter.snapshot().speaker(&key(HEADPHONES)).is_none());
        let lost = NoticeMessage::SpeakerLost {
            speaker: "Headphones".into(),
        };
        assert_eq!(messages(&adapter), [lost]);
    }

    /// The endpoint IDs Windows gives one USB DAC in three USB ports.
    const DAC_PORT_1: &str = "usb-dac-port-1";
    const DAC_PORT_2: &str = "usb-dac-port-2";
    const DAC_PORT_3: &str = "usb-dac-port-3";

    /// Plugs the USB DAC in under `endpoint_id`, and polls.
    fn plug_dac(adapter: &mut AudioAdapter<MockBackend>, endpoint_id: &str) {
        adapter
            .audio_mut()
            .connect_output_as(endpoint_id, "USB DAC", OutputKind::Usb);
        adapter.poll();
    }

    fn unplug(adapter: &mut AudioAdapter<MockBackend>, endpoint_id: &str) {
        adapter.audio_mut().disconnect_output(endpoint_id);
        adapter.poll();
    }

    #[test]
    fn a_speaker_on_that_wasnt_listed_since_launch_doesnt_count_as_on() {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.add_app(PLAYER_PID, "player.exe");
        // Saved as on under an endpoint ID Windows no longer lists: the DAC
        // went to another USB port while AudioSync wasn't running.
        audio.set_output_enabled(DAC_PORT_1, true).unwrap();
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        adapter.set_running(true);
        assert!(!adapter.snapshot().running);
        assert_eq!(messages(&adapter), [NoticeMessage::NoSpeaker]);

        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        // Every speaker the user can see is off: streaming stops.
        adapter.set_speaker_enabled(&key(SPEAKERS), false);
        assert!(!adapter.snapshot().running);

        // It kept its switch: listed, it counts and plays.
        plug_dac(&mut adapter, DAC_PORT_1);
        assert!(enabled(&adapter, DAC_PORT_1));
        adapter.set_running(true);
        assert_eq!(playing(&adapter), [DAC_PORT_1]);
    }

    #[test]
    fn a_usb_speaker_plugged_into_another_port_keeps_its_settings_under_its_new_id() {
        let mut adapter = adapter();
        plug_dac(&mut adapter, DAC_PORT_1);
        turn_on(&mut adapter, DAC_PORT_1);
        adapter.set_speaker_volume(&key(DAC_PORT_1), 35.0);
        adapter.set_speaker_delay(&key(DAC_PORT_1), 120.0);
        adapter.set_auto_connect(&key(DAC_PORT_1), true);
        adapter.new_zone("Desk").unwrap();
        adapter.poll();

        // Into another port: Windows lists it under a new endpoint ID.
        unplug(&mut adapter, DAC_PORT_1);
        let lost = NoticeMessage::SpeakerLost {
            speaker: "USB DAC".into(),
        };
        assert_eq!(messages(&adapter), [lost]);
        plug_dac(&mut adapter, DAC_PORT_2);
        let dac = speaker_of(&adapter, &key(DAC_PORT_2));
        assert!(dac.enabled);
        assert_eq!(dac.volume, 35.0);
        assert_eq!(dac.delay_ms, Some(120.0));
        assert!(dac.auto_connect);
        assert_eq!(adapter.snapshot().zones[0].members, [key(DAC_PORT_2)]);
        assert_eq!(playing(&adapter), [DAC_PORT_2]);
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        assert_eq!(
            adapter.audio.enabled_outputs(),
            [DAC_PORT_2],
            "the old ID is forgotten"
        );
        let saved = adapter.current_settings();
        assert!(!saved.outputs.contains_key(DAC_PORT_1));
        assert!(!saved.auto_connect.contains(&key(DAC_PORT_1)));

        // Back in the first port, the settings follow it again.
        unplug(&mut adapter, DAC_PORT_2);
        plug_dac(&mut adapter, DAC_PORT_1);
        let dac = speaker_of(&adapter, &key(DAC_PORT_1));
        assert!(dac.enabled);
        assert_eq!(dac.volume, 35.0);
        assert_eq!(playing(&adapter), [DAC_PORT_1]);
        // And into a third: the IDs it left behind hold nothing anymore.
        unplug(&mut adapter, DAC_PORT_1);
        plug_dac(&mut adapter, DAC_PORT_3);
        assert!(enabled(&adapter, DAC_PORT_3));
        assert_eq!(playing(&adapter), [DAC_PORT_3]);
    }

    #[test]
    fn settings_follow_a_new_id_only_from_the_one_absent_speaker_of_its_name() {
        let mut adapter = adapter();
        // Two DACs of the same model, both on: one plugged in under a new ID
        // while the other is still listed stays a new speaker.
        plug_dac(&mut adapter, DAC_PORT_1);
        turn_on(&mut adapter, DAC_PORT_1);
        plug_dac(&mut adapter, DAC_PORT_2);
        assert!(!enabled(&adapter, DAC_PORT_2));
        turn_on(&mut adapter, DAC_PORT_2);

        // Both unplugged, a third port could be either: a new speaker.
        unplug(&mut adapter, DAC_PORT_1);
        unplug(&mut adapter, DAC_PORT_2);
        plug_dac(&mut adapter, DAC_PORT_3);
        assert!(!enabled(&adapter, DAC_PORT_3));
        assert_eq!(adapter.audio.enabled_outputs(), [DAC_PORT_1, DAC_PORT_2]);
    }

    #[test]
    fn a_new_id_doesnt_take_settings_when_an_untouched_twin_left_too() {
        let mut adapter = adapter();
        // Two DACs of the same model: only the first was ever set.
        plug_dac(&mut adapter, DAC_PORT_1);
        turn_on(&mut adapter, DAC_PORT_1);
        adapter.set_speaker_volume(&key(DAC_PORT_1), 35.0);
        plug_dac(&mut adapter, DAC_PORT_2);

        // Both unplugged, the untouched one into a third port: it could be
        // either of them, so it is a new speaker.
        unplug(&mut adapter, DAC_PORT_1);
        unplug(&mut adapter, DAC_PORT_2);
        plug_dac(&mut adapter, DAC_PORT_3);
        assert!(!enabled(&adapter, DAC_PORT_3));
        assert_eq!(adapter.audio.enabled_outputs(), [DAC_PORT_1]);

        // The first one, back in its port, still has its settings.
        plug_dac(&mut adapter, DAC_PORT_1);
        let dac = speaker_of(&adapter, &key(DAC_PORT_1));
        assert!(dac.enabled);
        assert_eq!(dac.volume, 35.0);
    }

    /// Plugs a USB device in under `endpoint_id`, listed as `name`, and
    /// polls.
    fn plug_named(adapter: &mut AudioAdapter<MockBackend>, endpoint_id: &str, name: &str) {
        adapter
            .audio_mut()
            .connect_output_as(endpoint_id, name, OutputKind::Usb);
        adapter.poll();
    }

    #[test]
    fn a_usb_speaker_windows_numbers_in_another_port_keeps_its_settings() {
        let mut adapter = adapter();
        plug_named(&mut adapter, DAC_PORT_1, "Speakers (Thronmax MDrill)");
        turn_on(&mut adapter, DAC_PORT_1);
        adapter.set_speaker_volume(&key(DAC_PORT_1), 35.0);
        adapter.set_speaker_delay(&key(DAC_PORT_1), 120.0);

        // In another port, Windows numbers it: still the same device.
        unplug(&mut adapter, DAC_PORT_1);
        plug_named(&mut adapter, DAC_PORT_2, "Speakers (2- Thronmax MDrill)");
        let dac = speaker_of(&adapter, &key(DAC_PORT_2));
        assert!(dac.enabled);
        assert_eq!(dac.volume, 35.0);
        assert_eq!(dac.delay_ms, Some(120.0));
        assert_eq!(playing(&adapter), [DAC_PORT_2]);
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        assert_eq!(adapter.audio.enabled_outputs(), [DAC_PORT_2]);

        // Back in the first port without the number, they follow it again.
        unplug(&mut adapter, DAC_PORT_2);
        plug_named(&mut adapter, DAC_PORT_1, "Speakers (Thronmax MDrill)");
        assert!(enabled(&adapter, DAC_PORT_1));
        assert_eq!(speaker_of(&adapter, &key(DAC_PORT_1)).volume, 35.0);
    }

    #[test]
    fn only_the_number_windows_puts_inside_the_parentheses_is_ignored() {
        for (name, plain) in [
            (
                "Speakers (2- Thronmax MDrill)",
                "Speakers (Thronmax MDrill)",
            ),
            ("Speakers (12- USB DAC)", "Speakers (USB DAC)"),
            ("Speakers (USB DAC)", "Speakers (USB DAC)"),
            ("Speakers (Realtek(R) Audio)", "Speakers (Realtek(R) Audio)"),
            ("Speakers (2-USB DAC)", "Speakers (2-USB DAC)"),
            ("Speakers (2 USB DAC)", "Speakers (2 USB DAC)"),
            ("2- USB DAC", "2- USB DAC"),
            ("Speakers (", "Speakers ("),
            ("", ""),
        ] {
            assert_eq!(super::unnumbered(name), plain, "{name:?}");
        }
    }

    #[test]
    fn twins_windows_numbers_apart_still_count_as_twins() {
        let mut adapter = adapter();
        // Two DACs of the same model: Windows numbers the second one.
        plug_named(&mut adapter, DAC_PORT_1, "Speakers (USB DAC)");
        turn_on(&mut adapter, DAC_PORT_1);
        plug_named(&mut adapter, DAC_PORT_2, "Speakers (2- USB DAC)");
        assert!(!enabled(&adapter, DAC_PORT_2));

        // Both unplugged, one in a third port could be either: a new
        // speaker, numbered or not.
        unplug(&mut adapter, DAC_PORT_1);
        unplug(&mut adapter, DAC_PORT_2);
        plug_named(&mut adapter, DAC_PORT_3, "Speakers (USB DAC)");
        assert!(!enabled(&adapter, DAC_PORT_3));
        assert_eq!(adapter.audio.enabled_outputs(), [DAC_PORT_1]);
    }

    #[test]
    fn twins_windows_numbers_apart_that_appear_together_dont_take_settings() {
        const DAC_PORT_4: &str = "usb-dac-port-4";
        let mut adapter = adapter();
        plug_named(&mut adapter, DAC_PORT_1, "Speakers (USB DAC)");
        turn_on(&mut adapter, DAC_PORT_1);
        unplug(&mut adapter, DAC_PORT_1);
        // Two of the model appear in one listing, one numbered by Windows:
        // either could be the one that left.
        let audio = adapter.audio_mut();
        audio.connect_output_as(DAC_PORT_3, "Speakers (USB DAC)", OutputKind::Usb);
        audio.connect_output_as(DAC_PORT_4, "Speakers (2- USB DAC)", OutputKind::Usb);
        adapter.poll();
        assert!(!enabled(&adapter, DAC_PORT_3));
        assert!(!enabled(&adapter, DAC_PORT_4));
        assert_eq!(adapter.audio.enabled_outputs(), [DAC_PORT_1]);
    }

    #[test]
    fn a_port_a_moved_speaker_left_counts_as_new_only_until_it_is_listed_again() {
        let mut adapter = adapter();
        plug_dac(&mut adapter, DAC_PORT_1);
        turn_on(&mut adapter, DAC_PORT_1);
        unplug(&mut adapter, DAC_PORT_1);
        plug_dac(&mut adapter, DAC_PORT_2);
        assert!(enabled(&adapter, DAC_PORT_2), "its settings followed it");

        // A second DAC of the model, in the port the first one left, while
        // the first one is still listed: a new speaker.
        plug_dac(&mut adapter, DAC_PORT_1);
        assert!(!enabled(&adapter, DAC_PORT_1));
        // Both unplugged, the second one back in that port: listed before,
        // it is that second DAC again, not the first one.
        unplug(&mut adapter, DAC_PORT_1);
        unplug(&mut adapter, DAC_PORT_2);
        plug_dac(&mut adapter, DAC_PORT_1);
        assert!(!enabled(&adapter, DAC_PORT_1));
        assert_eq!(adapter.audio.enabled_outputs(), [DAC_PORT_2]);
    }

    /// Two displays of the same model on HDMI.
    const MONITOR_A: &str = "hdmi-monitor-a";
    const MONITOR_B: &str = "hdmi-monitor-b";

    fn connect_monitor(adapter: &mut AudioAdapter<MockBackend>, endpoint_id: &str) {
        adapter
            .audio_mut()
            .connect_output_as(endpoint_id, "LG HDR 4K", OutputKind::Hdmi);
        adapter.poll();
    }

    #[test]
    fn a_twin_listed_before_that_comes_back_first_doesnt_take_the_others_settings() {
        let mut adapter = adapter();
        connect_monitor(&mut adapter, MONITOR_A);
        connect_monitor(&mut adapter, MONITOR_B);
        turn_on(&mut adapter, MONITOR_A);
        adapter.set_auto_connect(&key(MONITOR_A), true);
        adapter.new_zone("Desk").unwrap();
        adapter.poll();

        // The displays sleep: both leave the list, and the one never set
        // wakes first, under the endpoint ID it had.
        unplug(&mut adapter, MONITOR_A);
        unplug(&mut adapter, MONITOR_B);
        connect_monitor(&mut adapter, MONITOR_B);
        let b = speaker_of(&adapter, &key(MONITOR_B));
        assert!(!b.enabled);
        assert!(!b.auto_connect);
        assert_eq!(adapter.audio.enabled_outputs(), [MONITOR_A]);
        assert_eq!(adapter.snapshot().zones[0].members, [key(MONITOR_A)]);
        let saved = adapter.current_settings();
        assert!(saved.auto_connect.contains(&key(MONITOR_A)));

        // The other one wakes too, with everything that was set for it.
        connect_monitor(&mut adapter, MONITOR_A);
        let a = speaker_of(&adapter, &key(MONITOR_A));
        assert!(a.enabled);
        assert!(a.auto_connect);
        assert!(!speaker_of(&adapter, &key(MONITOR_B)).enabled);
    }

    #[test]
    fn a_new_speaker_of_another_kind_doesnt_take_the_settings_of_one_with_its_name() {
        let mut adapter = adapter();
        plug_dac(&mut adapter, DAC_PORT_1);
        turn_on(&mut adapter, DAC_PORT_1);
        unplug(&mut adapter, DAC_PORT_1);
        adapter
            .audio_mut()
            .connect_output_as(DAC_PORT_2, "USB DAC", OutputKind::Bluetooth);
        adapter.poll();
        assert!(!enabled(&adapter, DAC_PORT_2));
        assert_eq!(adapter.audio.enabled_outputs(), [DAC_PORT_1]);
    }

    #[test]
    fn a_speaker_another_app_holds_stays_on_with_a_notice_until_it_is_free() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter
            .audio_mut()
            .fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        turn_on(&mut adapter, HEADPHONES);
        adapter.poll();
        assert!(enabled(&adapter, HEADPHONES), "it stays on");
        assert!(adapter.snapshot().running);
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::SpeakerBusy {
                speaker: "Headphones".into(),
            }]
        );
        assert_eq!(playing(&adapter), [SPEAKERS]);

        adapter.audio_mut().fail_output(HEADPHONES, None);
        poll_until_busy_retried(&mut adapter);
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
        assert!(messages(&adapter).is_empty(), "its cause is gone");
    }

    fn state_of(adapter: &AudioAdapter<MockBackend>, id: &str) -> SpeakerState {
        let state = adapter.snapshot();
        state.speaker_state(state.speaker(id).expect("listed"))
    }

    #[test]
    fn a_busy_speaker_row_waits_for_its_device_until_it_plays_or_is_turned_off() {
        let waiting = SpeakerState::Waiting(ui_model::Waiting::ForDevice);
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter
            .audio_mut()
            .fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        turn_on(&mut adapter, HEADPHONES);
        adapter.poll();
        assert_eq!(state_of(&adapter, &key(HEADPHONES)), waiting);
        assert_eq!(state_of(&adapter, &key(SPEAKERS)), SpeakerState::Playing);
        // The row keeps saying so after the notice is dismissed.
        adapter.dismiss_all_notices();
        adapter.poll();
        assert_eq!(state_of(&adapter, &key(HEADPHONES)), waiting);
        // Free again, it plays.
        adapter.audio_mut().fail_output(HEADPHONES, None);
        poll_until_busy_retried(&mut adapter);
        assert_eq!(playing(&adapter), [HEADPHONES, SPEAKERS]);
        assert_eq!(state_of(&adapter, &key(HEADPHONES)), SpeakerState::Playing);

        // Busy again after a new start, then turned off: off, and turned on
        // with its device free it doesn't wait.
        adapter
            .audio_mut()
            .fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        adapter.set_speaker_enabled(&key(HEADPHONES), false);
        turn_on(&mut adapter, HEADPHONES);
        adapter.poll();
        assert_eq!(state_of(&adapter, &key(HEADPHONES)), waiting);
        adapter.set_speaker_enabled(&key(HEADPHONES), false);
        assert_eq!(state_of(&adapter, &key(HEADPHONES)), SpeakerState::Off);
        adapter.audio_mut().fail_output(HEADPHONES, None);
        turn_on(&mut adapter, HEADPHONES);
        assert_eq!(state_of(&adapter, &key(HEADPHONES)), SpeakerState::Playing);
    }

    #[test]
    fn the_only_speaker_being_busy_keeps_streaming_on() {
        let mut adapter = adapter();
        adapter
            .audio_mut()
            .fail_output(SPEAKERS, Some(BackendError::DeviceBusy));
        turn_on(&mut adapter, SPEAKERS);
        adapter.poll();
        assert!(adapter.snapshot().running, "it plays once it's free");
        assert!(enabled(&adapter, SPEAKERS));
        adapter.audio_mut().fail_output(SPEAKERS, None);
        poll_until_busy_retried(&mut adapter);
        assert_eq!(playing(&adapter), [SPEAKERS]);
    }

    /// Polls for as long as a busy speaker waits for its next try: about
    /// three seconds, six polls.
    fn poll_until_busy_retried(adapter: &mut AudioAdapter<MockBackend>) {
        for _ in 0..6 {
            adapter.poll();
        }
    }

    #[test]
    fn an_auto_connect_speaker_that_appears_is_turned_on_and_plays() {
        let mut adapter = adapter();
        adapter.set_auto_connect(&key(HEADPHONES), true);
        let state = adapter.snapshot();
        assert!(state.speaker(&key(HEADPHONES)).unwrap().auto_connect);
        assert!(
            !enabled(&adapter, HEADPHONES),
            "marking it turns nothing on"
        );

        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        assert!(adapter.poll());
        assert!(enabled(&adapter, HEADPHONES));
        assert!(adapter.snapshot().running);
        assert_eq!(playing(&adapter), [HEADPHONES]);
    }

    #[test]
    fn auto_connect_doesnt_restart_streaming_the_user_switched_off() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_running(false);
        adapter.set_auto_connect(&key(HEADPHONES), true);
        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        adapter.poll();
        assert!(
            enabled(&adapter, HEADPHONES),
            "on, to play at the next start"
        );
        assert!(!adapter.snapshot().running);
    }

    #[test]
    fn notices_can_be_dismissed_one_by_one_or_all_at_once() {
        let mut adapter = adapter();
        turn_on(&mut adapter, HEADPHONES);
        adapter
            .audio_mut()
            .fail_output(SPEAKERS, Some(BackendError::UnsupportedFormat));
        turn_on(&mut adapter, SPEAKERS);
        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        let notices = adapter.snapshot().notices;
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].id > notices[1].id, "newest first");
        adapter.dismiss_notice(notices[1].id);
        let left: Vec<u64> = adapter.snapshot().notices.iter().map(|n| n.id).collect();
        assert_eq!(left, [notices[0].id]);
        adapter.dismiss_all_notices();
        assert!(adapter.snapshot().notices.is_empty());
    }

    #[test]
    fn the_equalizer_is_quantized_and_reaches_the_capture() {
        let mut adapter = adapter();
        adapter.set_equalizer(EqSettings {
            enabled: true,
            preamp_db: 6.2,
            ..EqSettings::default()
        });
        assert_eq!(adapter.snapshot().equalizer.preamp_db, 6.0);
        turn_on(&mut adapter, SPEAKERS);
        let raised = 0.25 * 10f32.powf(6.0 / 20.0);
        let fed = adapter.audio_mut().feed_capture_level(4, 0.25);
        assert!(fed.iter().all(|s| (s - raised).abs() < 1e-4), "{fed:?}");
    }

    #[test]
    fn a_poll_reports_a_change_only_when_something_happened() {
        let mut adapter = adapter();
        assert!(!adapter.poll());
        adapter.audio_mut().connect_output("tv", "TV");
        assert!(adapter.poll());
        assert!(adapter.snapshot().speaker("lc:tv").is_some());
        assert!(!adapter.poll());
    }

    #[test]
    fn sources_are_listed_again_every_few_polls_and_on_refresh() {
        let mut adapter = adapter();
        adapter.audio_mut().add_app(3000, "music.exe");
        let names = |adapter: &AudioAdapter<MockBackend>| -> Vec<String> {
            let sources = adapter.snapshot().sources;
            sources.into_iter().map(|s| s.name).collect()
        };
        let changed: Vec<bool> = (0..SOURCE_POLLS).map(|_| adapter.poll()).collect();
        assert_eq!(changed.iter().filter(|&&c| c).count(), 1, "{changed:?}");
        assert_eq!(names(&adapter), ["All System Audio", "Music", "Player"]);

        adapter.audio_mut().add_app(4000, "video.exe");
        adapter.refresh_devices();
        assert_eq!(
            names(&adapter),
            ["All System Audio", "Music", "Player", "Video"]
        );
    }

    #[test]
    fn unknown_ids_change_nothing() {
        let mut adapter = adapter();
        let before = adapter.snapshot();
        adapter.set_speaker_enabled("lc:nothing-here", false);
        adapter.set_speaker_enabled("ap:kitchen", true);
        adapter.set_speaker_volume("garbage", 10.0);
        adapter.select_source("app:999999");
        adapter.toggle_zone("zone:9");
        adapter.dismiss_notice(42);
        assert_eq!(adapter.snapshot(), before);
    }

    #[test]
    fn user_actions_decisions_and_notices_are_logged() {
        let dir = test_dir("adapter-actions");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        audio.add_app(PLAYER_PID, "player.exe");
        audio.set_debug_log(log.clone());
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), log.clone());
        adapter.set_running(true);
        adapter.select_source(&player());
        adapter.set_speaker_enabled(&key(SPEAKERS), true);
        adapter.set_speaker_enabled(&key(SPEAKERS), false);
        assert!(log.flush(TEST_FLUSH));
        assert_in_order(
            &read_messages(&dir.join(FILE_NAME)),
            &[
                "outputs listed: 2",
                "sources listed: All System Audio; Player (player.exe, process 2000)",
                "source chosen: All System Audio (the default)",
                "the user switched streaming on",
                "start (User) refused: no speaker is on",
                "notice about Start: NoSpeaker",
                "source chosen: Player (app:2000:player.exe)",
                "the user switched Speakers on",
                "output \"Speakers\" (speakers) switched on",
                "start (User): Player, 1 speaker(s) on",
                "starting: capturing the app of process 2000",
                "capture started: process 2000 and its children",
                "output \"Speakers\" (speakers) started:",
                "the user switched Speakers off",
                "output \"Speakers\" (speakers) switched off",
                "output \"Speakers\" (speakers) stopped: switched off",
                "streaming stops: no speaker is on anymore",
                "stopping",
                "session ended:",
            ],
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    const TV: &str = "tv";
    const CABLE: &str = "cable";

    /// The double-audio notices showing.
    fn heard_twice(adapter: &AudioAdapter<MockBackend>) -> Vec<NoticeMessage> {
        adapter
            .snapshot()
            .notices
            .into_iter()
            .filter(|notice| matches!(notice.message, NoticeMessage::HeardTwice { .. }))
            .map(|notice| notice.message)
            .collect()
    }

    /// The double-audio notice for `source` (`None` for the whole system),
    /// naming the outputs to mute and the ones AudioSync plays to as well.
    fn twice(source: Option<&str>, mute: &[&str], shared: &[&str]) -> NoticeMessage {
        let names = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect();
        NoticeMessage::HeardTwice {
            source: source.map(str::to_owned),
            mute: names(mute),
            shared: names(shared),
            default_output: None,
        }
    }

    /// The ID of the double-audio notice showing.
    fn heard_twice_id(adapter: &AudioAdapter<MockBackend>) -> Option<u64> {
        adapter
            .snapshot()
            .notices
            .into_iter()
            .find(|notice| matches!(notice.message, NoticeMessage::HeardTwice { .. }))
            .map(|notice| notice.id)
    }

    /// Runs the double-audio check now and takes in what it reports.
    fn check_routing(adapter: &mut AudioAdapter<MockBackend>) {
        adapter.audio_mut().check_routing_now();
        adapter.poll();
    }

    /// The adapter of [`adapter`] with a TV and a virtual cable connected,
    /// streaming the player to the speakers while it plays on the TV.
    fn player_on_the_tv() -> AudioAdapter<MockBackend> {
        let mut adapter = adapter();
        let audio = adapter.audio_mut();
        audio.connect_output_as(TV, "TV", OutputKind::Hdmi);
        audio.connect_output_as(CABLE, "Cable Input", OutputKind::Virtual);
        audio.play_on(PLAYER_PID, TV);
        adapter.refresh_devices();
        adapter.select_source(&player());
        turn_on(&mut adapter, SPEAKERS);
        assert!(adapter.snapshot().running);
        adapter
    }

    #[test]
    fn the_double_audio_notice_names_a_renamed_output_by_its_new_name() {
        let mut adapter = adapter();
        let audio = adapter.audio_mut();
        audio.connect_output_as(TV, "TV", OutputKind::Hdmi);
        audio.connect_output_as(CABLE, "Cable Input", OutputKind::Virtual);
        audio.play_on(PLAYER_PID, TV);
        adapter.refresh_devices();
        adapter.rename_speaker(&key(TV), "Living Room TV");
        adapter.select_source(&player());
        turn_on(&mut adapter, SPEAKERS);
        assert_eq!(
            heard_twice(&adapter),
            [twice(Some("Player"), &["Living Room TV"], &[])]
        );
    }

    #[test]
    fn hearing_the_app_twice_raises_one_notice_that_follows_the_routing() {
        let mut adapter = player_on_the_tv();
        assert_eq!(heard_twice(&adapter), [twice(Some("Player"), &["TV"], &[])]);
        assert_eq!(
            adapter.snapshot().notices[0].subject,
            "DoubleAudio",
            "one subject"
        );
        // It plays on the speakers as well, which AudioSync plays to: the
        // TV can be muted, the speakers can't.
        adapter.audio_mut().play_on(PLAYER_PID, SPEAKERS);
        check_routing(&mut adapter);
        assert_eq!(
            heard_twice(&adapter),
            [twice(Some("Player"), &["TV"], &["Speakers"])]
        );
        // Moved in the volume mixer to a virtual cable nobody listens to, it
        // is heard once again after a few checks.
        adapter.audio_mut().pause_on(PLAYER_PID, SPEAKERS);
        adapter.audio_mut().move_session(PLAYER_PID, TV, CABLE);
        check_routing(&mut adapter);
        assert_eq!(heard_twice(&adapter).len(), 1, "held a few checks");
        for _ in 0..HOLD_CHECKS {
            check_routing(&mut adapter);
        }
        assert!(heard_twice(&adapter).is_empty());
    }

    #[test]
    fn the_double_audio_notice_clears_when_the_output_is_muted() {
        let mut adapter = player_on_the_tv();
        adapter.audio_mut().mute_output(TV, true);
        check_routing(&mut adapter);
        assert!(heard_twice(&adapter).is_empty());
        adapter.audio_mut().mute_output(TV, false);
        check_routing(&mut adapter);
        assert_eq!(heard_twice(&adapter), [twice(Some("Player"), &["TV"], &[])]);
    }

    #[test]
    fn the_double_audio_notice_clears_when_streaming_stops() {
        let mut adapter = player_on_the_tv();
        adapter.set_running(false);
        adapter.poll();
        assert!(!adapter.snapshot().running);
        assert!(heard_twice(&adapter).is_empty());
    }

    #[test]
    fn the_double_audio_notice_follows_a_new_source() {
        let mut adapter = player_on_the_tv();
        adapter.audio_mut().add_app(3000, "video.exe");
        adapter.refresh_devices();
        adapter.select_source("app:3000:video.exe");
        assert!(adapter.snapshot().running);
        assert!(
            heard_twice(&adapter).is_empty(),
            "the new app plays nowhere yet"
        );
        // The whole system takes in the player on the TV too.
        adapter.select_source(SYSTEM_KEY);
        assert_eq!(heard_twice(&adapter), [twice(None, &["TV"], &[])]);
    }

    #[test]
    fn the_whole_system_notice_names_the_default_output() {
        let mut adapter = player_on_the_tv();
        adapter.audio_mut().set_default_output(SPEAKERS);
        adapter.select_source(SYSTEM_KEY);
        // The speakers count as the default output, the TV because the
        // player plays there: the guide words each its own way.
        let expected = NoticeMessage::HeardTwice {
            source: None,
            mute: vec!["TV".to_owned()],
            shared: vec!["Speakers".to_owned()],
            default_output: Some("Speakers".to_owned()),
        };
        assert_eq!(heard_twice(&adapter), [expected]);
    }

    #[test]
    fn a_dismissed_double_audio_notice_comes_back_only_when_the_routing_changes() {
        let mut adapter = player_on_the_tv();
        let id = adapter.snapshot().notices[0].id;
        adapter.dismiss_notice(id);
        check_routing(&mut adapter);
        assert!(heard_twice(&adapter).is_empty(), "nothing changed");
        adapter.audio_mut().play_on(PLAYER_PID, SPEAKERS);
        check_routing(&mut adapter);
        assert_eq!(
            heard_twice(&adapter),
            [twice(Some("Player"), &["TV"], &["Speakers"])]
        );
    }

    #[test]
    fn turning_on_an_output_the_notice_names_changes_it_in_place() {
        let mut adapter = player_on_the_tv();
        let id = heard_twice_id(&adapter);
        turn_on(&mut adapter, TV);
        check_routing(&mut adapter);
        assert_eq!(heard_twice_id(&adapter), id, "not posted again");
        assert_eq!(heard_twice(&adapter), [twice(Some("Player"), &[], &["TV"])]);
        // Dismissed, it stays away while the output goes off and on again.
        adapter.dismiss_notice(id.unwrap());
        adapter.set_speaker_enabled(&key(TV), false);
        check_routing(&mut adapter);
        turn_on(&mut adapter, TV);
        check_routing(&mut adapter);
        assert!(heard_twice(&adapter).is_empty());
    }

    #[test]
    fn a_dismissed_double_audio_notice_stays_away_until_the_app_quits() {
        const BUDS: &str = "buds";
        let mut adapter = player_on_the_tv();
        adapter.dismiss_notice(heard_twice_id(&adapter).unwrap());
        // Streaming off and on again.
        adapter.set_running(false);
        adapter.poll();
        adapter.set_running(true);
        assert!(adapter.snapshot().running);
        assert!(heard_twice(&adapter).is_empty(), "after a restart");
        // Every speaker off, then on again.
        adapter.set_speaker_enabled(&key(SPEAKERS), false);
        adapter.poll();
        assert!(!adapter.snapshot().running);
        turn_on(&mut adapter, SPEAKERS);
        assert!(heard_twice(&adapter).is_empty(), "after the speakers");
        // Another output: a notice that names it.
        adapter
            .audio_mut()
            .connect_output_as(BUDS, "Buds", OutputKind::Bluetooth);
        adapter.audio_mut().play_on(PLAYER_PID, BUDS);
        check_routing(&mut adapter);
        assert_eq!(
            heard_twice(&adapter),
            [twice(Some("Player"), &["Buds", "TV"], &[])]
        );
        // Dismissed, it stays away while that output drops out for a moment.
        adapter.dismiss_all_notices();
        adapter.audio_mut().disconnect_output(BUDS);
        check_routing(&mut adapter);
        adapter
            .audio_mut()
            .connect_output_as(BUDS, "Buds", OutputKind::Bluetooth);
        check_routing(&mut adapter);
        assert!(heard_twice(&adapter).is_empty(), "after the drop-out");
        // The app quits and starts again: a notice about the new one.
        adapter.audio_mut().quit_app(PLAYER_PID);
        adapter.poll();
        assert!(!adapter.snapshot().running);
        adapter.audio_mut().add_app(2100, "player.exe");
        adapter.audio_mut().play_on(2100, TV);
        adapter.set_running(true);
        assert!(adapter.snapshot().running);
        assert_eq!(heard_twice(&adapter), [twice(Some("Player"), &["TV"], &[])]);
    }

    const PASSWORD_TXT: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "pw=true",
    ];

    /// [`adapter`] with an AirPlay sender that lists Kitchen, which can
    /// play, and Study, which asks for a password.
    fn airplay_adapter() -> (AudioAdapter<MockBackend>, FakeAirPlay, Device, Device) {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        audio.add_app(PLAYER_PID, "player.exe");
        let fake = FakeAirPlay::new();
        audio.set_airplay(fake.link(), None);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let study = device(2, "Study", PASSWORD_TXT);
        fake.appear(&kitchen);
        fake.appear(&study);
        let adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        (adapter, fake, kitchen, study)
    }

    /// The UI's ID of an AirPlay speaker.
    fn ap(device: &Device) -> String {
        format!("ap:{}", id_of(device))
    }

    fn local_delay(adapter: &AudioAdapter<MockBackend>, endpoint_id: &str) -> i64 {
        adapter
            .audio
            .playing_params(endpoint_id)
            .expect("it plays")
            .delay_frames
    }

    #[test]
    fn an_airplay_receiver_that_appears_becomes_a_connected_speaker_row() {
        let (mut adapter, fake, kitchen, study) = airplay_adapter();
        let state = adapter.snapshot();
        let row = state.speaker(&ap(&kitchen)).expect("Kitchen is listed");
        assert_eq!(row.name, "Kitchen");
        assert_eq!(row.kind, SpeakerKind::AirPlay);
        assert_eq!(row.delay_ms, None, "AirPlay rows have no sync delay");
        assert_eq!(row.availability, Availability::Open);
        assert!(!row.enabled);
        assert_eq!(
            state.speaker(&ap(&study)).unwrap().availability,
            Availability::NeedsCode
        );
        let (this_pc, connected) = state.layout();
        assert!(this_pc.is_empty());
        let names: Vec<&str> = connected.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Headphones", "Kitchen", "Speakers", "Study"]);

        let bedroom = device(3, "Bedroom", OPEN_TXT);
        fake.appear(&bedroom);
        assert!(adapter.poll(), "the snapshot changed");
        assert!(adapter.snapshot().speaker(&ap(&bedroom)).is_some());
        fake.gone(&bedroom);
        assert!(adapter.poll());
        assert!(adapter.snapshot().speaker(&ap(&bedroom)).is_none());
    }

    #[test]
    fn turning_an_airplay_speaker_on_and_off_lines_local_outputs_up_and_back() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_delay(&key(SPEAKERS), 150.0);
        assert!(adapter.snapshot().running);
        assert_eq!(local_delay(&adapter, SPEAKERS), 7_200, "150 ms after");
        fake.take_calls();

        adapter.set_speaker_enabled(&ap(&kitchen), true);
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);
        assert!(matches!(fake.take_calls()[..], [Call::Add { .. }]));
        assert_eq!(local_delay(&adapter, SPEAKERS), 91_200, "L + 150 ms");

        adapter.set_speaker_enabled(&ap(&kitchen), false);
        assert_eq!(
            local_delay(&adapter, SPEAKERS),
            7_200,
            "its own delay again"
        );
        assert!(adapter.snapshot().running, "Speakers is still on");
    }

    #[test]
    fn an_airplay_speaker_alone_starts_and_stops_streaming() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        assert!(adapter.snapshot().running, "one AirPlay speaker is enough");
        assert!(playing(&adapter).is_empty());
        assert_eq!(fake.take_calls()[0], Call::RestartTimeline);
        adapter.set_speaker_enabled(&ap(&kitchen), false);
        assert!(!adapter.snapshot().running, "nothing is on anymore");
    }

    /// `airplay_adapter()` with an Apple TV that asks for a PIN listed too.
    fn apple_tv_adapter() -> (AudioAdapter<MockBackend>, FakeAirPlay, Device) {
        let (mut adapter, fake, _, _) = airplay_adapter();
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        fake.appear(&tv);
        adapter.poll();
        fake.take_calls();
        (adapter, fake, tv)
    }

    #[test]
    fn the_pin_prompt_names_a_renamed_apple_tv_by_its_new_name() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        adapter.rename_speaker(&id, "Den");
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&tv, PairingEvent::PinRequested);
        adapter.poll();
        assert_eq!(adapter.snapshot().pin_prompt.unwrap().name, "Den");
    }

    #[test]
    fn switching_on_an_apple_tv_pairs_it_and_then_it_streams() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        assert_eq!(
            adapter.snapshot().speaker(&id).unwrap().availability,
            Availability::NeedsCode
        );
        adapter.set_speaker_enabled(&id, true);
        assert_eq!(fake.take_calls(), [Call::StartPairing(id_of(&tv))]);
        let state = adapter.snapshot();
        assert!(!state.running, "nothing streams while it pairs");
        assert_eq!(state.pin_prompt, None, "no prompt until the PIN shows");
        assert_eq!(
            state.speaker(&id).unwrap().availability,
            Availability::Pairing
        );

        fake.pairing(&tv, PairingEvent::PinRequested);
        assert!(adapter.poll());
        assert_eq!(
            adapter.snapshot().pin_prompt,
            Some(PinPrompt {
                speaker_id: id.clone(),
                name: "Den TV".to_owned(),
                phase: PinPhase::Ready,
                wrong_pin: false,
            })
        );
        adapter.submit_pin(&id, "4821");
        assert_eq!(
            fake.take_calls(),
            [Call::SubmitPin(id_of(&tv), "4821".to_owned())]
        );
        assert_eq!(
            adapter.snapshot().pin_prompt.unwrap().phase,
            PinPhase::Checking
        );

        fake.pairing(
            &tv,
            PairingEvent::Paired(credentials("0123456789ABCDEF", 0x11)),
        );
        assert!(adapter.poll());
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        let row = state.speaker(&id).unwrap();
        assert!(row.enabled && row.paired);
        assert_eq!(row.availability, Availability::Open);
        assert!(state.running, "paired and on: streaming starts");
        assert!(
            fake.take_calls().contains(&Call::Credentials(
                id_of(&tv),
                "0123456789ABCDEF".to_owned()
            )),
            "it joins with its credentials"
        );

        adapter.forget_pairing(&id);
        let state = adapter.snapshot();
        let row = state.speaker(&id).unwrap();
        assert!(!row.enabled && !row.paired);
        assert_eq!(row.availability, Availability::NeedsCode);
        assert!(!state.running, "nothing else is on");
        assert_eq!(fake.take_calls().last(), Some(&Call::Remove(id_of(&tv))));
    }

    #[test]
    fn a_second_apple_tv_waits_to_pair_while_the_first_has_the_prompt() {
        let (mut adapter, fake, first) = apple_tv_adapter();
        let second = device(7, "Living Room TV", APPLE_TV_TXT);
        fake.appear(&second);
        adapter.poll();
        let (first_id, second_id) = (ap(&first), ap(&second));
        adapter.set_speaker_enabled(&first_id, true);
        fake.pairing(&first, PairingEvent::PinRequested);
        assert!(adapter.poll());
        adapter.set_speaker_enabled(&second_id, true);
        let availability = |adapter: &AudioAdapter<MockBackend>, id: &str| {
            adapter.snapshot().speaker(id).unwrap().availability
        };
        assert_eq!(availability(&adapter, &first_id), Availability::Pairing);
        assert_eq!(
            availability(&adapter, &second_id),
            Availability::WaitingToPair,
            "the PIN dialog belongs to the first Apple TV"
        );

        adapter.cancel_pairing(&first_id);
        fake.pairing(&second, PairingEvent::PinRequested);
        adapter.poll();
        assert_eq!(
            adapter
                .snapshot()
                .pin_prompt
                .map(|prompt| prompt.speaker_id),
            Some(second_id.clone())
        );
        assert_eq!(availability(&adapter, &second_id), Availability::Pairing);
    }

    #[test]
    fn a_wrong_pin_keeps_the_prompt_and_cancel_leaves_the_speaker_off() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&tv, PairingEvent::PinRequested);
        adapter.poll();
        adapter.submit_pin(&id, "1111");
        fake.take_calls();
        fake.pairing(
            &tv,
            PairingEvent::Failed {
                error: SessionError::new(ErrorKind::PairingFailed),
                wrong_pin: true,
            },
        );
        adapter.poll();
        assert_eq!(fake.take_calls(), [Call::StartPairing(id_of(&tv))]);
        let prompt = adapter.snapshot().pin_prompt.expect("the prompt stays");
        assert!(prompt.wrong_pin);
        assert_eq!(prompt.phase, PinPhase::Starting);
        assert!(messages(&adapter).is_empty(), "no notice for a wrong PIN");

        adapter.cancel_pairing(&id);
        assert_eq!(fake.take_calls(), [Call::CancelPairing(id_of(&tv))]);
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        assert!(!state.speaker(&id).unwrap().enabled);
        assert!(!state.running);
        fake.pairing(&tv, PairingEvent::Stopped);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "no notice for a cancel");
    }

    /// An adapter whose sender has the AirPlay 2 switch `on`, with the
    /// AirPlay 2 receiver Loft listed.
    fn airplay2_adapter(on: bool) -> (AudioAdapter<MockBackend>, FakeAirPlay, Device) {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        let fake = FakeAirPlay::new();
        fake.set_airplay2(on);
        audio.set_airplay(fake.link(), None);
        let loft = airplay2_tv(9, "Loft");
        fake.appear(&loft);
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        adapter.poll();
        fake.take_calls();
        (adapter, fake, loft)
    }

    #[test]
    fn an_airplay2_speaker_asks_for_a_pin_and_plays_once_paired() {
        use crate::audio::pairings::tests::homekit;

        let (mut adapter, fake, loft) = airplay2_adapter(true);
        let id = ap(&loft);
        let row = adapter.snapshot().speaker(&id).unwrap().clone();
        assert_eq!(row.kind, SpeakerKind::AirPlay2);
        assert_eq!(row.availability, Availability::NeedsCode);
        adapter.set_speaker_enabled(&id, true);
        assert_eq!(fake.take_calls(), [Call::StartPairing(id_of(&loft))]);
        fake.pairing(&loft, PairingEvent::PinRequested);
        adapter.poll();
        assert_eq!(
            adapter.snapshot().pin_prompt.unwrap().phase,
            PinPhase::Ready
        );

        // A wrong PIN: the prompt stays and a new pairing starts.
        adapter.submit_pin(&id, "1111");
        fake.take_calls();
        fake.pairing(
            &loft,
            PairingEvent::Failed {
                error: SessionError::new(ErrorKind::PairingFailed),
                wrong_pin: true,
            },
        );
        adapter.poll();
        assert_eq!(fake.take_calls(), [Call::StartPairing(id_of(&loft))]);
        assert!(adapter.snapshot().pin_prompt.unwrap().wrong_pin);
        assert!(messages(&adapter).is_empty(), "no notice for a wrong PIN");

        fake.pairing(&loft, PairingEvent::PinRequested);
        adapter.poll();
        adapter.submit_pin(&id, "4821");
        assert_eq!(
            fake.take_calls(),
            [Call::SubmitPin(id_of(&loft), "4821".to_owned())]
        );
        fake.pairing(&loft, PairingEvent::HomeKitPaired(homekit(0x33)));
        assert!(adapter.poll());
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        let row = state.speaker(&id).unwrap();
        assert!(row.enabled && row.paired);
        assert_eq!(row.availability, Availability::Open);
        assert!(state.running, "paired and on: streaming starts");
        assert!(
            fake.take_calls().contains(&Call::HomeKit(id_of(&loft))),
            "it joins with its AirPlay 2 credentials"
        );
    }

    #[test]
    fn an_airplay2_speaker_row_says_airplay_2_only_with_the_switch_on() {
        let (adapter, _, loft) = airplay2_adapter(false);
        let row = adapter.snapshot().speaker(&ap(&loft)).unwrap().clone();
        assert_eq!(row.kind, SpeakerKind::AirPlay);
        assert_eq!(row.availability, Availability::AirPlay2NotYet);

        let (mut adapter, fake, loft) = airplay2_adapter(true);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        fake.appear(&kitchen);
        adapter.poll();
        let state = adapter.snapshot();
        assert_eq!(
            state.speaker(&ap(&loft)).unwrap().kind,
            SpeakerKind::AirPlay2
        );
        // A receiver AirPlay 1 drives stays "AirPlay" with the switch on.
        assert_eq!(
            state.speaker(&ap(&kitchen)).unwrap().kind,
            SpeakerKind::AirPlay
        );
    }

    /// After `speaker` turned its stored key down: the row asks for a code
    /// again, it is off with a notice saying it needs a PIN, and switching it
    /// on starts a new pairing.
    fn assert_key_forgotten(
        adapter: &mut AudioAdapter<MockBackend>,
        fake: &FakeAirPlay,
        speaker: &Device,
        name: &str,
    ) {
        let id = ap(speaker);
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None, "no prompt opens by itself");
        let row = state.speaker(&id).unwrap();
        assert!(!row.enabled && !row.paired);
        assert_eq!(row.availability, Availability::NeedsCode);
        assert!(!state.running);
        assert_eq!(
            messages(adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: name.to_owned(),
                problem: ui_model::AirPlayProblem::NeedsPin,
            }]
        );
        fake.take_calls();
        adapter.set_speaker_enabled(&id, true);
        assert_eq!(fake.take_calls(), [Call::StartPairing(id_of(speaker))]);
        assert!(messages(adapter).is_empty(), "a new attempt clears it");
    }

    #[test]
    fn an_apple_tv_that_turns_its_stored_key_down_asks_for_a_pin_again() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&tv, PairingEvent::PinRequested);
        adapter.poll();
        adapter.submit_pin(&id, "4821");
        fake.pairing(
            &tv,
            PairingEvent::Paired(credentials("0123456789ABCDEF", 0x11)),
        );
        adapter.poll();
        assert!(adapter.snapshot().speaker(&id).unwrap().paired);

        fake.fail(&tv, ErrorKind::PairingRequired);
        adapter.poll();
        assert_key_forgotten(&mut adapter, &fake, &tv, "Den TV");
    }

    #[test]
    fn an_airplay2_speaker_that_turns_its_stored_key_down_asks_for_a_pin_again() {
        use crate::audio::pairings::tests::homekit;
        use audiosync_airplay::sender::SpeakerEvent as AirPlayEvent;

        let (mut adapter, fake, loft) = airplay2_adapter(true);
        let id = ap(&loft);
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&loft, PairingEvent::PinRequested);
        adapter.poll();
        adapter.submit_pin(&id, "4821");
        fake.pairing(&loft, PairingEvent::HomeKitPaired(homekit(0x33)));
        adapter.poll();
        assert!(adapter.snapshot().speaker(&id).unwrap().paired);

        fake.push(SenderEvent::Speaker {
            key: loft.key.clone(),
            event: AirPlayEvent::HomeKitRejected,
        });
        fake.fail(&loft, ErrorKind::PairingRequired);
        adapter.poll();
        assert_key_forgotten(&mut adapter, &fake, &loft, "Loft");
    }

    #[test]
    fn a_failed_pairing_closes_the_prompt_with_a_notice() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&tv, PairingEvent::PinRequested);
        adapter.poll();
        fake.pairing(
            &tv,
            PairingEvent::Failed {
                error: SessionError::new(ErrorKind::Unreachable),
                wrong_pin: false,
            },
        );
        adapter.poll();
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        assert!(!state.speaker(&id).unwrap().enabled);
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: "Den TV".to_owned(),
                problem: ui_model::AirPlayProblem::PairingFailed,
            }]
        );
    }

    #[test]
    fn auto_connect_never_starts_a_pairing() {
        let (mut adapter, fake, tv) = apple_tv_adapter();
        let id = ap(&tv);
        adapter.set_auto_connect(&id, true);
        fake.gone(&tv);
        adapter.poll();
        fake.appear(&tv);
        adapter.poll();
        assert!(
            !fake
                .take_calls()
                .iter()
                .any(|call| matches!(call, Call::StartPairing(_))),
            "no PIN appears on the TV by itself"
        );
        let state = adapter.snapshot();
        assert_eq!(state.pin_prompt, None);
        assert!(!state.speaker(&id).unwrap().enabled);
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: "Den TV".to_owned(),
                problem: ui_model::AirPlayProblem::NeedsPin,
            }]
        );
    }

    /// Study switched on by the user, with its password prompt open.
    fn study_asks(adapter: &mut AudioAdapter<MockBackend>, fake: &FakeAirPlay, study: &Device) {
        adapter.set_speaker_enabled(&ap(study), true);
        let state = adapter.snapshot();
        assert_eq!(
            state.password_prompt,
            Some(PasswordPrompt {
                speaker_id: ap(study),
                name: "Study".to_owned(),
                wrong: false,
            })
        );
        assert!(!state.speaker(&ap(study)).unwrap().enabled);
        assert!(!state.running);
        assert!(fake.take_calls().is_empty(), "nothing connects before it");
        assert!(messages(adapter).is_empty());
    }

    #[test]
    fn the_password_prompt_names_a_renamed_speaker_by_its_new_name() {
        let (mut adapter, _fake, _, study) = airplay_adapter();
        adapter.poll();
        adapter.rename_speaker(&ap(&study), "Office");
        adapter.set_speaker_enabled(&ap(&study), true);
        assert_eq!(adapter.snapshot().password_prompt.unwrap().name, "Office");
    }

    #[test]
    fn a_password_speaker_asks_for_its_password_and_plays_with_it() {
        let (mut adapter, fake, _, study) = airplay_adapter();
        study_asks(&mut adapter, &fake, &study);
        adapter.submit_password(&ap(&study), "hunter2");
        let state = adapter.snapshot();
        assert_eq!(state.password_prompt, None);
        let row = state.speaker(&ap(&study)).unwrap();
        assert!(row.enabled);
        assert_eq!(row.availability, Availability::Open);
        assert!(state.running, "switched on, it streams");
        assert!(
            fake.take_calls()
                .contains(&Call::Password(id_of(&study), "hunter2".to_owned())),
            "the password reaches the sender"
        );
    }

    #[test]
    fn a_wrong_password_asks_again_and_cancel_leaves_the_speaker_off() {
        let (mut adapter, fake, _, study) = airplay_adapter();
        study_asks(&mut adapter, &fake, &study);
        adapter.submit_password(&ap(&study), "hunter1");
        fake.fail(&study, ErrorKind::WrongPassword);
        adapter.poll();
        let prompt = adapter.snapshot().password_prompt.expect("asked again");
        assert!(prompt.wrong);
        assert!(messages(&adapter).is_empty(), "the prompt says so");

        adapter.cancel_password(&ap(&study));
        let state = adapter.snapshot();
        assert_eq!(state.password_prompt, None);
        assert!(!state.speaker(&ap(&study)).unwrap().enabled);
        assert!(!state.running, "nothing else is on");
        assert!(messages(&adapter).is_empty(), "no notice for a cancel");
    }

    #[test]
    fn a_password_changed_on_the_speaker_prompts_on_the_next_switch_on() {
        let (mut adapter, fake, _, study) = airplay_adapter();
        study_asks(&mut adapter, &fake, &study);
        adapter.submit_password(&ap(&study), "hunter2");
        fake.state(&study, AirPlayState::Playing);
        adapter.poll();
        adapter.set_speaker_enabled(&ap(&study), false);
        adapter.set_speaker_enabled(&ap(&study), true);
        assert_eq!(adapter.snapshot().password_prompt, None, "kept this run");
        fake.fail(&study, ErrorKind::WrongPassword);
        adapter.poll();
        let prompt = adapter.snapshot().password_prompt.expect("asked again");
        assert!(prompt.wrong, "says the old one was turned down");
        assert!(messages(&adapter).is_empty());
    }

    #[test]
    fn a_password_discovery_didnt_announce_prompts_on_the_first_switch_on() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        fake.fail(&kitchen, ErrorKind::PasswordRequired);
        adapter.poll();
        let prompt = adapter.snapshot().password_prompt.expect("asked at once");
        assert!(!prompt.wrong);
        assert!(messages(&adapter).is_empty());
    }

    #[test]
    fn auto_connect_never_asks_for_a_password() {
        let (mut adapter, fake, _, study) = airplay_adapter();
        adapter.set_auto_connect(&ap(&study), true);
        fake.gone(&study);
        adapter.poll();
        fake.appear(&study);
        adapter.poll();
        let state = adapter.snapshot();
        assert_eq!(state.password_prompt, None);
        assert!(!state.speaker(&ap(&study)).unwrap().enabled);
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: "Study".to_owned(),
                problem: ui_model::AirPlayProblem::NeedsPassword,
            }]
        );
    }

    #[test]
    fn airplay_errors_become_notices() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        fake.fail(&kitchen, ErrorKind::Unreachable);
        assert!(adapter.poll());
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayRetrying {
                speaker: "Kitchen".to_owned(),
                problem: ui_model::AirPlayProblem::NotResponding,
            }]
        );
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);
        // It plays again: the notice goes.
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        assert!(messages(&adapter).is_empty());
        // One that needs the user is turned off, and streaming stops with it.
        fake.fail(&kitchen, ErrorKind::PairingRequired);
        adapter.poll();
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayCantPlay {
                speaker: "Kitchen".to_owned(),
                problem: ui_model::AirPlayProblem::PairingNotYet,
            }]
        );
        assert!(!adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);
        assert!(!adapter.snapshot().running);
    }

    #[test]
    fn a_retrying_airplay_speaker_row_says_reconnecting_until_it_plays() {
        let reconnecting = SpeakerState::Waiting(ui_model::Waiting::Reconnecting);
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        assert_eq!(state_of(&adapter, &ap(&kitchen)), SpeakerState::Playing);
        fake.fail(&kitchen, ErrorKind::Unreachable);
        adapter.poll();
        assert_eq!(state_of(&adapter, &ap(&kitchen)), reconnecting);
        // The row keeps saying so after the notice is dismissed.
        adapter.dismiss_all_notices();
        assert_eq!(state_of(&adapter, &ap(&kitchen)), reconnecting);
        // It plays again.
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        assert_eq!(state_of(&adapter, &ap(&kitchen)), SpeakerState::Playing);
        // Retrying, then turned off: off, and turned on again a new attempt.
        fake.fail(&kitchen, ErrorKind::Unreachable);
        adapter.poll();
        assert_eq!(state_of(&adapter, &ap(&kitchen)), reconnecting);
        adapter.set_speaker_enabled(&ap(&kitchen), false);
        assert_eq!(state_of(&adapter, &ap(&kitchen)), SpeakerState::Off);
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        assert_eq!(state_of(&adapter, &ap(&kitchen)), SpeakerState::Playing);
        // Streaming stopped and started again: no stale wait either.
        fake.fail(&kitchen, ErrorKind::Unreachable);
        adapter.poll();
        assert_eq!(state_of(&adapter, &ap(&kitchen)), reconnecting);
        adapter.set_running(false);
        adapter.set_running(true);
        assert_eq!(state_of(&adapter, &ap(&kitchen)), SpeakerState::Playing);
    }

    #[test]
    fn a_speaker_waiting_for_the_airplay2_slot_says_so_not_reconnecting() {
        use audiosync_airplay::error::UnsupportedReason;

        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        fake.fail(
            &kitchen,
            ErrorKind::Unsupported(UnsupportedReason::OneAirPlay2Session),
        );
        adapter.poll();
        assert_eq!(
            state_of(&adapter, &ap(&kitchen)),
            SpeakerState::Waiting(ui_model::Waiting::ForAirPlay2Slot)
        );
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        assert_eq!(state_of(&adapter, &ap(&kitchen)), SpeakerState::Playing);
    }

    #[test]
    fn a_sender_that_stops_running_leaves_a_notice() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        assert!(messages(&adapter).is_empty());
        fake.stop_running();
        adapter.poll();
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayStopped],
            "AirPlay must not go silent without a hint"
        );
        // Nothing it listed is there anymore, and the notice comes once.
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).is_none());
        adapter.dismiss_all_notices();
        adapter.poll();
        assert!(messages(&adapter).is_empty());
    }

    #[test]
    fn the_ptp_port_clash_leaves_a_notice_until_ptp_binds() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        fake.push(SenderEvent::Speaker {
            key: kitchen.key.clone(),
            event: audiosync_airplay::sender::SpeakerEvent::Warning(
                audiosync_airplay::error::Warning::PtpUnavailable,
            ),
        });
        adapter.poll();
        assert_eq!(messages(&adapter), [NoticeMessage::AirPlayPtpPortsTaken]);
        // A speaker on AirPlay 1, or one that didn't bind PTP, says nothing
        // about the ports.
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        assert_eq!(messages(&adapter), [NoticeMessage::AirPlayPtpPortsTaken]);
        fake.state(&kitchen, AirPlayState::Connecting);
        fake.push(SenderEvent::Speaker {
            key: kitchen.key.clone(),
            event: audiosync_airplay::sender::SpeakerEvent::PtpOpened,
        });
        adapter.poll();
        assert!(messages(&adapter).is_empty());
    }

    #[test]
    fn a_ptp_bind_clears_the_notice_even_if_its_session_never_plays() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        fake.push(SenderEvent::Speaker {
            key: kitchen.key.clone(),
            event: audiosync_airplay::sender::SpeakerEvent::Warning(
                audiosync_airplay::error::Warning::PtpUnavailable,
            ),
        });
        adapter.poll();
        assert_eq!(messages(&adapter), [NoticeMessage::AirPlayPtpPortsTaken]);
        // It binds PTP, then fails before playing; the next session binds
        // with no clash to tell and says nothing about the ports.
        fake.push(SenderEvent::Speaker {
            key: kitchen.key.clone(),
            event: audiosync_airplay::sender::SpeakerEvent::PtpOpened,
        });
        fake.fail(&kitchen, ErrorKind::Unreachable);
        adapter.poll();
        fake.state(&kitchen, AirPlayState::Connecting);
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        assert!(
            !messages(&adapter).contains(&NoticeMessage::AirPlayPtpPortsTaken),
            "{:?}",
            messages(&adapter)
        );
    }

    #[test]
    fn a_discovery_failure_leaves_a_notice() {
        let (mut adapter, fake, kitchen, study) = airplay_adapter();
        // The sender reports every receiver gone just before.
        fake.gone(&kitchen);
        fake.gone(&study);
        fake.push(SenderEvent::DiscoveryFailed);
        adapter.poll();
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayStopped],
            "a speaker list that empties for good needs a hint"
        );
    }

    #[test]
    fn retry_starts_a_new_sender_and_the_speakers_that_were_on_come_back() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        let next = FakeAirPlay::new();
        let started = next.clone();
        adapter.audio.set_airplay_starter(Box::new(move || {
            Some(StartedSender::browsing(started.link(), None))
        }));
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        adapter.set_speaker_volume(&ap(&kitchen), 30.0);
        fake.state(&kitchen, AirPlayState::Playing);
        adapter.poll();
        fake.stop_running();
        adapter.poll();
        assert_eq!(messages(&adapter), [NoticeMessage::AirPlayStopped]);

        adapter.retry_airplay();
        assert!(messages(&adapter).is_empty(), "Retry clears the notice");
        assert!(adapter.snapshot().running, "streaming goes on");
        assert_eq!(
            next.take_calls(),
            [Call::RestartTimeline],
            "the capture feeds the new sender"
        );
        next.appear(&kitchen);
        adapter.poll();
        let speaker = speaker_of(&adapter, &ap(&kitchen));
        assert!(speaker.enabled, "on again, as after a launch");
        assert_eq!(speaker.volume, 30.0);
        assert!(matches!(
            next.take_calls().as_slice(),
            [Call::Add { id, .. }] if *id == id_of(&kitchen)
        ));
    }

    #[test]
    fn a_late_retry_leaves_a_working_sender_alone() {
        let (mut adapter, _fake, _, _) = airplay_adapter();
        let starts = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = starts.clone();
        adapter.audio.set_airplay_starter(Box::new(move || {
            counted.set(counted.get() + 1);
            None
        }));
        adapter.retry_airplay();
        assert_eq!(
            starts.get(),
            0,
            "a Retry while the sender runs doesn't replace it"
        );
    }

    #[test]
    fn a_retry_whose_sender_cant_start_keeps_the_notice() {
        let (mut adapter, fake, _, _) = airplay_adapter();
        adapter.audio.set_airplay_starter(Box::new(|| None));
        fake.stop_running();
        adapter.poll();
        adapter.retry_airplay();
        assert_eq!(messages(&adapter), [NoticeMessage::AirPlayStopped]);
    }

    /// An adapter whose AirPlay sender comes from `senders` in turn: the
    /// first at launch, the others at each Retry.
    fn adapter_starting_airplay(senders: Vec<Option<StartedSender>>) -> AudioAdapter<MockBackend> {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        let mut senders = senders.into_iter();
        audio.start_airplay(
            Box::new(move || senders.next().flatten()),
            PairingStore::in_memory(),
        );
        AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default())
    }

    #[test]
    fn a_sender_that_cant_start_at_launch_leaves_a_notice_whose_retry_starts_one() {
        let fake = FakeAirPlay::new();
        let mut adapter =
            adapter_starting_airplay(vec![None, Some(StartedSender::browsing(fake.link(), None))]);
        adapter.poll();
        assert_eq!(messages(&adapter), [NoticeMessage::AirPlayStopped]);

        adapter.retry_airplay();
        assert!(messages(&adapter).is_empty(), "Retry clears the notice");
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        fake.appear(&kitchen);
        adapter.poll();
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).is_some());
    }

    #[test]
    fn a_discovery_that_cant_start_at_launch_leaves_a_notice_whose_retry_starts_a_sender() {
        let first = FakeAirPlay::new();
        let second = FakeAirPlay::new();
        let mut adapter = adapter_starting_airplay(vec![
            Some(StartedSender {
                link: first.link(),
                feed: None,
                browsing: false,
            }),
            Some(StartedSender::browsing(second.link(), None)),
        ]);
        adapter.poll();
        assert_eq!(messages(&adapter), [NoticeMessage::AirPlayStopped]);

        adapter.retry_airplay();
        assert!(messages(&adapter).is_empty());
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        second.appear(&kitchen);
        adapter.poll();
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).is_some());
    }

    /// Twenty seconds of polls: the UI polls twice a second.
    const POLLS_IN_20_S: u32 = 40;

    fn poll_times(adapter: &mut AudioAdapter<MockBackend>, polls: u32) {
        for _ in 0..polls {
            adapter.poll();
        }
    }

    /// Whether the speaker list says that no AirPlay speaker was found.
    fn none_found_hint(adapter: &AudioAdapter<MockBackend>) -> bool {
        adapter.snapshot().airplay_none_found
    }

    /// A sender that browses but finds nothing (a firewall that blocks
    /// discovery, a Public network) must not leave an empty list without a
    /// hint; it is a note in the list, not a notice.
    #[test]
    fn twenty_seconds_of_discovery_without_an_airplay_speaker_show_a_hint() {
        let fake = FakeAirPlay::new();
        let mut adapter =
            adapter_starting_airplay(vec![Some(StartedSender::browsing(fake.link(), None))]);
        assert!(!none_found_hint(&adapter), "not at launch");
        poll_times(&mut adapter, POLLS_IN_20_S - 1);
        assert!(!none_found_hint(&adapter), "not before 20 seconds");
        assert!(adapter.poll(), "the snapshot changes as the hint shows");
        assert!(
            none_found_hint(&adapter),
            "an empty AirPlay list after 20 seconds needs a hint"
        );
        assert!(messages(&adapter).is_empty(), "a note, not a notice");
    }

    /// The flyout's empty list says it is looking until the none-found note
    /// takes over, and stops once a speaker is listed.
    #[test]
    fn discovery_without_a_speaker_is_searching_until_the_none_found_hint() {
        let fake = FakeAirPlay::new();
        let mut adapter =
            adapter_starting_airplay(vec![Some(StartedSender::browsing(fake.link(), None))]);
        assert!(adapter.poll(), "the snapshot changes as the search shows");
        assert!(adapter.snapshot().airplay_searching, "right after launch");
        poll_times(&mut adapter, POLLS_IN_20_S - 1);
        let snapshot = adapter.snapshot();
        assert!(
            !snapshot.airplay_searching,
            "the none-found hint replaces it"
        );
        assert!(snapshot.airplay_none_found);

        let fake = FakeAirPlay::new();
        let mut adapter =
            adapter_starting_airplay(vec![Some(StartedSender::browsing(fake.link(), None))]);
        poll_times(&mut adapter, 2);
        fake.appear(&device(2, "Study", PASSWORD_TXT));
        adapter.poll();
        assert!(!adapter.snapshot().airplay_searching, "a speaker was found");
    }

    #[test]
    fn the_none_found_hint_goes_once_a_speaker_appears_and_comes_back_20_seconds_after_all_are_gone()
     {
        let fake = FakeAirPlay::new();
        let mut adapter =
            adapter_starting_airplay(vec![Some(StartedSender::browsing(fake.link(), None))]);
        poll_times(&mut adapter, POLLS_IN_20_S);
        assert!(none_found_hint(&adapter));

        // Any receiver shows that discovery works, even one AudioSync can't
        // play yet.
        let study = device(2, "Study", PASSWORD_TXT);
        fake.appear(&study);
        assert!(adapter.poll(), "the snapshot changes as the hint goes");
        assert!(!none_found_hint(&adapter), "a speaker was found");
        poll_times(&mut adapter, 3 * POLLS_IN_20_S);
        assert!(
            !none_found_hint(&adapter),
            "it stays away while one is listed"
        );

        fake.gone(&study);
        poll_times(&mut adapter, POLLS_IN_20_S - 1);
        assert!(
            !none_found_hint(&adapter),
            "not before another 20 seconds without one"
        );
        adapter.poll();
        assert!(
            none_found_hint(&adapter),
            "back after 20 seconds without one"
        );
    }

    #[test]
    fn a_speaker_found_within_20_seconds_keeps_the_none_found_hint_away() {
        let fake = FakeAirPlay::new();
        let mut adapter =
            adapter_starting_airplay(vec![Some(StartedSender::browsing(fake.link(), None))]);
        poll_times(&mut adapter, POLLS_IN_20_S - 5);
        fake.appear(&device(1, "Kitchen", OPEN_TXT));
        poll_times(&mut adapter, 3 * POLLS_IN_20_S);
        assert!(!none_found_hint(&adapter));
    }

    /// While discovery is dead, the AirPlay notice and its Retry say so: the
    /// list never adds the hint, even once that notice is dismissed.
    #[test]
    fn no_none_found_hint_while_discovery_is_dead() {
        // Without AirPlay at all.
        let mut without_airplay = adapter();
        poll_times(&mut without_airplay, 3 * POLLS_IN_20_S);
        assert!(!none_found_hint(&without_airplay), "no AirPlay");

        // Discovery couldn't start.
        let fake = FakeAirPlay::new();
        let mut adapter = adapter_starting_airplay(vec![Some(StartedSender {
            link: fake.link(),
            feed: None,
            browsing: false,
        })]);
        poll_times(&mut adapter, 3 * POLLS_IN_20_S);
        assert!(!none_found_hint(&adapter), "discovery couldn't start");
        adapter.dismiss_all_notices();
        poll_times(&mut adapter, 3 * POLLS_IN_20_S);
        assert!(
            !none_found_hint(&adapter),
            "discovery couldn't start, notice dismissed"
        );

        // No sender at all.
        let mut adapter = adapter_starting_airplay(vec![None]);
        poll_times(&mut adapter, 3 * POLLS_IN_20_S);
        assert!(!none_found_hint(&adapter), "no sender");

        // A sender that stopped after an internal error.
        let fake = FakeAirPlay::new();
        let mut adapter =
            adapter_starting_airplay(vec![Some(StartedSender::browsing(fake.link(), None))]);
        fake.stop_running();
        poll_times(&mut adapter, 3 * POLLS_IN_20_S);
        assert!(!none_found_hint(&adapter), "the sender stopped");
    }

    #[test]
    fn the_none_found_hint_goes_when_discovery_stops_and_counts_again_after_retry() {
        let first = FakeAirPlay::new();
        let second = FakeAirPlay::new();
        let mut adapter = adapter_starting_airplay(vec![
            Some(StartedSender::browsing(first.link(), None)),
            Some(StartedSender::browsing(second.link(), None)),
        ]);
        poll_times(&mut adapter, POLLS_IN_20_S);
        assert!(none_found_hint(&adapter));

        first.push(SenderEvent::DiscoveryFailed);
        assert!(adapter.poll());
        assert!(
            !none_found_hint(&adapter),
            "the AirPlay notice and its Retry take over"
        );
        assert_eq!(messages(&adapter), [NoticeMessage::AirPlayStopped]);
        poll_times(&mut adapter, 3 * POLLS_IN_20_S);
        assert!(!none_found_hint(&adapter));

        adapter.retry_airplay();
        poll_times(&mut adapter, POLLS_IN_20_S - 1);
        assert!(
            !none_found_hint(&adapter),
            "the new sender gets its own 20 seconds"
        );
        adapter.poll();
        assert!(none_found_hint(&adapter));
    }

    #[test]
    fn a_source_switch_keeps_airplay_speakers_in_the_stream() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        assert!(adapter.snapshot().running);
        fake.take_calls();
        adapter.select_source(&player());
        assert_eq!(
            captures(&adapter)[1],
            (LoopbackMode::IncludeTree, PLAYER_PID),
            "capture starts again from the player"
        );
        assert_eq!(
            fake.take_calls(),
            [Call::RestartTimeline],
            "the speaker stays in the stream, only its timeline restarts (S141)"
        );
        assert!(adapter.snapshot().running);
    }

    #[test]
    fn zones_and_auto_connect_hold_airplay_speakers() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        adapter.new_zone("Downstairs").unwrap();
        let zone = adapter.snapshot().zones[0].clone();
        let mut members = zone.members.clone();
        members.sort();
        let mut expected = [key(SPEAKERS), ap(&kitchen)];
        expected.sort();
        assert_eq!(members, expected);
        adapter.toggle_zone(&zone.id);
        assert!(!adapter.snapshot().running, "both switched off");
        assert!(!adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);

        // Auto-connect: Kitchen switches on when it appears again.
        adapter.set_auto_connect(&ap(&kitchen), true);
        fake.gone(&kitchen);
        adapter.poll();
        fake.appear(&kitchen);
        adapter.poll();
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).unwrap().enabled);
        assert!(adapter.snapshot().running);
    }

    /// The mock world at a launch: both outputs and the player app.
    fn launch_audio() -> MockBackend {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        audio.add_app(PLAYER_PID, "player.exe");
        audio
    }

    /// The adapter at a launch over `audio`, with the settings in `file`.
    fn launch(audio: MockBackend, file: &std::path::Path) -> AudioAdapter<MockBackend> {
        let file = SettingsFile::at(file.to_path_buf());
        AudioAdapter::with_settings(audio, || "09.41".to_owned(), DebugLog::default(), file)
    }

    fn speaker_of(adapter: &AudioAdapter<MockBackend>, id: &str) -> Speaker {
        adapter.snapshot().speaker(id).cloned().expect("listed")
    }

    #[test]
    fn a_renamed_speaker_keeps_its_name_across_a_restart_until_it_is_cleared() {
        let dir = test_dir("adapter-speaker-rename");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        adapter.poll();
        adapter.rename_speaker(&key(SPEAKERS), "  Desk\nleft ");
        adapter.rename_speaker("lc:never-listed", "Ghost");
        let speakers = speaker_of(&adapter, &key(SPEAKERS));
        assert_eq!(speakers.name, "Desk left");
        assert_eq!(speakers.system_name, "Speakers");
        assert_eq!(speaker_of(&adapter, &key(HEADPHONES)).name, "Headphones");
        adapter.save_settings();

        let mut adapter = launch(launch_audio(), &path);
        adapter.poll();
        assert_eq!(speaker_of(&adapter, &key(SPEAKERS)).name, "Desk left");
        let (saved, _) = SettingsFile::at(path.clone()).load();
        assert_eq!(
            saved.speaker_names.len(),
            1,
            "a speaker never listed isn't named"
        );

        adapter.rename_speaker(&key(SPEAKERS), "");
        assert_eq!(speaker_of(&adapter, &key(SPEAKERS)).name, "Speakers");
        adapter.save_settings();
        let (saved, _) = SettingsFile::at(path).load();
        assert!(saved.speaker_names.is_empty(), "cleared, nothing is kept");
    }

    fn hidden_entry(adapter: &AudioAdapter<MockBackend>, id: &str) -> HiddenSpeaker {
        adapter
            .snapshot()
            .hidden
            .into_iter()
            .find(|hidden| hidden.id == id)
            .expect("in Hidden Speakers")
    }

    #[test]
    fn hiding_a_speaker_that_is_on_turns_it_off_and_lists_it_nowhere() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_speaker_delay(&key(SPEAKERS), 150.0);
        let kind = speaker_of(&adapter, &key(SPEAKERS)).kind;
        assert_eq!(playing(&adapter), [SPEAKERS]);

        adapter.hide_speaker(&key(SPEAKERS), "Speakers kind");
        let state = adapter.snapshot();
        assert!(state.speaker(&key(SPEAKERS)).is_none());
        assert!(playing(&adapter).is_empty(), "turned off first");
        assert!(!state.running);
        assert!(adapter.audio.enabled_outputs().is_empty());
        let entry = hidden_entry(&adapter, &key(SPEAKERS));
        assert_eq!(entry.name, "Speakers");
        assert_eq!(entry.kind_text, "Speakers kind");
        assert_eq!(entry.present, Some(kind));
        assert!(state.hidden_in(kind.is_this_pc()));
        let on = audiosync_remote::Command::Speaker {
            id: key(SPEAKERS),
            change: audiosync_remote::SpeakerChange {
                on: Some(true),
                ..Default::default()
            },
        };
        assert_eq!(
            crate::remote::actions(&on, &state, |_| true),
            Err(crate::remote::Refusal::UnknownSpeaker),
            "the Web Remote can't reach it"
        );

        adapter.hide_speaker(&key(SPEAKERS), "Speakers kind");
        assert_eq!(adapter.snapshot().hidden.len(), 1, "hiding twice keeps one");

        adapter.unhide_speaker(&key(SPEAKERS));
        let row = speaker_of(&adapter, &key(SPEAKERS));
        assert!(!row.enabled, "back off");
        assert_eq!(row.delay_ms, Some(150.0), "its delay was kept");
        assert!(adapter.snapshot().hidden.is_empty());
    }

    #[test]
    fn a_hidden_usb_speaker_moved_to_another_port_stays_hidden() {
        let mut adapter = adapter();
        plug_dac(&mut adapter, DAC_PORT_1);
        adapter.hide_speaker(&key(DAC_PORT_1), "USB");
        unplug(&mut adapter, DAC_PORT_1);
        plug_dac(&mut adapter, DAC_PORT_2);
        assert!(adapter.snapshot().speaker(&key(DAC_PORT_2)).is_none());
        let hidden = adapter.snapshot().hidden;
        assert_eq!(hidden.len(), 1, "no stale entry: {hidden:?}");
        assert_eq!(hidden[0].id, key(DAC_PORT_2));
        assert!(hidden[0].present.is_some());
    }

    #[test]
    fn the_reference_output_cant_be_hidden() {
        let mut adapter = AudioAdapter::new(
            reference_audio(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        adapter.poll();
        let reference = adapter
            .audio
            .outputs()
            .into_iter()
            .find(|output| output.is_reference)
            .expect("a reference");
        adapter.hide_speaker(&key(&reference.id), "Speakers");
        assert!(adapter.snapshot().speaker(&key(&reference.id)).is_some());
        assert!(adapter.snapshot().hidden.is_empty());
    }

    #[test]
    fn a_zone_switches_only_its_members_that_arent_hidden() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        turn_on(&mut adapter, HEADPHONES);
        adapter.new_zone("Desk").unwrap();
        adapter.hide_speaker(&key(HEADPHONES), "Headphones");
        adapter.set_speaker_enabled(&key(SPEAKERS), false);
        let zone = adapter.snapshot().zones[0].id.clone();

        adapter.toggle_zone(&zone);
        assert_eq!(adapter.audio.enabled_outputs(), [SPEAKERS]);
        assert_eq!(playing(&adapter), [SPEAKERS]);
    }

    #[test]
    fn a_hidden_auto_connect_speaker_isnt_turned_on_when_it_appears() {
        let mut adapter = adapter();
        adapter.set_auto_connect(&key(HEADPHONES), true);
        adapter.hide_speaker(&key(HEADPHONES), "Headphones");
        adapter.audio_mut().disconnect_output(HEADPHONES);
        adapter.poll();
        assert_eq!(hidden_entry(&adapter, &key(HEADPHONES)).present, None);
        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        adapter.poll();
        assert!(adapter.snapshot().speaker(&key(HEADPHONES)).is_none());
        assert!(hidden_entry(&adapter, &key(HEADPHONES)).present.is_some());
        assert!(!adapter.snapshot().running);
        assert!(adapter.audio.enabled_outputs().is_empty());

        adapter.unhide_speaker(&key(HEADPHONES));
        let row = speaker_of(&adapter, &key(HEADPHONES));
        assert!(!row.enabled);
        assert!(row.auto_connect, "its mark was kept");
    }

    #[test]
    fn a_hidden_airplay_speaker_stays_hidden_when_it_appears_again() {
        let (mut adapter, fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        adapter.set_auto_connect(&ap(&kitchen), true);
        fake.take_calls();

        adapter.hide_speaker(&ap(&kitchen), "AirPlay");
        assert!(!adapter.is_airplay_enabled(&id_of(&kitchen)));
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).is_none());
        fake.gone(&kitchen);
        adapter.poll();
        let entry = hidden_entry(&adapter, &ap(&kitchen));
        assert_eq!((entry.name.as_str(), entry.present), ("Kitchen", None));
        fake.take_calls();

        fake.appear(&kitchen);
        adapter.poll();
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).is_none());
        assert_eq!(
            hidden_entry(&adapter, &ap(&kitchen)).present,
            Some(SpeakerKind::AirPlay)
        );
        assert!(
            !fake
                .take_calls()
                .iter()
                .any(|call| matches!(call, Call::Add { .. })),
            "auto-connect skips it"
        );
        assert!(!adapter.snapshot().running);
    }

    #[test]
    fn a_hidden_cast_speaker_stays_hidden_when_it_announces_again() {
        let (mut adapter, fake) = cast_adapter();
        adapter.hide_speaker(&cc(CAST_KITCHEN), "Google Cast");
        assert!(adapter.snapshot().speaker(&cc(CAST_KITCHEN)).is_none());
        fake.announce(CAST_KITCHEN, "Kitchen", DEVICE_PORT);
        adapter.poll();
        assert!(adapter.snapshot().speaker(&cc(CAST_KITCHEN)).is_none());
        assert!(adapter.snapshot().speaker(&cc(CAST_LIVING)).is_some());
        let entry = hidden_entry(&adapter, &cc(CAST_KITCHEN));
        assert_eq!(entry.present, Some(SpeakerKind::Cast));
        assert_eq!(entry.kind_text, "Google Cast");
    }

    #[test]
    fn hidden_speakers_survive_a_restart_with_what_was_set_for_them() {
        let dir = test_dir("adapter-hidden-speakers");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        adapter.poll();
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_delay(&key(HEADPHONES), 120.0);
        adapter.hide_speaker(&key(HEADPHONES), "Headphones kind");
        adapter.save_settings();
        let (saved, _) = SettingsFile::at(path.clone()).load();
        assert_eq!(
            saved.hidden_speakers,
            [SavedHidden {
                key: key(HEADPHONES),
                name: "Headphones".into(),
                kind: "Headphones kind".into(),
            }]
        );

        let mut adapter = launch(launch_audio(), &path);
        adapter.poll();
        assert!(adapter.snapshot().speaker(&key(HEADPHONES)).is_none());
        assert!(adapter.audio.enabled_outputs().is_empty());
        adapter.unhide_speaker(&key(HEADPHONES));
        let row = speaker_of(&adapter, &key(HEADPHONES));
        assert!(!row.enabled);
        assert_eq!(row.delay_ms, Some(120.0));
        adapter.save_settings();
        let (saved, _) = SettingsFile::at(path).load();
        assert!(saved.hidden_speakers.is_empty());
    }

    #[test]
    fn a_speaker_saved_on_and_hidden_isnt_turned_on_at_a_launch() {
        let dir = test_dir("adapter-hidden-saved-on");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        adapter.poll();
        turn_on(&mut adapter, SPEAKERS);
        adapter.save_settings();
        let (mut saved, _) = SettingsFile::at(path.clone()).load();
        saved.hidden_speakers.push(SavedHidden {
            key: key(SPEAKERS),
            name: "Speakers".into(),
            kind: String::new(),
        });
        SettingsFile::at(path.clone()).save(&saved).unwrap();

        let mut adapter = launch(launch_audio(), &path);
        adapter.poll();
        assert!(adapter.audio.enabled_outputs().is_empty());
        assert!(!adapter.snapshot().running);
    }

    #[test]
    fn a_speaker_gone_mid_rename_still_takes_the_typed_name() {
        let dir = test_dir("adapter-speaker-rename-gone");
        let mut adapter = launch(launch_audio(), &dir.join(settings::FILE_NAME));
        adapter.poll();
        unplug(&mut adapter, HEADPHONES);
        assert!(adapter.snapshot().speaker(&key(HEADPHONES)).is_none());
        adapter.rename_speaker(&key(HEADPHONES), "Studio cans");

        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        adapter.poll();
        let headphones = speaker_of(&adapter, &key(HEADPHONES));
        assert_eq!(headphones.name, "Studio cans");
        assert_eq!(headphones.system_name, "Headphones");
    }

    #[test]
    fn the_users_choices_survive_a_restart_without_streaming() {
        let dir = test_dir("adapter-settings-restart");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        adapter.poll();
        assert!(!path.exists(), "nothing changed, nothing written");

        adapter.select_source(&player());
        adapter.set_master_volume(70.0);
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_speaker_volume(&key(SPEAKERS), 35.0);
        adapter.set_speaker_delay(&key(SPEAKERS), 240.0);
        adapter.set_speaker_delay(&key(HEADPHONES), 15.0);
        adapter.new_zone("Desk").expect("made");
        let mut equalizer = EqSettings {
            enabled: true,
            ..EqSettings::default()
        };
        equalizer.gains_db[0] = 4.5;
        adapter.set_equalizer(equalizer);
        let curves = vec![UserCurve {
            name: "Desk".into(),
            gains_db: equalizer.gains_db,
        }];
        adapter.set_equalizer_curves(curves.clone());
        adapter.set_view_settings(ViewSettings {
            show_virtual_devices: true,
            appearance: ui_model::Appearance::Light,
            window: None,
            remote: crate::remote::RemoteSettings {
                on: true,
                port: 8123,
            },
            airplay2_experimental: true,
        });
        assert!(adapter.snapshot().running);
        adapter.poll();
        let zones = adapter.snapshot().zones;

        // The app quits while streaming, and starts again with the player
        // running under a new process ID.
        let mut audio = launch_audio();
        audio.quit_app(PLAYER_PID);
        audio.add_app(PLAYER_PID + 7, "player.exe");
        let mut adapter = launch(audio, &path);
        let state = adapter.snapshot();
        assert!(!state.running, "a launch doesn't start streaming by itself");
        assert_eq!(
            state.source_id,
            format!("app:{}:player.exe", PLAYER_PID + 7)
        );
        assert_eq!(state.master_volume, 70.0);
        let speakers = speaker_of(&adapter, &key(SPEAKERS));
        assert!(speakers.enabled);
        assert_eq!(speakers.volume, 35.0);
        assert_eq!(speakers.delay_ms, Some(240.0));
        let headphones = speaker_of(&adapter, &key(HEADPHONES));
        assert!(!headphones.enabled);
        assert_eq!(headphones.delay_ms, Some(15.0));
        assert_eq!(state.zones, zones);
        assert_eq!(state.equalizer, equalizer);
        assert_eq!(state.equalizer_curves, curves);
        assert!(adapter.view_settings().show_virtual_devices);
        assert!(adapter.view_settings().airplay2_experimental);
        assert_eq!(
            adapter.view_settings().appearance,
            ui_model::Appearance::Light
        );
        assert_eq!(
            adapter.view_settings().remote,
            crate::remote::RemoteSettings {
                on: true,
                port: 8123
            }
        );

        // A new zone gets the next number, and streaming starts from the
        // saved speakers when the user asks.
        adapter.new_zone("Two").expect("made");
        assert_eq!(adapter.snapshot().zones[1].id, "zone:2");
        adapter.set_running(true);
        assert_eq!(playing(&adapter), [SPEAKERS]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The PC's own speakers (the reference), a Bluetooth speaker and a TV,
    /// as at a launch.
    #[test]
    fn a_calibration_that_cant_run_says_why_and_leaves_the_delay() {
        let mut adapter = AudioAdapter::new(
            reference_audio(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        let earbuds = key("bluetooth");
        adapter.set_speaker_delay(&earbuds, 80.0);
        adapter.start_calibration(&earbuds);
        let state = adapter.snapshot();
        assert_eq!(
            state.calibration.speaker_id.as_deref(),
            Some(earbuds.as_str())
        );
        assert_eq!(
            state.calibration.phase,
            calibration::Phase::Failed(calibration::Failure::NotPlaying)
        );
        assert!(!state.calibration.busy);
        // Streaming, but the mock has no microphone: refused before the
        // music fades, and nothing holds the outputs.
        turn_on(&mut adapter, "builtin");
        turn_on(&mut adapter, "bluetooth");
        adapter.start_calibration(&earbuds);
        let state = adapter.snapshot();
        assert_eq!(
            state.calibration.phase,
            calibration::Phase::Failed(calibration::Failure::NoMicrophone)
        );
        assert!(!state.calibration.busy);
        assert_eq!(speaker_of(&adapter, &earbuds).delay_ms, Some(80.0));
        // The reference itself has nothing to calibrate.
        adapter.start_calibration(&key("builtin"));
        assert_eq!(
            adapter.snapshot().calibration.phase,
            calibration::Phase::Failed(calibration::Failure::NoReference)
        );
        // A delay set by hand takes the line about that speaker away.
        adapter.start_calibration(&earbuds);
        adapter.set_speaker_delay(&earbuds, 90.0);
        assert_eq!(
            adapter.snapshot().calibration.phase,
            calibration::Phase::Idle
        );
    }

    #[test]
    fn a_delay_step_past_the_maximum_keeps_the_calibrations_line() {
        let mut adapter = AudioAdapter::new(
            reference_audio(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        let earbuds = key("bluetooth");
        adapter.set_speaker_delay(&earbuds, 20_000.0);
        adapter.start_calibration(&earbuds);
        let line = adapter.snapshot().calibration.phase;
        assert_ne!(line, calibration::Phase::Idle);
        // The window's + step asks for 20 010 ms, the web remote's for the
        // 20 000 ms it already has; neither changes the offset.
        adapter.set_speaker_delay(&earbuds, 20_010.0);
        adapter.set_speaker_delay(&earbuds, 20_000.0);
        assert_eq!(adapter.snapshot().calibration.phase, line);
        assert_eq!(speaker_of(&adapter, &earbuds).delay_ms, Some(20_000.0));
    }

    #[test]
    fn a_calibrations_line_goes_once_its_speaker_is_turned_off() {
        let mut adapter = AudioAdapter::new(
            reference_audio(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        turn_on(&mut adapter, "builtin");
        turn_on(&mut adapter, "bluetooth");
        adapter.start_calibration(&key("bluetooth"));
        assert_ne!(
            adapter.snapshot().calibration.phase,
            calibration::Phase::Idle
        );
        adapter.set_speaker_enabled(&key("bluetooth"), false);
        adapter.poll();
        assert_eq!(
            adapter.snapshot().calibration.phase,
            calibration::Phase::Idle
        );
    }

    #[test]
    fn calibrate_all_applies_every_clear_row_at_once_and_one_undo_puts_them_back() {
        let mut adapter = AudioAdapter::new(
            reference_audio(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        for id in ["builtin", "bluetooth", "tv"] {
            turn_on(&mut adapter, id);
        }
        adapter.set_speaker_delay(&key("bluetooth"), 80.0);
        adapter.set_speaker_delay(&key("tv"), -20.0);
        let (sheet, members, rate) = crate::audio::calibrate_all::tests::local_sheet(&[
            ("builtin", 0.0, 2010.0),
            ("bluetooth", 80.0, 2190.0),
            ("tv", -20.0, 1950.0),
        ]);
        let measured: Vec<(String, f64)> = sheet
            .rows()
            .iter()
            .filter_map(|row| match row.state {
                RowState::Certain(m) => {
                    let id = row.id.strip_prefix("local:").expect("a local row");
                    Some((key(id), m.new_ms.round()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(measured.len(), 2, "both speakers were measured clearly");
        adapter.calibrate_all.sheet = Some(KeptSheet {
            sheet,
            members,
            rate,
            failure: None,
            again_ended: None,
            applied: None,
            undone: false,
        });
        let shown = adapter.snapshot().calibrate_all.sheet.expect("results");
        assert!(shown.can_apply && !shown.can_undo);

        adapter.apply_calibrate_all();
        for (id, ms) in &measured {
            assert_eq!(speaker_of(&adapter, id).delay_ms, Some(*ms));
        }
        // The stored delays are what Apply wrote, so the results and Undo stay.
        adapter.poll();
        let shown = adapter.snapshot().calibrate_all.sheet.expect("results");
        assert!(shown.applied && shown.can_undo && !shown.can_apply);

        adapter.undo_calibrate_all();
        assert_eq!(speaker_of(&adapter, &key("bluetooth")).delay_ms, Some(80.0));
        assert_eq!(speaker_of(&adapter, &key("tv")).delay_ms, Some(-20.0));
        let shown = adapter.snapshot().calibrate_all.sheet.expect("results");
        assert!(shown.undone && !shown.can_undo && !shown.can_apply);
    }

    #[test]
    fn calibrate_all_results_go_once_a_measured_delay_changes_before_apply() {
        let mut adapter = AudioAdapter::new(
            reference_audio(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        for id in ["builtin", "bluetooth"] {
            turn_on(&mut adapter, id);
        }
        adapter.set_speaker_delay(&key("bluetooth"), 80.0);
        let (sheet, members, rate) = crate::audio::calibrate_all::tests::local_sheet(&[
            ("builtin", 0.0, 2010.0),
            ("bluetooth", 80.0, 2190.0),
        ]);
        adapter.calibrate_all.sheet = Some(KeptSheet {
            sheet,
            members,
            rate,
            failure: None,
            again_ended: None,
            applied: None,
            undone: false,
        });
        adapter.poll();
        assert!(adapter.snapshot().calibrate_all.sheet.is_some());

        adapter.set_speaker_delay(&key("bluetooth"), 95.0);
        adapter.poll();
        let state = adapter.snapshot().calibrate_all;
        assert_eq!(state.sheet, None, "Apply would undo the new delay");
        assert_eq!(state.phase, AllPhase::Stale);
        assert_eq!(speaker_of(&adapter, &key("bluetooth")).delay_ms, Some(95.0));
    }

    fn keep(
        adapter: &mut AudioAdapter<MockBackend>,
        (sheet, members, rate): (Sheet, Vec<Member>, u32),
    ) {
        adapter.calibrate_all.sheet = Some(KeptSheet {
            sheet,
            members,
            rate,
            failure: None,
            again_ended: None,
            applied: None,
            undone: false,
        });
    }

    #[test]
    fn undo_puts_back_a_cast_speaker_without_a_stored_offset_as_none() {
        let (mut adapter, _) = cast_adapter();
        keep(
            &mut adapter,
            crate::audio::calibrate_all::tests::sheet_of(&[
                (Class::Local, SPEAKERS, Some(0.0), 2010.0),
                (Class::Local, HEADPHONES, Some(0.0), 2060.0),
                (Class::Cast, CAST_KITCHEN, None, 3100.0),
            ]),
        );
        let offset = |adapter: &AudioAdapter<MockBackend>| {
            adapter
                .audio
                .cast_speakers()
                .into_iter()
                .find(|cast| cast.id == CAST_KITCHEN)
                .map(|cast| cast.offset_ms)
        };
        adapter.apply_calibrate_all();
        assert!(offset(&adapter).is_some_and(|ms| ms > 0.0));
        assert_ne!(speaker_of(&adapter, &key(HEADPHONES)).delay_ms, Some(0.0));

        adapter.undo_calibrate_all();
        // An offset of 0 is none: the next run searches for the speaker
        // again from the core's cap.
        assert_eq!(offset(&adapter), Some(0.0));
        assert_eq!(speaker_of(&adapter, &cc(CAST_KITCHEN)).delay_ms, Some(0.0));
        assert_eq!(speaker_of(&adapter, &key(HEADPHONES)).delay_ms, Some(0.0));
    }

    #[test]
    fn apply_waits_for_airplay_to_play_rather_than_leave_its_delay_out() {
        let mut adapter = adapter();
        turn_on(&mut adapter, SPEAKERS);
        keep(
            &mut adapter,
            crate::audio::calibrate_all::tests::sheet_of(&[
                (Class::Local, SPEAKERS, Some(0.0), 2010.0),
                (Class::Local, HEADPHONES, Some(0.0), 2060.0),
                (Class::AirPlay, "kitchen", Some(2000.0), 2050.0),
            ]),
        );
        assert_eq!(adapter.audio.airplay_sender_latency_ms(), None);
        let shown = adapter.snapshot().calibrate_all.sheet.expect("results");
        assert!(shown.airplay_waits && !shown.can_apply);

        adapter.apply_calibrate_all();
        let shown = adapter.snapshot().calibrate_all.sheet.expect("results");
        assert!(!shown.applied, "nothing was set");
        assert_eq!(speaker_of(&adapter, &key(HEADPHONES)).delay_ms, Some(0.0));
    }

    #[test]
    fn calibrate_all_says_why_it_cant_start() {
        let mut adapter = AudioAdapter::new(
            reference_audio(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        assert_eq!(
            adapter.snapshot().calibrate_all.blocked,
            Some(Failure::NotPlaying)
        );
        turn_on(&mut adapter, "builtin");
        assert_eq!(
            adapter.snapshot().calibrate_all.blocked,
            Some(Failure::NothingToMeasure)
        );
        turn_on(&mut adapter, "bluetooth");
        assert_eq!(adapter.snapshot().calibrate_all.blocked, None);
        // The mock has no microphone: refused before the music fades.
        adapter.start_calibrate_all();
        let state = adapter.snapshot();
        assert_eq!(
            state.calibrate_all.phase,
            AllPhase::Failed(Failure::NoMicrophone)
        );
        assert!(!state.calibration.busy);
    }

    #[test]
    fn headphones_that_are_on_are_listed_left_out_until_included_anyway() {
        let mut audio = reference_audio();
        audio.set_output_traits(
            "bluetooth",
            crate::audio::outputs::DeviceTraits {
                form_factor: Some(3),
                enumerator: Some("BTHENUM".to_owned()),
            },
        );
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        turn_on(&mut adapter, "builtin");
        turn_on(&mut adapter, "bluetooth");
        let state = adapter.snapshot();
        assert_eq!(state.calibrate_all.blocked, Some(Failure::OnlyLeftOut));
        assert_eq!(
            state.calibrate_all.left_out,
            vec![LeftOutRow {
                id: "bluetooth".to_owned(),
                name: "Earbuds".to_owned(),
                kind: MicKind::Headphones,
                included: false,
            }]
        );
        assert_eq!(
            speaker_of(&adapter, &speaker_key("bluetooth")).mic_left_out,
            Some(MicKind::Headphones)
        );
        assert_eq!(
            speaker_of(&adapter, &speaker_key("builtin")).mic_left_out,
            None
        );
        assert_eq!(speaker_of(&adapter, &speaker_key("tv")).mic_left_out, None);
        // Its row offers no Calibrate with Microphone, and none starts.
        adapter.start_calibration(&speaker_key("bluetooth"));
        assert_eq!(adapter.snapshot().calibration.speaker_id, None);

        adapter.include_in_calibration("bluetooth", true);
        let state = adapter.snapshot();
        assert_eq!(state.calibrate_all.blocked, None);
        assert!(state.calibrate_all.left_out[0].included);
        adapter.include_in_calibration("bluetooth", false);
        assert_eq!(
            adapter.snapshot().calibrate_all.blocked,
            Some(Failure::OnlyLeftOut)
        );
        adapter.include_in_calibration("bluetooth", true);
        adapter.close_calibrate_all();
        assert!(
            !adapter.snapshot().calibrate_all.left_out[0].included,
            "Include Anyway lasts until the results close"
        );
    }

    fn reference_audio() -> MockBackend {
        let mut audio = MockBackend::empty();
        audio.connect_output_as("builtin", "Speakers", OutputKind::BuiltIn);
        audio.connect_output_as("bluetooth", "Earbuds", OutputKind::Bluetooth);
        audio.connect_output_as("tv", "TV", OutputKind::Hdmi);
        audio
    }

    #[test]
    fn the_sync_hint_names_a_renamed_reference_by_its_new_name() {
        let mut adapter = AudioAdapter::new(
            reference_audio(),
            || "09.41".to_owned(),
            DebugLog::default(),
        );
        adapter.poll();
        adapter.rename_speaker(&key("builtin"), "Desk");
        assert_eq!(adapter.snapshot().reference_name.as_deref(), Some("Desk"));
    }

    #[test]
    fn an_older_settings_file_is_migrated_once_and_saved_in_the_new_version() {
        let dir = test_dir("adapter-settings-delay-migration");
        let path = dir.join(settings::FILE_NAME);
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "outputs": {
                    "builtin": {"enabled": true, "delay_ms": 250},
                    "bluetooth": {"enabled": true, "delay_ms": 0},
                    "tv": {"delay_ms": 120}
                }
            }"#,
        )
        .expect("written");
        let adapter = launch(reference_audio(), &path);
        let state = adapter.snapshot();
        assert_eq!(state.reference_name.as_deref(), Some("Speakers"));
        assert_eq!(speaker_of(&adapter, &key("builtin")).delay_ms, None);
        assert_eq!(
            speaker_of(&adapter, &key("bluetooth")).delay_ms,
            Some(-250.0)
        );
        assert_eq!(speaker_of(&adapter, &key("tv")).delay_ms, Some(-130.0));
        assert_eq!(adapter.audio.airplay_adjustment(), 250.0);
        let (saved, found) = SettingsFile::at(path.clone()).load();
        assert_eq!(
            found,
            settings::Loaded::Read {
                version: settings::VERSION,
                adjusted: false
            }
        );
        assert_eq!(settings::VERSION, 3);
        assert_eq!(saved.outputs["bluetooth"].delay_ms, -250.0);
        assert_eq!(saved.outputs["tv"].delay_ms, -130.0);
        assert_eq!(saved.outputs["builtin"].delay_ms, 0.0);
        assert!(saved.outputs["builtin"].enabled);
        assert_eq!(saved.airplay_adjustment_ms, 250.0);

        // The next launch reads the new format and migrates nothing again.
        let adapter = launch(reference_audio(), &path);
        assert_eq!(
            speaker_of(&adapter, &key("bluetooth")).delay_ms,
            Some(-250.0)
        );
        assert_eq!(adapter.audio.airplay_adjustment(), 250.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn auto_connect_at_the_first_launch_after_the_upgrade_starts_at_the_migrated_delays() {
        let dir = test_dir("adapter-settings-delay-migration-auto-connect");
        let path = dir.join(settings::FILE_NAME);
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "outputs": {
                    "builtin": {"enabled": true, "delay_ms": 250},
                    "bluetooth": {"enabled": true, "delay_ms": 0}
                },
                "auto_connect": ["lc:bluetooth"]
            }"#,
        )
        .expect("written");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut audio = reference_audio();
        audio.set_debug_log(log.clone());
        let file = SettingsFile::at(path.clone());
        let adapter = AudioAdapter::with_settings(audio, || "09.41".to_owned(), log.clone(), file);
        assert!(
            adapter.snapshot().running,
            "the auto-connect speaker starts"
        );
        assert_eq!(local_delay(&adapter, "builtin"), 12_000, "250 ms at 48 kHz");
        assert_eq!(local_delay(&adapter, "bluetooth"), 0);
        assert!(log.flush(TEST_FLUSH));
        let messages = read_messages(&dir.join(FILE_NAME));
        assert!(
            !messages.iter().any(|message| message.contains("delay now")),
            "the outputs start at their migrated hold-backs, not re-timed: {messages:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_older_file_waits_for_an_output_list_before_it_is_migrated() {
        let dir = test_dir("adapter-settings-delay-migration-wait");
        let path = dir.join(settings::FILE_NAME);
        std::fs::write(
            &path,
            r#"{"version": 1, "outputs": {"builtin": {"delay_ms": 250}, "tv": {"delay_ms": 120}}}"#,
        )
        .expect("written");
        let mut audio = reference_audio();
        audio.fail_listing(Some(BackendError::System {
            call: "IMMDeviceEnumerator::EnumAudioEndpoints",
            code: -1,
        }));
        let mut adapter = launch(audio, &path);
        adapter.poll();
        let (saved, found) = SettingsFile::at(path.clone()).load();
        assert!(
            matches!(found, settings::Loaded::Read { version: 1, .. }),
            "not saved before the migration: {found}"
        );
        assert_eq!(saved.outputs["tv"].delay_ms, 120.0);
        adapter.audio_mut().fail_listing(None);
        adapter.poll();
        assert_eq!(speaker_of(&adapter, &key("tv")).delay_ms, Some(-130.0));
        let (saved, _) = SettingsFile::at(path.clone()).load();
        assert_eq!(saved.outputs["tv"].delay_ms, -130.0);
        assert_eq!(saved.airplay_adjustment_ms, 250.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_airplay_delay_field_shows_l_plus_a_and_follows_a_new_l() {
        let (mut with_sender, fake, _, _) = airplay_adapter();
        let shown = |adapter: &AudioAdapter<MockBackend>| adapter.snapshot().airplay_delay_ms;
        assert_eq!(shown(&with_sender), Some(1750.0), "L_s + 0");
        with_sender.set_airplay_delay(2000.0);
        assert_eq!(shown(&with_sender), Some(2000.0));
        assert_eq!(with_sender.audio.airplay_adjustment(), 250.0);
        fake.set_latency(std::time::Duration::from_secs(1));
        with_sender.poll();
        assert_eq!(shown(&with_sender), Some(1250.0));
        with_sender.set_airplay_delay(f64::NAN);
        assert_eq!(shown(&with_sender), Some(1250.0));
        with_sender.set_airplay_delay(-500.0);
        assert_eq!(shown(&with_sender), Some(0.0), "clamped");
        // Without an AirPlay sender there is no field.
        assert_eq!(shown(&adapter()), None);
    }

    #[test]
    fn an_auto_connect_speaker_that_is_there_at_launch_starts_streaming() {
        let dir = test_dir("adapter-settings-auto-connect");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        adapter.set_auto_connect(&key(HEADPHONES), true);
        adapter.save_settings();

        let adapter = launch(launch_audio(), &path);
        assert!(adapter.snapshot().running);
        assert_eq!(playing(&adapter), [HEADPHONES]);

        // Not there at launch: nothing starts until it is plugged in.
        let mut audio = launch_audio();
        audio.disconnect_output(HEADPHONES);
        let mut adapter = launch(audio, &path);
        assert!(!adapter.snapshot().running);
        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        adapter.poll();
        assert_eq!(playing(&adapter), [HEADPHONES]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_device_missing_at_launch_keeps_its_settings_until_it_is_back() {
        let dir = test_dir("adapter-settings-missing");
        let path = dir.join(settings::FILE_NAME);
        let mut adapter = launch(launch_audio(), &path);
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_delay(&key(HEADPHONES), 120.0);
        adapter.set_speaker_volume(&key(HEADPHONES), 25.0);
        adapter.set_running(false);
        adapter.poll();

        let mut audio = launch_audio();
        audio.disconnect_output(HEADPHONES);
        let mut adapter = launch(audio, &path);
        assert!(adapter.snapshot().speaker(&key(HEADPHONES)).is_none());
        // Other changes are saved; the missing device's settings stay.
        adapter.set_speaker_volume(&key(SPEAKERS), 80.0);
        adapter.poll();
        let (saved, _) = SettingsFile::at(path.clone()).load();
        assert_eq!(
            saved.outputs.get(HEADPHONES),
            Some(&SavedSpeaker {
                enabled: true,
                volume: 25.0,
                delay_ms: 120.0
            })
        );
        assert_eq!(saved.outputs[SPEAKERS].volume, 80.0);

        adapter.audio_mut().connect_output(HEADPHONES, "Headphones");
        adapter.poll();
        let headphones = speaker_of(&adapter, &key(HEADPHONES));
        assert!(headphones.enabled);
        assert_eq!(headphones.volume, 25.0);
        assert_eq!(headphones.delay_ms, Some(120.0));
        assert!(
            !adapter.snapshot().running,
            "it isn't an auto-connect speaker"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn airplay_speakers_get_their_settings_back_when_found() {
        let dir = test_dir("adapter-settings-airplay");
        let path = dir.join(settings::FILE_NAME);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let with_airplay = |fake: &FakeAirPlay| {
            let mut audio = launch_audio();
            audio.set_airplay(fake.link(), None);
            audio
        };
        let fake = FakeAirPlay::new();
        fake.appear(&kitchen);
        let mut adapter = launch(with_airplay(&fake), &path);
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        adapter.set_speaker_volume(&ap(&kitchen), 30.0);
        adapter.set_running(false);
        adapter.poll();

        // Found only after the launch, as discovery does.
        let fake = FakeAirPlay::new();
        let mut adapter = launch(with_airplay(&fake), &path);
        assert!(adapter.snapshot().speaker(&ap(&kitchen)).is_none());
        fake.appear(&kitchen);
        adapter.poll();
        let speaker = speaker_of(&adapter, &ap(&kitchen));
        assert!(speaker.enabled);
        assert_eq!(speaker.volume, 30.0);

        // Without an AirPlay sender the speaker stays off, and its settings
        // stay saved for a launch that has one.
        let mut adapter = launch(launch_audio(), &path);
        adapter.set_master_volume(50.0);
        adapter.poll();
        let (saved, _) = SettingsFile::at(path.clone()).load();
        assert_eq!(saved.master_volume, 50.0);
        assert_eq!(
            saved.airplay.get(&id_of(&kitchen)),
            Some(&SavedSpeaker {
                enabled: true,
                volume: 30.0,
                delay_ms: 0.0
            })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_settings_file_is_logged_and_ignored_then_replaced() {
        let dir = test_dir("adapter-settings-log");
        let path = dir.join(settings::FILE_NAME);
        std::fs::write(&path, "{\"version\": 1, \"outputs\": ").expect("written");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let file = SettingsFile::at(path.clone());
        let mut adapter =
            AudioAdapter::with_settings(launch_audio(), || "09.41".to_owned(), log.clone(), file);
        assert!(!adapter.snapshot().running);
        assert_eq!(
            speaker_of(&adapter, &key(SPEAKERS)).volume,
            DEFAULT_LOCAL_POSITION
        );
        adapter.poll();
        assert!(
            std::fs::read_to_string(&path)
                .expect("still there")
                .ends_with(": "),
            "an ignored file is left alone until something changes"
        );
        adapter.set_master_volume(64.0);
        adapter.poll();
        assert_eq!(SettingsFile::at(path.clone()).load().0.master_volume, 64.0);

        let file = SettingsFile::at(path.clone());
        let _adapter =
            AudioAdapter::with_settings(launch_audio(), || "09.41".to_owned(), log.clone(), file);
        assert!(log.flush(TEST_FLUSH));
        assert_in_order(
            &read_messages(&dir.join(FILE_NAME)),
            &[
                "settings: ignored: it isn't valid (",
                "settings: saved",
                "settings: read (format version 3)",
            ],
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_newer_settings_file_is_kept_aside_once_and_logged() {
        let dir = test_dir("adapter-settings-newer");
        let path = dir.join(settings::FILE_NAME);
        let newer = format!("{{\"version\": {}}}", settings::VERSION + 1);
        std::fs::write(&path, &newer).expect("written");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let file = SettingsFile::at(path.clone());
        let mut adapter =
            AudioAdapter::with_settings(launch_audio(), || "09.41".to_owned(), log.clone(), file);
        adapter.set_master_volume(64.0);
        adapter.poll();
        adapter.set_master_volume(65.0);
        adapter.poll();
        assert_eq!(
            crate::data_folder::copies_kept_aside(&dir, "settings.newer"),
            [newer.into_bytes()]
        );
        assert!(log.flush(TEST_FLUSH));
        let messages = read_messages(&dir.join(FILE_NAME));
        assert_in_order(
            &messages,
            &[
                "settings: ignored: a newer AudioSync wrote it",
                "settings: a newer AudioSync's file was kept as settings.newer-",
                "settings: saved",
                "settings: saved",
            ],
        );
        let kept_lines = messages
            .iter()
            .filter(|message| message.contains("was kept as"))
            .count();
        assert_eq!(kept_lines, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_airplay_speaker_on_while_windows_has_no_output_gets_a_notice() {
        let (mut adapter, _fake, kitchen, _) = airplay_adapter();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "Windows has outputs");
        adapter.audio_mut().disconnect_output(SPEAKERS);
        adapter.audio_mut().disconnect_output(HEADPHONES);
        assert!(adapter.poll());
        assert_eq!(messages(&adapter), [NoticeMessage::NoSoundOutput]);
        // Dismissed, it stays away while nothing changes.
        adapter.dismiss_all_notices();
        adapter.poll();
        assert!(messages(&adapter).is_empty());
        // An output back clears it; all gone again, it comes back.
        adapter.audio_mut().connect_output(SPEAKERS, "Speakers");
        adapter.poll();
        adapter.audio_mut().disconnect_output(SPEAKERS);
        adapter.poll();
        assert_eq!(messages(&adapter), [NoticeMessage::NoSoundOutput]);
        // With the AirPlay speaker off there's nothing to tell.
        adapter.set_speaker_enabled(&ap(&kitchen), false);
        adapter.poll();
        assert!(messages(&adapter).is_empty());
    }

    #[test]
    fn settings_that_cant_be_saved_get_one_notice_until_a_save_works() {
        let dir = test_dir("adapter-settings-unsaved");
        let path = dir.join(settings::FILE_NAME);
        // A folder where the temporary file goes: every save fails.
        let blocked = crate::data_folder::temporary_path(&path);
        std::fs::create_dir(&blocked).expect("blocked");
        let mut adapter = launch(launch_audio(), &path);
        adapter.set_master_volume(64.0);
        assert!(adapter.poll(), "the notice shows");
        assert_eq!(messages(&adapter), [NoticeMessage::SettingsNotSaved]);
        // Told once: dismissed, it stays away while the saves keep failing.
        adapter.dismiss_all_notices();
        adapter.set_master_volume(65.0);
        adapter.poll();
        assert!(messages(&adapter).is_empty());
        // A save that works ends it; a failure after that tells again, and
        // the next save that works clears the notice.
        std::fs::remove_dir(&blocked).expect("unblocked");
        adapter.set_master_volume(66.0);
        adapter.poll();
        assert_eq!(SettingsFile::at(path.clone()).load().0.master_volume, 66.0);
        std::fs::create_dir(&blocked).expect("blocked");
        adapter.set_master_volume(67.0);
        adapter.poll();
        assert_eq!(messages(&adapter), [NoticeMessage::SettingsNotSaved]);
        std::fs::remove_dir(&blocked).expect("unblocked");
        adapter.set_master_volume(68.0);
        assert!(adapter.poll(), "the notice goes");
        assert!(messages(&adapter).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pairing_that_cant_be_saved_gets_a_notice_until_a_write_works() {
        use crate::audio::pairings::tests::sealer;
        let dir = test_dir("adapter-pairings-unsaved");
        let path = dir.join(crate::audio::pairings::FILE_NAME);
        // A folder where the file goes: nothing can be written there.
        std::fs::create_dir(&path).expect("the folder is made");
        let mut audio = launch_audio();
        let fake = FakeAirPlay::new();
        let (store, _) = PairingStore::open(path.clone(), sealer());
        audio.set_airplay_with_pairings(fake.link(), None, store);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        fake.appear(&tv);
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        let id = ap(&tv);
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&tv, PairingEvent::PinRequested);
        adapter.poll();
        adapter.submit_pin(&id, "4821");
        fake.pairing(
            &tv,
            PairingEvent::Paired(credentials("0123456789ABCDEF", 0x11)),
        );
        assert!(adapter.poll());
        assert_eq!(messages(&adapter), [NoticeMessage::PairingsNotSaved]);
        assert!(
            adapter.snapshot().speaker(&id).unwrap().enabled,
            "paired for this run all the same"
        );
        // Writable again: the next write clears the notice.
        std::fs::remove_dir(&path).expect("the folder is removed");
        adapter.forget_pairing(&id);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_unreadable_at_launch_get_a_notice_once_they_are_kept_aside() {
        let dir = test_dir("adapter-settings-kept-aside");
        let path = dir.join(settings::FILE_NAME);
        let held = dir.join("held");
        std::fs::write(&held, "the user's settings").expect("written");
        // Another program holds the file at launch (a folder stands in its
        // place), and lets go of it before the first save.
        std::fs::create_dir(&path).expect("the folder is made");
        let mut adapter = launch(launch_audio(), &path);
        assert!(messages(&adapter).is_empty(), "nothing kept aside yet");
        std::fs::remove_dir(&path).expect("the folder is removed");
        std::fs::rename(&held, &path).expect("moved back");
        adapter.set_master_volume(64.0);
        assert!(adapter.poll(), "the notice shows");
        let shown = messages(&adapter);
        let [NoticeMessage::SettingsKeptAside { file }] = shown.as_slice() else {
            panic!("{shown:?}");
        };
        assert!(file.starts_with("settings.ignored-"), "{file}");
        assert_eq!(
            std::fs::read_to_string(dir.join(file)).expect("kept"),
            "the user's settings"
        );
        // Told once: dismissed, it stays away at the next saves.
        adapter.dismiss_all_notices();
        adapter.set_master_volume(65.0);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_settings_file_gets_its_notice_at_launch_naming_the_copy() {
        let dir = test_dir("adapter-settings-damaged-at-launch");
        let path = dir.join(settings::FILE_NAME);
        std::fs::write(&path, "{ the user's settings, cut short").expect("written");
        let mut adapter = launch(launch_audio(), &path);
        // Told at launch, though the user changes nothing.
        let shown = messages(&adapter);
        let [NoticeMessage::SettingsKeptAside { file }] = shown.as_slice() else {
            panic!("{shown:?}");
        };
        assert!(file.starts_with("settings.ignored-"), "{file}");
        assert_eq!(
            std::fs::read_to_string(dir.join(file)).expect("kept"),
            "{ the user's settings, cut short"
        );
        // Told once: dismissed, it stays away, and the first save keeps no
        // other copy.
        adapter.dismiss_all_notices();
        adapter.poll();
        adapter.set_master_volume(64.0);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        assert_eq!(
            crate::data_folder::copies_kept_aside(&dir, "settings.ignored").len(),
            1
        );
        assert_eq!(SettingsFile::at(path).load().0.master_volume, 64.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_unreadable_at_launch_get_their_notice_once_they_can_be_kept_aside_unchanged() {
        use crate::data_folder::KEEP_ASIDE_RETRY_POLLS;
        let dir = test_dir("adapter-settings-kept-aside-unchanged");
        let path = dir.join(settings::FILE_NAME);
        let held = dir.join("held");
        std::fs::write(&held, "the user's settings").expect("written");
        // Another program holds the file at launch (a folder stands in its
        // place), and lets go of it; the user changes nothing.
        std::fs::create_dir(&path).expect("the folder is made");
        let mut adapter = launch(launch_audio(), &path);
        assert!(messages(&adapter).is_empty(), "nothing kept aside yet");
        std::fs::remove_dir(&path).expect("the folder is removed");
        std::fs::rename(&held, &path).expect("moved back");
        for _ in 0..KEEP_ASIDE_RETRY_POLLS {
            adapter.poll();
        }
        let shown = messages(&adapter);
        let [NoticeMessage::SettingsKeptAside { file }] = shown.as_slice() else {
            panic!("{shown:?}");
        };
        assert_eq!(
            std::fs::read_to_string(dir.join(file)).expect("kept"),
            "the user's settings"
        );
        // Nothing changed, so the file itself is left as it was; told once.
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the user's settings"
        );
        adapter.dismiss_all_notices();
        for _ in 0..2 * KEEP_ASIDE_RETRY_POLLS {
            adapter.poll();
        }
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The adapter over AirPlay with the pairings in `path`, and the fake
    /// sender behind it.
    fn launch_with_pairings(path: &std::path::Path) -> (AudioAdapter<MockBackend>, FakeAirPlay) {
        use crate::audio::pairings::tests::sealer;
        let mut audio = launch_audio();
        let fake = FakeAirPlay::new();
        let (store, _) = PairingStore::open(path.to_path_buf(), sealer());
        audio.set_airplay_with_pairings(fake.link(), None, store);
        let adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        (adapter, fake)
    }

    #[test]
    fn damaged_pairings_get_their_notice_at_launch_naming_the_copy() {
        let dir = test_dir("adapter-pairings-damaged-at-launch");
        let path = dir.join(crate::audio::pairings::FILE_NAME);
        std::fs::write(&path, b"no pairings in here").expect("written");
        let (mut adapter, _fake) = launch_with_pairings(&path);
        // Told at the first poll, with no pairing made.
        adapter.poll();
        let shown = messages(&adapter);
        let [NoticeMessage::PairingsKeptAside { file }] = shown.as_slice() else {
            panic!("{shown:?}");
        };
        assert!(file.starts_with("airplay-pairings.ignored-"), "{file}");
        assert_eq!(
            std::fs::read(dir.join(file)).expect("kept"),
            b"no pairings in here"
        );
        adapter.dismiss_all_notices();
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pairings_unreadable_at_launch_get_their_notice_once_they_can_be_kept_aside_unchanged() {
        use crate::data_folder::KEEP_ASIDE_RETRY_POLLS;
        let dir = test_dir("adapter-pairings-kept-aside-unchanged");
        let path = dir.join(crate::audio::pairings::FILE_NAME);
        // Another program holds the file at launch (a folder stands in its
        // place), and lets go of it; nothing is paired meanwhile.
        std::fs::create_dir(&path).expect("the folder is made");
        let (mut adapter, _fake) = launch_with_pairings(&path);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "nothing kept aside yet");
        std::fs::remove_dir(&path).expect("the folder is removed");
        std::fs::write(&path, b"the pairings made before").expect("written");
        for _ in 0..KEEP_ASIDE_RETRY_POLLS {
            adapter.poll();
        }
        let shown = messages(&adapter);
        let [NoticeMessage::PairingsKeptAside { file }] = shown.as_slice() else {
            panic!("{shown:?}");
        };
        assert_eq!(
            std::fs::read(dir.join(file)).expect("kept"),
            b"the pairings made before"
        );
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"the pairings made before"
        );
        adapter.dismiss_all_notices();
        for _ in 0..2 * KEEP_ASIDE_RETRY_POLLS {
            adapter.poll();
        }
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pairings_unreadable_at_launch_get_a_notice_once_they_are_kept_aside() {
        use crate::audio::pairings::tests::sealer;
        let dir = test_dir("adapter-pairings-kept-aside");
        let path = dir.join(crate::audio::pairings::FILE_NAME);
        // Another program holds the file at launch (a folder stands in its
        // place), and lets go of it before the first pairing.
        std::fs::create_dir(&path).expect("the folder is made");
        let mut audio = launch_audio();
        let fake = FakeAirPlay::new();
        let (store, _) = PairingStore::open(path.clone(), sealer());
        audio.set_airplay_with_pairings(fake.link(), None, store);
        std::fs::remove_dir(&path).expect("the folder is removed");
        std::fs::write(&path, b"the pairings made before").expect("written");
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        fake.appear(&tv);
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        let id = ap(&tv);
        adapter.set_speaker_enabled(&id, true);
        fake.pairing(&tv, PairingEvent::PinRequested);
        adapter.poll();
        adapter.submit_pin(&id, "4821");
        fake.pairing(
            &tv,
            PairingEvent::Paired(credentials("0123456789ABCDEF", 0x11)),
        );
        assert!(adapter.poll());
        let shown = messages(&adapter);
        let [NoticeMessage::PairingsKeptAside { file }] = shown.as_slice() else {
            panic!("{shown:?}");
        };
        assert!(file.starts_with("airplay-pairings.ignored-"), "{file}");
        assert_eq!(
            std::fs::read(dir.join(file)).expect("kept"),
            b"the pairings made before"
        );
        // Told once: dismissed, it stays away at the next writes.
        adapter.dismiss_all_notices();
        adapter.forget_pairing(&id);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The two outputs, and Kitchen and Living Room as Google Cast speakers.
    fn cast_adapter() -> (AudioAdapter<MockBackend>, FakeCast) {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        let fake = FakeCast::default();
        fake.announce(CAST_KITCHEN, "Kitchen", DEVICE_PORT);
        fake.announce(CAST_LIVING, "Living Room", DEVICE_PORT);
        audio.set_cast(fake.link());
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        adapter.poll();
        (adapter, fake)
    }

    /// The UI's ID of a Google Cast speaker.
    fn cc(id: &str) -> String {
        format!("cc:{id}")
    }

    /// The UI's ID of a Google Cast speaker added by address.
    fn cc_address(text: &str) -> String {
        let address = CastAddress::parse(text).expect("an address");
        cc(&address.device_id().to_string())
    }

    // covers: C18
    #[test]
    fn add_by_address_checks_the_address_then_lists_and_keeps_the_speaker() {
        let (mut adapter, _fake) = cast_adapter();
        adapter.set_cast_probe(|address| !address.host().ends_with(".invalid"));
        assert!(adapter.snapshot().cast_by_address);
        assert_eq!(
            adapter.add_cast_address("not an address"),
            Err(CastAddressError::Invalid)
        );
        assert_eq!(
            adapter.add_cast_address("speaker.invalid"),
            Err(CastAddressError::NotFound)
        );
        assert!(adapter.current_settings().cast_addresses.is_empty());

        assert_eq!(adapter.add_cast_address(" 192.0.2.20 "), Ok(()));
        let id = cc_address("192.0.2.20");
        let row = speaker_of(&adapter, &id);
        assert_eq!(row.name, "192.0.2.20");
        assert_eq!(row.kind, SpeakerKind::Cast);
        assert!(row.cast.as_ref().is_some_and(|cast| cast.added_by_address));
        let kitchen = speaker_of(&adapter, &cc(CAST_KITCHEN));
        assert!(
            !kitchen
                .cast
                .as_ref()
                .is_some_and(|cast| cast.added_by_address)
        );
        assert_eq!(adapter.current_settings().cast_addresses, ["192.0.2.20"]);

        adapter.remove_cast_address(&cc(CAST_KITCHEN));
        assert!(
            adapter.snapshot().speaker(&cc(CAST_KITCHEN)).is_some(),
            "not added"
        );
        adapter.remove_cast_address(&id);
        assert!(adapter.snapshot().speaker(&id).is_none());
        assert!(adapter.current_settings().cast_addresses.is_empty());
    }

    #[test]
    fn checking_an_address_leaves_the_wait_to_the_caller() {
        let (mut adapter, _fake) = cast_adapter();
        adapter.set_cast_probe(|_| panic!("probed on the calling thread"));
        let check = adapter.check_cast_address("192.0.2.20").expect("taken");
        assert_eq!(check.address.text(), "192.0.2.20");
        assert_eq!(
            adapter.add_checked_cast_address(check.address.clone(), false),
            Err(CastAddressError::NotFound)
        );
        assert!(
            adapter
                .snapshot()
                .speaker(&cc_address("192.0.2.20"))
                .is_none()
        );
        assert!(adapter.current_settings().cast_addresses.is_empty());
        assert_eq!(
            adapter.add_checked_cast_address(check.address, true),
            Ok(())
        );
        assert_eq!(adapter.current_settings().cast_addresses, ["192.0.2.20"]);
    }

    #[test]
    fn without_a_cast_sender_add_by_address_is_off() {
        let adapter = AudioAdapter::new(launch_audio(), || "09.41".to_owned(), DebugLog::default());
        assert!(!adapter.snapshot().cast_by_address);
    }

    // covers: C18
    #[test]
    fn speakers_added_by_address_come_back_at_the_next_launch() {
        let dir = test_dir("adapter-settings-cast-address");
        let path = dir.join(settings::FILE_NAME);
        let with_cast = || {
            let mut audio = launch_audio();
            audio.set_cast(FakeCast::default().link());
            audio
        };
        let mut adapter = launch(with_cast(), &path);
        adapter.set_cast_probe(|_| true);
        assert_eq!(adapter.add_cast_address("192.0.2.20:8010"), Ok(()));
        adapter.poll();

        let adapter = launch(with_cast(), &path);
        let row = speaker_of(&adapter, &cc_address("192.0.2.20:8010"));
        assert_eq!(row.name, "192.0.2.20:8010");
        assert!(row.cast.as_ref().is_some_and(|cast| cast.added_by_address));
    }

    fn cast_ended(attempt: u64, end: SessionEnd) -> HostEvent {
        HostEvent::Ended {
            attempt: AttemptId(attempt),
            device: cast_device(CAST_KITCHEN),
            end,
        }
    }

    fn cast_cant_play(speaker: &str, problem: UiCastProblem) -> NoticeMessage {
        NoticeMessage::CastCantPlay {
            speaker: speaker.to_owned(),
            problem,
        }
    }

    #[test]
    fn cast_speakers_are_connected_speakers_of_their_own_kind() {
        let (adapter, _) = cast_adapter();
        let state = adapter.snapshot();
        let row = state.speaker(&cc(CAST_KITCHEN)).expect("Kitchen is listed");
        assert_eq!(row.kind, SpeakerKind::Cast);
        assert_eq!(row.delay_ms, Some(0.0), "the offset the user sets");
        assert_eq!(
            row.cast.as_ref().map(|cast| cast.state),
            Some(CastState::Idle)
        );
        assert!(!row.enabled);
        let (this_pc, connected) = state.layout();
        assert!(this_pc.is_empty());
        let names: Vec<&str> = connected.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Headphones", "Kitchen", "Living Room", "Speakers"]);
        assert_eq!(state.cast_list, CastList::Quiet);
    }

    #[test]
    fn a_cast_rows_id_is_the_cores_cast_key() {
        let (adapter, _) = cast_adapter();
        let state = adapter.snapshot();
        let row = state.speaker(&cc(CAST_KITCHEN)).expect("Kitchen is listed");
        assert_eq!(
            SpeakerKey::parse(&row.id),
            Some(SpeakerKey::Cast(CAST_KITCHEN.to_owned()))
        );
        assert_eq!(cast_key(CAST_KITCHEN), row.id);
        assert_eq!(cast_id(&row.id), Some(CAST_KITCHEN));
        assert_eq!(cast_id(&speaker_key(CAST_KITCHEN)), None);
    }

    #[test]
    fn the_cast_list_says_it_searches_then_that_none_was_found() {
        let mut audio = MockBackend::empty();
        audio.connect_output(SPEAKERS, "Speakers");
        let fake = FakeCast::default();
        audio.set_cast(fake.link());
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        adapter.poll();
        assert_eq!(adapter.snapshot().cast_list, CastList::Searching);
        for _ in 0..NONE_FOUND_POLLS {
            adapter.poll();
        }
        assert_eq!(adapter.snapshot().cast_list, CastList::NoneFound);
        fake.announce(CAST_KITCHEN, "Kitchen", DEVICE_PORT);
        assert!(adapter.poll());
        assert_eq!(adapter.snapshot().cast_list, CastList::Quiet);
    }

    #[test]
    fn airplay_speakers_at_the_sender_delay_limit_say_they_play_ahead_of_the_cast_speaker() {
        let (mut adapter, _airplay, kitchen, _) = airplay_adapter();
        let cast = FakeCast::default();
        cast.announce(CAST_LIVING, "Living Room", DEVICE_PORT);
        adapter.audio.set_cast(cast.link());
        adapter.poll();
        adapter.set_speaker_enabled(&ap(&kitchen), true);
        adapter.set_speaker_enabled(&cc(CAST_LIVING), true);
        adapter.set_speaker_delay(&ap(&kitchen), 3250.0);
        adapter.set_speaker_delay(&cc(CAST_LIVING), 15_000.0);
        adapter.poll();
        assert!(
            messages(&adapter).is_empty(),
            "13.25 s more is within reach"
        );
        // L is 1.75 s: 20 s asks 18.25 s on top of Kitchen's own 3.25 s,
        // 1.5 s past the sender's 20 s.
        adapter.set_speaker_delay(&cc(CAST_LIVING), 20_000.0);
        adapter.poll();
        assert_eq!(
            messages(&adapter),
            [NoticeMessage::AirPlayAheadOfCast {
                speaker: "Living Room".to_owned(),
                tenths: 15,
            }]
        );
        adapter.poll();
        assert_eq!(messages(&adapter).len(), 1, "posted once");
        adapter.set_speaker_delay(&cc(CAST_LIVING), 4000.0);
        adapter.poll();
        assert!(messages(&adapter).is_empty(), "cleared once they keep up");
    }

    #[test]
    fn a_cast_speaker_alone_streams_and_shows_where_it_is() {
        let (mut adapter, fake) = cast_adapter();
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        let state = adapter.snapshot();
        assert!(state.running, "one Cast speaker is enough");
        let row = state.speaker(&cc(CAST_KITCHEN)).unwrap();
        assert!(row.enabled);
        assert_eq!(row.cast.as_ref().unwrap().state, CastState::Connecting);
        assert_eq!(fake.state().started, [cast_device(CAST_KITCHEN)]);
        assert!(adapter.audio.driver().capture_feeds_cast());

        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: cast_device(CAST_KITCHEN),
            event: SessionEvent::Playing,
        });
        assert!(adapter.poll());
        let row = adapter
            .snapshot()
            .speaker(&cc(CAST_KITCHEN))
            .cloned()
            .unwrap();
        assert_eq!(row.cast.unwrap().state, CastState::Playing);

        adapter.set_speaker_muted(&cc(CAST_KITCHEN), true);
        adapter.resync_speaker(&cc(CAST_KITCHEN));
        adapter.set_speaker_volume(&cc(CAST_KITCHEN), 25.0);
        assert_eq!(fake.state().mutes, [true]);
        assert_eq!(fake.state().resyncs, 1);
        assert_eq!(fake.state().volumes, [0.25]);

        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), false);
        assert!(!adapter.snapshot().running, "nothing is on anymore");
        assert_eq!(fake.state().stops, 1);
    }

    #[test]
    fn a_cast_group_is_one_row_that_plays_with_its_own_offset() {
        let (mut adapter, fake) = cast_adapter();
        fake.announce(CAST_ATTIC, "Attic", 32_123);
        adapter.poll();
        let group = |adapter: &AudioAdapter<MockBackend>| {
            let state = adapter.snapshot();
            let row = state
                .speaker(&cc(CAST_ATTIC))
                .expect("Attic is listed")
                .clone();
            assert_eq!(row.kind, SpeakerKind::Cast);
            assert_eq!(row.availability, Availability::Open);
            row
        };
        let members = |row: &Speaker| {
            row.cast
                .as_ref()
                .and_then(|cast| cast.group.as_ref())
                .expect("a group")
                .members
                .clone()
        };
        assert_eq!(members(&group(&adapter)), CastMembers::NotReported);
        assert!(
            adapter
                .snapshot()
                .speaker(&cc(CAST_KITCHEN))
                .is_some_and(|row| row.cast.as_ref().is_some_and(|cast| cast.group.is_none())),
            "a member stays a speaker of its own"
        );

        adapter.set_speaker_delay(&cc(CAST_ATTIC), 180.0);
        assert_eq!(
            fake.state().memory.offset(&cast_device(CAST_ATTIC)),
            Some(std::time::Duration::from_millis(180))
        );
        assert_eq!(fake.state().memory.offset(&cast_device(CAST_KITCHEN)), None);

        adapter.set_speaker_enabled(&cc(CAST_ATTIC), true);
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));
        assert!(group(&adapter).enabled);
        assert_eq!(fake.state().started, [cast_device(CAST_ATTIC)]);

        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: cast_device(CAST_ATTIC),
            event: SessionEvent::Members(vec![GroupMember {
                device_id: CAST_KITCHEN.to_owned(),
                id: Some(cast_device(CAST_KITCHEN)),
                name: None,
            }]),
        });
        adapter.poll();
        assert_eq!(
            members(&group(&adapter)),
            CastMembers::Known(vec!["Kitchen".to_owned()])
        );
        adapter.set_speaker_volume(&cc(CAST_ATTIC), 50.0);
        assert_eq!(fake.state().volumes, [0.5]);
    }

    #[test]
    fn a_second_cast_speaker_is_refused_with_a_notice() {
        let (mut adapter, _fake) = cast_adapter();
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        adapter.set_speaker_enabled(&cc(CAST_LIVING), true);
        assert!(messages(&adapter).contains(&cast_cant_play("Living Room", UiCastProblem::Busy)));
        let state = adapter.snapshot();
        assert!(!state.speaker(&cc(CAST_LIVING)).unwrap().enabled);
        assert!(state.speaker(&cc(CAST_KITCHEN)).unwrap().enabled);
        assert!(state.running);
    }

    fn zone(members: Vec<String>) -> SavedZone {
        SavedZone {
            id: "zone:1".to_owned(),
            name: "Downstairs".to_owned(),
            members,
            volume: 80.0,
        }
    }

    #[test]
    fn a_zone_volume_change_reaches_the_playing_members_as_a_master_change_does() {
        let (mut adapter, fake) = cast_adapter();
        adapter.restore(Settings {
            speaker_zones: vec![zone(vec![key(SPEAKERS), cc(CAST_KITCHEN)])],
            ..Settings::default()
        });
        adapter.poll();
        adapter.set_speaker_enabled(&key(SPEAKERS), true);
        adapter.set_speaker_enabled(&key(HEADPHONES), true);
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: cast_device(CAST_KITCHEN),
            event: SessionEvent::Playing,
        });
        adapter.poll();
        adapter.set_speaker_volume(&key(SPEAKERS), 30.0);
        adapter.set_speaker_volume(&cc(CAST_KITCHEN), 50.0);
        adapter.set_master_volume(50.0);
        let gain = |adapter: &AudioAdapter<MockBackend>, id| {
            adapter.audio.playing_params(id).expect("playing").gain
        };
        assert_eq!(
            gain(&adapter, SPEAKERS),
            zoned_output_gain(30.0, 80.0, 50.0) as f32
        );
        assert_eq!(
            gain(&adapter, HEADPHONES),
            output_gain(DEFAULT_LOCAL_POSITION, 50.0) as f32,
            "in no zone: the gain of before"
        );
        // Master × zone × own: 1 × 0.8 × 0.5, then the master at 0.5.
        assert_eq!(fake.state().volumes, [0.4, 0.2]);

        adapter.set_zone_volume("zone:1", 25.0);
        assert_eq!(
            gain(&adapter, SPEAKERS),
            zoned_output_gain(30.0, 25.0, 50.0) as f32
        );
        assert_eq!(fake.state().volumes, [0.4, 0.2, 0.0625]);
        assert_eq!(adapter.snapshot().zones[0].volume, 25.0);
        let state = adapter.snapshot();
        assert_eq!(
            state.speaker(&key(SPEAKERS)).unwrap().volume,
            30.0,
            "own stays"
        );
        assert_eq!(state.master_volume, 50.0, "the master stays");

        adapter.set_zone_volume("zone:9", 10.0);
        assert_eq!(fake.state().volumes.len(), 3, "no such zone: nothing");
    }

    // covers: S246
    #[test]
    fn a_zone_click_turns_on_its_present_members_and_one_cast_speaker() {
        let (mut adapter, _fake) = cast_adapter();
        adapter.restore(Settings {
            speaker_zones: vec![zone(vec![
                key(SPEAKERS),
                cc(CAST_KITCHEN),
                cc(CAST_LIVING),
                key("gone"),
            ])],
            ..Settings::default()
        });
        adapter.poll();
        adapter.toggle_zone("zone:1");
        let state = adapter.snapshot();
        assert!(state.running);
        assert!(enabled(&adapter, SPEAKERS) && !enabled(&adapter, HEADPHONES));
        assert!(state.speaker(&cc(CAST_KITCHEN)).unwrap().enabled);
        assert!(
            !state.speaker(&cc(CAST_LIVING)).unwrap().enabled,
            "one Cast speaker at a time"
        );
        assert!(
            !messages(&adapter).contains(&cast_cant_play("Living Room", UiCastProblem::Busy)),
            "the click never tries the second one"
        );

        adapter.toggle_zone("zone:1");
        let state = adapter.snapshot();
        assert!(!state.running, "its speakers were the last ones on");
        assert!(!enabled(&adapter, SPEAKERS));
        assert!(!state.speaker(&cc(CAST_KITCHEN)).unwrap().enabled);
        adapter.toggle_zone("zone:9");
        assert!(!adapter.snapshot().running, "no such zone: nothing");
    }

    // covers: S246
    #[test]
    fn a_zone_click_leaves_a_cast_speaker_outside_the_zone_on() {
        let (mut adapter, _fake) = cast_adapter();
        adapter.restore(Settings {
            speaker_zones: vec![zone(vec![key(SPEAKERS), cc(CAST_KITCHEN)])],
            ..Settings::default()
        });
        adapter.poll();
        adapter.set_speaker_enabled(&cc(CAST_LIVING), true);
        adapter.toggle_zone("zone:1");
        let living =
            |adapter: &AudioAdapter<MockBackend>| speaker_of(adapter, &cc(CAST_LIVING)).enabled;
        assert!(enabled(&adapter, SPEAKERS) && living(&adapter));
        assert!(!speaker_of(&adapter, &cc(CAST_KITCHEN)).enabled);
        // The Cast member left off counts neither way: the next click is off.
        adapter.toggle_zone("zone:1");
        assert!(!enabled(&adapter, SPEAKERS) && living(&adapter));
        assert!(adapter.snapshot().running);
    }

    // covers: S248
    #[test]
    fn the_groups_become_zones_once_and_are_saved_with_the_mark() {
        let mut adapter = adapter();
        adapter.restore(Settings {
            groups: vec![
                SavedGroup {
                    id: "group:1".to_owned(),
                    name: "Desk".to_owned(),
                    members: vec![key(SPEAKERS), key(HEADPHONES)],
                },
                SavedGroup {
                    id: "group:2".to_owned(),
                    name: "Group 2".to_owned(),
                    members: vec![key(HEADPHONES)],
                },
            ],
            speaker_zones_migrated: false,
            ..Settings::default()
        });
        let members: Vec<(String, Vec<String>)> = adapter
            .snapshot()
            .zones
            .into_iter()
            .map(|zone| (zone.id, zone.members))
            .collect();
        assert_eq!(
            members,
            [
                ("group:1".to_owned(), vec![key(SPEAKERS), key(HEADPHONES)]),
                ("group:2".to_owned(), Vec::new()),
            ]
        );
        let saved = adapter.current_settings();
        assert!(saved.speaker_zones_migrated);
        assert_eq!(saved.groups.len(), 2, "the groups stay");
        assert_eq!(
            saved.speaker_zones[1],
            SavedZone {
                id: "group:2".to_owned(),
                name: "Group 2".to_owned(),
                members: Vec::new(),
                volume: 100.0,
            }
        );
        // A later launch keeps the zones as saved, whatever the groups are.
        let mut later = adapter.current_settings();
        later.groups.clear();
        adapter.restore(later);
        assert_eq!(adapter.snapshot().zones.len(), 2);
    }

    fn saved_preset(members: &[(String, Option<f64>)]) -> SavedPreset {
        SavedPreset {
            id: "preset:1".to_owned(),
            name: "Evening".to_owned(),
            members: members
                .iter()
                .map(|(key, volume)| settings::SavedPresetMember {
                    key: key.clone(),
                    volume: *volume,
                })
                .collect(),
            zones: vec![settings::SavedPresetZone {
                id: "zone:1".to_owned(),
                volume: 45.0,
            }],
            master_volume: 70.0,
            gains_db: [2.0; audiosync_core::eq::BAND_COUNT],
        }
    }

    // covers: S252
    #[test]
    fn recall_leaves_delays_offsets_and_zone_members_untouched() {
        let mut audio = MockBackend::empty();
        audio.connect_output_as("builtin", "Built-in", OutputKind::BuiltIn);
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        adapter.restore(Settings {
            speaker_zones: vec![zone(vec![key(SPEAKERS)])],
            speaker_presets: vec![saved_preset(&[
                (key(HEADPHONES), Some(40.0)),
                (key("gone"), Some(10.0)),
            ])],
            auto_connect: BTreeSet::from([key(SPEAKERS)]),
            ..Settings::default()
        });
        adapter.poll();
        turn_on(&mut adapter, "builtin");
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_speaker_delay(&key(SPEAKERS), 120.0);
        adapter.set_speaker_delay(&key(HEADPHONES), -40.0);
        let delays = |adapter: &AudioAdapter<MockBackend>| -> Vec<(String, Option<f64>, bool)> {
            adapter
                .snapshot()
                .speakers
                .into_iter()
                .map(|speaker| (speaker.id, speaker.delay_ms, speaker.auto_connect))
                .collect()
        };
        let before = delays(&adapter);
        let zones_before = adapter.snapshot().zones;

        let left = adapter
            .recall_preset("preset:1")
            .expect("the preset exists");
        assert_eq!(left, [("gone".to_owned(), LeftOff::NotFound)]);
        assert!(enabled(&adapter, HEADPHONES) && !enabled(&adapter, SPEAKERS));
        assert!(
            enabled(&adapter, "builtin"),
            "the reference output stays on"
        );
        assert!(adapter.snapshot().running);
        assert_eq!(delays(&adapter), before);
        let zones_after = adapter.snapshot().zones;
        assert_eq!(zones_after[0].members, zones_before[0].members);
        assert_eq!(zones_after[0].volume, 45.0);
        assert_eq!(speaker_of(&adapter, &key(HEADPHONES)).volume, 40.0);
        assert_eq!(adapter.snapshot().master_volume, 70.0);
        assert_eq!(adapter.recall_preset("preset:9"), None);
    }

    // covers: S252
    #[test]
    fn a_recalled_zone_volume_reaches_the_playing_members() {
        let (mut adapter, fake) = cast_adapter();
        adapter.restore(Settings {
            speaker_zones: vec![zone(vec![key(SPEAKERS), cc(CAST_KITCHEN)])],
            speaker_presets: vec![saved_preset(&[
                (key(SPEAKERS), Some(30.0)),
                (cc(CAST_KITCHEN), Some(50.0)),
            ])],
            ..Settings::default()
        });
        adapter.poll();
        adapter.set_speaker_enabled(&key(SPEAKERS), true);
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: cast_device(CAST_KITCHEN),
            event: SessionEvent::Playing,
        });
        adapter.poll();
        adapter.set_speaker_volume(&key(SPEAKERS), 30.0);
        adapter.set_speaker_volume(&cc(CAST_KITCHEN), 50.0);

        let left = adapter
            .recall_preset("preset:1")
            .expect("the preset exists");
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(
            adapter
                .audio
                .playing_params(SPEAKERS)
                .expect("playing")
                .gain,
            zoned_output_gain(30.0, 45.0, 70.0) as f32
        );
        // Master × zone × own: 0.7 × 0.45 × 0.5.
        let cast = *fake.state().volumes.last().expect("a Cast volume");
        assert!((cast - 0.1575).abs() < 1e-9, "{cast}");
        assert_eq!(adapter.current_preset().as_deref(), Some("preset:1"));
    }

    // covers: S252, S253
    #[test]
    fn a_recalled_preset_is_current() {
        let (mut adapter, _fake) = cast_adapter();
        adapter.restore(Settings {
            speaker_zones: vec![zone(vec![key(SPEAKERS)])],
            speaker_presets: vec![saved_preset(&[
                (key(SPEAKERS), Some(35.0)),
                (cc(CAST_KITCHEN), None),
            ])],
            equalizer: SavedEqualizer {
                enabled: true,
                preamp_db: -3.0,
                gains_db: vec![0.0; audiosync_core::eq::BAND_COUNT],
            },
            ..Settings::default()
        });
        adapter.poll();
        turn_on(&mut adapter, HEADPHONES);
        adapter.set_speaker_enabled(&cc(CAST_LIVING), true);
        assert_eq!(adapter.current_preset(), None);

        let left = adapter
            .recall_preset("preset:1")
            .expect("the preset exists");
        assert!(left.is_empty(), "{left:?}");
        let state = adapter.snapshot();
        assert!(state.running);
        assert!(enabled(&adapter, SPEAKERS) && !enabled(&adapter, HEADPHONES));
        assert!(state.speaker(&cc(CAST_KITCHEN)).unwrap().enabled);
        assert!(
            !state.speaker(&cc(CAST_LIVING)).unwrap().enabled,
            "a Cast speaker outside the preset goes off"
        );
        assert_eq!(
            (
                state.equalizer.enabled,
                state.equalizer.preamp_db,
                state.equalizer.gains_db
            ),
            (true, -3.0, [2.0; audiosync_core::eq::BAND_COUNT]),
            "the preamp and the switch stay"
        );
        assert_eq!(adapter.current_preset().as_deref(), Some("preset:1"));

        adapter.set_equalizer(EqSettings {
            enabled: false,
            preamp_db: 4.0,
            gains_db: [2.0; audiosync_core::eq::BAND_COUNT],
        });
        assert_eq!(
            adapter.current_preset().as_deref(),
            Some("preset:1"),
            "the preamp and the switch never break the match"
        );
        adapter.set_speaker_volume(&key(SPEAKERS), 36.0);
        assert_eq!(adapter.current_preset(), None, "a slider breaks it");
        adapter.set_speaker_volume(&key(SPEAKERS), 35.0);
        assert_eq!(adapter.current_preset().as_deref(), Some("preset:1"));
        turn_on(&mut adapter, HEADPHONES);
        assert_eq!(adapter.current_preset(), None, "a switch breaks it");
    }

    fn preset_notices(adapter: &AudioAdapter<MockBackend>) -> Vec<NoticeMessage> {
        messages(adapter)
            .into_iter()
            .filter(|message| matches!(message, NoticeMessage::PresetLeftOff { .. }))
            .collect()
    }

    fn left_off_notice(preset: &str, left_off: &[(&str, PresetLeftOff)]) -> NoticeMessage {
        NoticeMessage::PresetLeftOff {
            preset: preset.to_owned(),
            left_off: left_off
                .iter()
                .map(|(name, reason)| ((*name).to_owned(), reason.clone()))
                .collect(),
            nothing_changed: false,
        }
    }

    /// Built-in, Speakers and Headphones, with the example zone over
    /// Speakers and `presets`.
    fn preset_adapter(presets: Vec<SavedPreset>) -> AudioAdapter<MockBackend> {
        let mut audio = MockBackend::empty();
        audio.connect_output_as("builtin", "Built-in", OutputKind::BuiltIn);
        audio.connect_output(SPEAKERS, "Speakers");
        audio.connect_output(HEADPHONES, "Headphones");
        let mut adapter = AudioAdapter::new(audio, || "09.41".to_owned(), DebugLog::default());
        adapter.restore(Settings {
            speaker_zones: vec![zone(vec![key(SPEAKERS)])],
            speaker_presets: presets,
            ..Settings::default()
        });
        adapter.poll();
        turn_on(&mut adapter, "builtin");
        adapter
    }

    // covers: S265
    #[test]
    fn the_preset_notice_follows_its_preset_and_goes_once_it_plays_in_full() {
        let mut adapter = preset_adapter(vec![saved_preset(&[
            (key(HEADPHONES), Some(40.0)),
            (key("late"), None),
        ])]);
        adapter.recall_preset("preset:1");
        assert_eq!(
            preset_notices(&adapter),
            [left_off_notice(
                "Evening",
                &[("late", PresetLeftOff::NotFound)]
            )]
        );

        Backend::rename_preset(&mut adapter, "preset:1", "Night");
        assert_eq!(
            preset_notices(&adapter),
            [left_off_notice(
                "Night",
                &[("late", PresetLeftOff::NotFound)]
            )],
            "the notice follows the new name in place"
        );

        adapter.audio.connect_output("late", "Late");
        adapter.poll();
        turn_on(&mut adapter, "late");
        adapter.poll();
        assert_eq!(adapter.current_preset().as_deref(), Some("preset:1"));
        assert!(
            preset_notices(&adapter).is_empty(),
            "the preset plays in full, so the notice goes"
        );
    }

    // covers: S262, S265
    #[test]
    fn deleting_the_preset_takes_its_notice_down_and_a_full_recall_clears_it() {
        let mut full = saved_preset(&[(key(SPEAKERS), None)]);
        full.id = "preset:2".to_owned();
        full.name = "Day".to_owned();
        let mut adapter = preset_adapter(vec![
            saved_preset(&[(key(HEADPHONES), None), (key("gone"), None)]),
            full,
        ]);
        adapter.recall_preset("preset:1");
        assert_eq!(preset_notices(&adapter).len(), 1);
        adapter.recall_preset("preset:2");
        assert!(
            preset_notices(&adapter).is_empty(),
            "a full recall clears it"
        );

        adapter.recall_preset("preset:1");
        assert_eq!(preset_notices(&adapter).len(), 1);
        Backend::delete_preset(&mut adapter, "preset:2");
        assert_eq!(preset_notices(&adapter).len(), 1, "another preset's delete");
        Backend::delete_preset(&mut adapter, "preset:1");
        assert!(preset_notices(&adapter).is_empty());
    }

    // covers: S251, S262
    #[test]
    fn save_and_update_capture_what_plays_and_need_a_speaker_on() {
        let mut adapter = preset_adapter(Vec::new());
        assert_eq!(
            Backend::save_preset(&mut adapter, "Evening"),
            Err(PresetError::NoSpeakerOn),
            "the reference output alone is no preset"
        );
        turn_on(&mut adapter, HEADPHONES);
        Backend::save_preset(&mut adapter, "Evening").expect("saved");
        let state = adapter.snapshot();
        assert_eq!(state.presets.len(), 1);
        assert_eq!(state.current_preset.as_deref(), Some("preset:1"));
        assert_eq!(adapter.presets[0].members.len(), 1, "Headphones only");

        turn_on(&mut adapter, SPEAKERS);
        assert_eq!(adapter.current_preset(), None);
        Backend::update_preset(&mut adapter, "preset:1").expect("updated");
        assert_eq!(adapter.presets[0].id, "preset:1");
        assert_eq!(adapter.presets[0].name, "Evening");
        assert_eq!(adapter.presets[0].members.len(), 2);
        assert_eq!(adapter.current_preset().as_deref(), Some("preset:1"));
    }

    // covers: S251, S265
    #[test]
    fn a_hidden_speaker_is_never_saved_and_a_recall_names_it_hidden() {
        let mut adapter = preset_adapter(vec![saved_preset(&[
            (key(SPEAKERS), None),
            (key(HEADPHONES), None),
        ])]);
        adapter.hide_speaker(&key(HEADPHONES), "Headphones");
        adapter.recall_preset("preset:1");
        assert_eq!(
            preset_notices(&adapter),
            [left_off_notice(
                "Evening",
                &[("Headphones", PresetLeftOff::Hidden)]
            )]
        );
        assert!(!enabled(&adapter, HEADPHONES), "it stays hidden and off");

        Backend::save_preset(&mut adapter, "Night").expect("saved");
        let night = adapter.presets.last().expect("the new preset");
        assert!(
            night
                .members
                .iter()
                .all(|member| member.key != key(HEADPHONES)),
            "{night:?}"
        );
    }

    // covers: S265
    #[test]
    fn a_member_another_app_casts_to_stays_off_as_in_use() {
        let (mut adapter, fake) = cast_adapter();
        adapter.restore(Settings {
            speaker_presets: vec![saved_preset(&[
                (key(SPEAKERS), None),
                (cc(CAST_KITCHEN), None),
            ])],
            ..Settings::default()
        });
        adapter.poll();
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        fake.report(cast_ended(1, SessionEnd::TakenOver));
        adapter.poll();

        adapter.recall_preset("preset:1");
        assert_eq!(
            preset_notices(&adapter),
            [left_off_notice(
                "Evening",
                &[("Kitchen", PresetLeftOff::Busy)]
            )]
        );
    }

    // covers: S265
    #[test]
    fn a_member_its_stereo_pair_holds_stays_off_and_names_the_pair() {
        use crate::audio::airplay::tests::stereo_pair;

        let (mut adapter, fake, _) = airplay2_adapter(true);
        let (pair, left, right) = stereo_pair();
        fake.appear_in(&left, &pair);
        fake.appear_in(&right, &pair);
        fake.target(&pair);
        adapter.poll();
        adapter.restore(Settings {
            speaker_presets: vec![saved_preset(&[(key(SPEAKERS), None), (ap(&left), None)])],
            ..Settings::default()
        });
        adapter.poll();

        adapter.recall_preset("preset:1");
        assert_eq!(
            preset_notices(&adapter),
            [left_off_notice(
                "Evening",
                &[("Left", PresetLeftOff::InTarget("Living Room".to_owned()))]
            )]
        );
    }

    // covers: S265, S266
    #[test]
    fn a_member_that_fails_to_start_after_a_recall_goes_into_the_preset_notice() {
        let (mut adapter, fake) = cast_adapter();
        adapter.restore(Settings {
            speaker_presets: vec![saved_preset(&[
                (key(SPEAKERS), None),
                (cc(CAST_KITCHEN), None),
            ])],
            ..Settings::default()
        });
        adapter.poll();
        adapter.recall_preset("preset:1");
        assert!(messages(&adapter).is_empty(), "{:?}", messages(&adapter));

        fake.report(cast_ended(
            1,
            SessionEnd::Failed(CastSessionError::Connect(ConnectError::NoAddress)),
        ));
        adapter.poll();
        assert_eq!(
            messages(&adapter),
            [left_off_notice(
                "Evening",
                &[("Kitchen", PresetLeftOff::CantPlay)]
            )],
            "one notice, and no Cast notice of its own"
        );

        // Switched on by the user, it is no longer the recall's: the preset
        // notice goes, and a new failure has its own notice.
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        adapter.poll();
        assert!(preset_notices(&adapter).is_empty());
        fake.report(cast_ended(
            2,
            SessionEnd::Failed(CastSessionError::Connect(ConnectError::NoAddress)),
        ));
        adapter.poll();
        assert!(
            messages(&adapter).contains(&cast_cant_play("Kitchen", UiCastProblem::NotResponding))
        );
    }

    // covers: S251
    #[test]
    fn save_as_preset_stops_at_the_most_presets_a_settings_file_keeps() {
        let full: Vec<SavedPreset> = (1..=crate::settings::MAX_ENTRIES)
            .map(|n| {
                let mut preset = saved_preset(&[(key(SPEAKERS), None)]);
                preset.id = format!("preset:{n}");
                preset
            })
            .collect();
        let mut adapter = preset_adapter(full);
        turn_on(&mut adapter, SPEAKERS);
        assert_eq!(adapter.presets.len(), ui_model::MAX_PRESETS);
        assert_eq!(
            Backend::save_preset(&mut adapter, "One more"),
            Err(PresetError::Full)
        );
        assert_eq!(adapter.presets.len(), ui_model::MAX_PRESETS);

        Backend::delete_preset(&mut adapter, "preset:3");
        Backend::save_preset(&mut adapter, "One more").expect("room again");
    }

    #[test]
    fn a_renamed_cast_speaker_shows_and_is_named_by_its_new_name() {
        let (mut adapter, _fake) = cast_adapter();
        adapter.rename_speaker(&cc(CAST_LIVING), "Den");
        let row = speaker_of(&adapter, &cc(CAST_LIVING));
        assert_eq!(row.name, "Den");
        assert_eq!(row.system_name, "Living Room");
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        adapter.set_speaker_enabled(&cc(CAST_LIVING), true);
        assert!(messages(&adapter).contains(&cast_cant_play("Den", UiCastProblem::Busy)));
        adapter.rename_speaker(&cc(CAST_LIVING), " ");
        assert_eq!(speaker_of(&adapter, &cc(CAST_LIVING)).name, "Living Room");
    }

    #[test]
    fn quitting_keeps_a_cast_offset_set_since_the_last_poll() {
        let (mut adapter, fake) = cast_adapter();
        let file = test_dir("cast-memory-quit").join("cast-speakers.json");
        adapter.set_cast_file(Some(file.clone()));
        adapter.save_settings();
        assert!(!file.exists(), "nothing changed, nothing is written");

        adapter.set_speaker_delay(&cc(CAST_KITCHEN), 250.0);
        adapter.save_settings();
        let saved = std::fs::read_to_string(&file).expect("the file is written");
        assert_eq!(DeviceMemory::from_json(&saved), fake.state().memory);
        assert_eq!(
            DeviceMemory::from_json(&saved).offset(&cast_device(CAST_KITCHEN)),
            Some(std::time::Duration::from_millis(250))
        );
    }

    #[test]
    fn a_refused_cast_speakers_notice_stays_while_the_other_starts_playing() {
        let (mut adapter, fake) = cast_adapter();
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        adapter.set_speaker_enabled(&cc(CAST_LIVING), true);
        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: cast_device(CAST_KITCHEN),
            event: SessionEvent::Playing,
        });
        adapter.poll();
        assert_eq!(
            adapter
                .snapshot()
                .speaker(&cc(CAST_KITCHEN))
                .unwrap()
                .cast
                .as_ref()
                .unwrap()
                .state,
            CastState::Playing
        );
        assert_eq!(
            messages(&adapter),
            [cast_cant_play("Living Room", UiCastProblem::Busy)]
        );
    }

    #[test]
    fn a_cast_session_that_ends_badly_turns_its_speaker_off_with_a_notice() {
        let (mut adapter, fake) = cast_adapter();
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        fake.report(cast_ended(
            1,
            SessionEnd::Failed(CastSessionError::Connect(ConnectError::NoAddress)),
        ));
        adapter.poll();
        assert!(
            messages(&adapter).contains(&cast_cant_play("Kitchen", UiCastProblem::NotResponding))
        );
        let state = adapter.snapshot();
        assert!(!state.speaker(&cc(CAST_KITCHEN)).unwrap().enabled);
        assert!(!state.running);

        // Turned on again, the notice goes.
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        assert!(messages(&adapter).is_empty());
    }

    #[test]
    fn a_late_end_of_an_older_cast_attempt_changes_nothing() {
        let (mut adapter, fake) = cast_adapter();
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), false);
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        assert_eq!(fake.state().started.len(), 2);
        fake.report(cast_ended(1, SessionEnd::TakenOver));
        adapter.poll();
        assert!(messages(&adapter).is_empty());
        assert!(
            adapter
                .snapshot()
                .speaker(&cc(CAST_KITCHEN))
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn a_cast_delay_is_its_offset_that_local_outputs_wait_for_and_is_saved() {
        let (mut adapter, fake) = cast_adapter();
        let file = test_dir("cast-memory").join("cast-speakers.json");
        adapter.set_cast_file(Some(file.clone()));
        turn_on(&mut adapter, SPEAKERS);
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), true);
        adapter.set_speaker_delay(&cc(CAST_KITCHEN), 400.0);
        assert_eq!(
            fake.state().memory.offset(&cast_device(CAST_KITCHEN)),
            Some(std::time::Duration::from_millis(400))
        );
        assert_eq!(
            adapter
                .snapshot()
                .speaker(&cc(CAST_KITCHEN))
                .unwrap()
                .delay_ms,
            Some(400.0)
        );
        assert_eq!(local_delay(&adapter, SPEAKERS), 19_200, "400 ms");
        adapter.poll();
        let saved = std::fs::read_to_string(&file).expect("the file is written");
        assert_eq!(DeviceMemory::from_json(&saved), fake.state().memory);

        adapter.set_speaker_delay(&cc(CAST_KITCHEN), -300.0);
        assert_eq!(
            adapter
                .snapshot()
                .speaker(&cc(CAST_KITCHEN))
                .unwrap()
                .delay_ms,
            Some(0.0),
            "an offset only holds the others back"
        );
        adapter.set_speaker_enabled(&cc(CAST_KITCHEN), false);
        assert_eq!(local_delay(&adapter, SPEAKERS), 0);
    }
}
