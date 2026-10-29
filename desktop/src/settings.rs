//! The user's choices, kept across restarts.
//!
//! One small JSON file in the app's data folder,
//! `%LOCALAPPDATA%\AudioSync\settings.json`, next to the AirPlay pairings:
//! the source (the whole system, an app by its executable's file name,
//! since its process ID changes every time it starts, or an input device by
//! its endpoint ID), each output's and
//! AirPlay speaker's switch, volume and delay (outputs by endpoint ID,
//! AirPlay speakers by the ID they announce), the AirPlay delay adjustment,
//! the master volume, the
//! auto-connect marks, the names the user gave speakers, the groups, the
//! zones, the speaker presets, the equalizer, View > Show Virtual Devices, View > Appearance, the main
//! window's place and size, and File > Web Remote with its port.
//!
//! The file carries a format version ([`VERSION`]). A file from an older
//! version is read for the fields it has; one from a newer version, one that
//! can't be read or one that isn't valid JSON is ignored as a whole: the
//! defaults apply, and the next change writes a fresh file, after keeping
//! the old one aside in a copy with the time in its name, which no later
//! copy replaces (a newer version's named after [`NEWER_FILE_NAME`], any
//! other after [`IGNORED_FILE_NAME`]). Any other is kept aside at launch
//! already when it can be read then, so the user is told at once where it
//! is ([`SettingsFile::keep_ignored_aside`]). A file that can't be read is
//! tried once more at launch (another program may hold it for a moment),
//! then every few seconds, and while it still can't be read nothing is
//! saved over it, since it may hold the user's settings; once it reads as
//! settings, they apply as if read at launch. Fields this version doesn't know are ignored,
//! missing ones take their defaults, and values out of range are clamped
//! ([`Settings::clamped`]). Nothing here panics on a bad file, and the
//! caller logs what [`Loaded`] says.
//!
//! The file is replaced as a whole through a temporary file
//! (`crate::data_folder::write_atomically`), so a crash while saving leaves
//! the old file or the new one.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::speaker_names::SpeakerNames;
use audiosync_core::eq::{BAND_COUNT, EqSettings, UserCurve, sanitized_curves};
use audiosync_core::policy::delay::{
    MAX_DELAY_MS, MAX_OFFSET_MS, MIN_OFFSET_MS, sanitize_airplay_adjustment_ms,
};
use audiosync_core::policy::presets::{self, Preset, PresetMember, PresetZone};
use audiosync_core::policy::source_quit::WhenSourceQuits;
use audiosync_core::policy::speakers::SpeakerKey;
use audiosync_core::policy::zones::{self, DEFAULT_ZONE_VOLUME, Zone, Zones};
use audiosync_core::volume::{
    DEFAULT_LOCAL_POSITION, DEFAULT_MASTER_POSITION, sanitize_own, sanitize_position,
};
use serde::{Deserialize, Serialize};

use crate::audio::cast_address;
use crate::data_folder;
use crate::remote::RemoteSettings;
use crate::ui_model::{UpdateRecord, WindowFrame};

/// The file's name in the app's data folder.
#[cfg(any(os_backend, test))]
pub const FILE_NAME: &str = "settings.json";

/// What the copy of a newer AudioSync's settings file is named after
/// (`settings.newer-<time>.json`), kept before this build first saves over
/// it.
pub const NEWER_FILE_NAME: &str = "settings.newer.json";

/// What the copy of a settings file this build ignored for another reason
/// (it couldn't be read at launch, or it was damaged) is named after
/// (`settings.ignored-<time>.json`), kept before this build first saves
/// over it.
pub const IGNORED_FILE_NAME: &str = "settings.ignored.json";

/// The format version this build writes, and the newest it reads.
///
/// Version 2 (the local delay model, spec S180): an output's `delay_ms` is
/// its offset relative to the reference output, −2000 to +20 000 ms, and
/// `airplay_adjustment_ms` is the AirPlay delay adjustment. In version 1 an
/// output's `delay_ms` was its own trim, 0 to 20 000 ms, and
/// [`migrate_delays`] turns it into an offset once.
///
/// Version 3 (zones, spec S248) adds `speaker_zones` and
/// `speaker_zones_migrated`. A version 2 file reads as version 3, its groups
/// becoming zones once ([`Settings::with_zones_migrated`]); the groups stay.
/// It also keeps `speaker_presets` and `speaker_presets_migrated` (spec
/// S251, S254): a file without the mark has its groups that lost a member
/// to the zones become presets once ([`Settings::with_presets_migrated`]).
pub const VERSION: u32 = 3;

/// The first format version with offsets relative to the reference.
pub const OFFSETS_VERSION: u32 = 2;

/// The first format version with zones; an older file's groups become zones
/// as it is read.
pub const ZONES_VERSION: u32 = 3;

/// The longest own delay an AirPlay speaker takes, in milliseconds: the
/// sender's limit (P99), the core's delay range.
pub const MAX_AIRPLAY_DELAY_MS: f64 = MAX_DELAY_MS;

/// The most entries kept in each list or map: far more than anyone has
/// devices, small enough that a damaged file can't make the app slow.
pub const MAX_ENTRIES: usize = 256;

/// The longest ID or name kept, in bytes; an entry with a longer ID is
/// dropped, a longer name is cut.
pub const MAX_TEXT: usize = 1024;

/// The main window's smallest size in logical pixels, as `ui/main.slint`
/// sets it.
pub const MIN_WINDOW: (f64, f64) = (860.0, 440.0);

/// The main window's largest size kept, in logical pixels.
pub const MAX_WINDOW: (f64, f64) = (16_384.0, 16_384.0);

/// The farthest a saved window position may be from the origin, in physical
/// pixels: Windows' own coordinate range.
pub const MAX_POSITION: i32 = 32_000;

/// View > Appearance: every window light or dark, or as Windows is set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// A value another build wrote, or none, follows Windows instead of making
/// the whole file unreadable.
impl<'de> Deserialize<'de> for Appearance {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value.as_str() {
            Some("light") => Appearance::Light,
            Some("dark") => Appearance::Dark,
            _ => Appearance::System,
        })
    }
}

/// What AudioSync captures.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SavedSource {
    /// Everything Windows plays.
    #[default]
    System,
    /// One app, by its executable's file name (`spotify.exe`), and the name
    /// it showed, for the source menu until it runs again.
    App {
        executable: String,
        #[serde(default)]
        name: String,
    },
}

/// The input device chosen as the source, kept beside [`Settings::source`]
/// rather than in it: an older build ignores a field it doesn't know, but
/// turns down the whole file over a source kind it doesn't know.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedInput {
    /// The capture endpoint's ID.
    pub id: String,
    /// The name it showed, for the source menu while it is unplugged.
    pub name: String,
}

/// How a source of several apps uses its list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixMode {
    /// Only these apps, mixed.
    #[default]
    Only,
    /// Every app except these.
    AllExcept,
}

/// One app of a mix: its executable's file name and the name it showed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedMixApp {
    pub executable: String,
    pub name: String,
}

/// Several apps chosen as the source, kept beside [`Settings::source`] for
/// the reason [`SavedInput`] is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedMix {
    pub mode: MixMode,
    #[serde(deserialize_with = "entries_that_read")]
    pub apps: Vec<SavedMixApp>,
}

/// An output's or AirPlay speaker's settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSpeaker {
    /// Whether the user switched it on.
    #[serde(deserialize_with = "bool_or_false")]
    pub enabled: bool,
    /// The volume slider position, 0 to 100.
    #[serde(deserialize_with = "number_or_nan")]
    pub volume: f64,
    /// The delay in milliseconds: for an output its offset relative to the
    /// reference, −2000 to +20 000 (0 to 20 000 in format 1, where it was
    /// the output's own trim); 0 to 20 000 for an AirPlay speaker's own delay.
    #[serde(deserialize_with = "number_or_nan")]
    pub delay_ms: f64,
}

impl Default for SavedSpeaker {
    fn default() -> SavedSpeaker {
        SavedSpeaker {
            enabled: false,
            volume: DEFAULT_LOCAL_POSITION,
            delay_ms: 0.0,
        }
    }
}

impl SavedSpeaker {
    /// Whether it holds nothing but the defaults, so it needn't be saved.
    pub fn is_default(&self) -> bool {
        *self == SavedSpeaker::default()
    }

    /// Delays clamped to `delays`; the volume to 0 to 100, or with `airplay`
    /// to 0 up, since an AirPlay speaker's own volume may pass 100 (S290).
    fn clamped(self, (min_delay_ms, max_delay_ms): (f64, f64), airplay: bool) -> SavedSpeaker {
        let volume = if airplay {
            Some(self.volume)
                .filter(|volume| volume.is_finite())
                .map(sanitize_own)
        } else {
            sanitize_position(self.volume)
        };
        SavedSpeaker {
            enabled: self.enabled,
            volume: volume.unwrap_or(DEFAULT_LOCAL_POSITION),
            delay_ms: if self.delay_ms.is_finite() {
                self.delay_ms.clamp(min_delay_ms, max_delay_ms)
            } else {
                0.0
            },
        }
    }
}

/// A group of speakers, by their speaker keys (`lc:` or `ap:` and the ID).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedGroup {
    pub id: String,
    #[serde(deserialize_with = "or_default")]
    pub name: String,
    #[serde(deserialize_with = "strings_that_read")]
    pub members: Vec<String>,
}

/// A zone (spec S245): its members by speaker key, a Cast speaker's
/// included, its volume slider position, 0 to 100, and the name each member
/// was last listed under (S286), left out of the file while empty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedZone {
    pub id: String,
    #[serde(deserialize_with = "or_default")]
    pub name: String,
    #[serde(deserialize_with = "strings_that_read")]
    pub members: Vec<String>,
    #[serde(deserialize_with = "number_or_nan")]
    pub volume: f64,
    #[serde(
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "names_that_are_text"
    )]
    pub member_names: BTreeMap<String, String>,
}

impl Default for SavedZone {
    fn default() -> SavedZone {
        SavedZone {
            id: String::new(),
            name: String::new(),
            members: Vec::new(),
            volume: DEFAULT_ZONE_VOLUME,
            member_names: BTreeMap::new(),
        }
    }
}

impl From<Zone> for SavedZone {
    fn from(zone: Zone) -> SavedZone {
        SavedZone {
            id: zone.id,
            name: zone.name,
            members: zone.members,
            volume: zone.volume,
            member_names: zone.member_names,
        }
    }
}

/// One stored member of a speaker preset (spec S251): its key and its own
/// volume, none for a fixed-volume Cast speaker.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPresetMember {
    pub key: String,
    #[serde(deserialize_with = "or_default")]
    pub volume: Option<f64>,
}

/// A zone volume a speaker preset stores, by zone ID (spec S251).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPresetZone {
    pub id: String,
    #[serde(deserialize_with = "number_or_nan")]
    pub volume: f64,
}

impl Default for SavedPresetZone {
    fn default() -> SavedPresetZone {
        SavedPresetZone {
            id: String::new(),
            volume: DEFAULT_ZONE_VOLUME,
        }
    }
}

/// A speaker preset (spec S251): the speakers on at save with their own
/// volumes, zone volumes, the master volume and the ten equalizer gains.
/// Never the preamp, the equalizer switch or a delay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPreset {
    pub id: String,
    #[serde(deserialize_with = "or_default")]
    pub name: String,
    #[serde(deserialize_with = "entries_that_read")]
    pub members: Vec<SavedPresetMember>,
    #[serde(deserialize_with = "entries_that_read")]
    pub zones: Vec<SavedPresetZone>,
    #[serde(deserialize_with = "number_or_nan")]
    pub master_volume: f64,
    /// Band 0 (32 Hz) to band 9 (16 kHz), in dB; an entry without ten is
    /// skipped when read, and a gain that isn't a number reads as 0 dB.
    #[serde(deserialize_with = "ten_gains")]
    pub gains_db: [f64; BAND_COUNT],
}

impl Default for SavedPreset {
    fn default() -> SavedPreset {
        SavedPreset {
            id: String::new(),
            name: String::new(),
            members: Vec::new(),
            zones: Vec::new(),
            master_volume: DEFAULT_MASTER_POSITION,
            gains_db: [0.0; BAND_COUNT],
        }
    }
}

impl From<Preset> for SavedPreset {
    fn from(preset: Preset) -> SavedPreset {
        SavedPreset {
            id: preset.id,
            name: preset.name,
            members: preset
                .members
                .into_iter()
                .map(|member| SavedPresetMember {
                    key: member.key,
                    volume: member.volume,
                })
                .collect(),
            zones: preset
                .zones
                .into_iter()
                .map(|zone| SavedPresetZone {
                    id: zone.id,
                    volume: zone.volume,
                })
                .collect(),
            master_volume: preset.master,
            gains_db: preset.gains_db,
        }
    }
}

impl SavedPreset {
    /// The core's preset, as stored; [`presets::from_stored`] makes it
    /// follow S251.
    pub fn preset(&self) -> Preset {
        Preset {
            id: self.id.clone(),
            name: self.name.clone(),
            members: self
                .members
                .iter()
                .map(|member| PresetMember {
                    key: member.key.clone(),
                    volume: member.volume,
                })
                .collect(),
            zones: self
                .zones
                .iter()
                .map(|zone| PresetZone {
                    id: zone.id.clone(),
                    volume: zone.volume,
                })
                .collect(),
            master: self.master_volume,
            gains_db: self.gains_db,
        }
    }
}

/// A preset's ten stored gains; a gain that isn't a number is 0 dB, as in
/// a saved curve, and another count leaves the preset unreadable.
fn ten_gains<'de, D>(deserializer: D) -> Result<[f64; BAND_COUNT], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let stored = value
        .as_array()
        .filter(|gains| gains.len() == BAND_COUNT)
        .ok_or_else(|| serde::de::Error::custom("a preset keeps ten gains"))?;
    let mut gains_db = [0.0; BAND_COUNT];
    for (gain, stored) in gains_db.iter_mut().zip(stored) {
        *gain = stored.as_f64().unwrap_or(0.0);
    }
    Ok(gains_db)
}

/// A stored list read leniently: an entry that doesn't read is skipped, and
/// anything that isn't a list reads as none.
fn entries_that_read<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Array(entries) = value else {
        return Ok(Vec::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect())
}

/// The equalizer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedEqualizer {
    #[serde(deserialize_with = "bool_or_false")]
    pub enabled: bool,
    #[serde(deserialize_with = "number_or_nan")]
    pub preamp_db: f64,
    /// Band 0 (32 Hz) to band 9 (16 kHz), in dB.
    #[serde(deserialize_with = "gains_that_read")]
    pub gains_db: Vec<f64>,
}

impl Default for SavedEqualizer {
    fn default() -> SavedEqualizer {
        SavedEqualizer::from(EqSettings::default())
    }
}

impl From<EqSettings> for SavedEqualizer {
    fn from(settings: EqSettings) -> SavedEqualizer {
        SavedEqualizer {
            enabled: settings.enabled,
            preamp_db: settings.preamp_db,
            gains_db: settings.gains_db.to_vec(),
        }
    }
}

impl SavedEqualizer {
    /// The settings, on the controls' steps: missing bands are flat, extra
    /// ones are dropped, gains are clamped to ±12 dB.
    pub fn settings(&self) -> EqSettings {
        let mut gains_db = [0.0; BAND_COUNT];
        for (gain, saved) in gains_db.iter_mut().zip(&self.gains_db) {
            *gain = *saved;
        }
        EqSettings {
            enabled: self.enabled,
            preamp_db: self.preamp_db,
            gains_db,
        }
        .quantized()
    }
}

/// The main window's frame with its size within the window's limits and its
/// position within Windows' coordinate range; `None` for a size that isn't a
/// number.
pub fn clamp_window(frame: WindowFrame) -> Option<WindowFrame> {
    if !frame.width.is_finite() || !frame.height.is_finite() {
        return None;
    }
    Some(WindowFrame {
        x: frame.x.clamp(-MAX_POSITION, MAX_POSITION),
        y: frame.y.clamp(-MAX_POSITION, MAX_POSITION),
        width: frame.width.clamp(MIN_WINDOW.0, MAX_WINDOW.0),
        height: frame.height.clamp(MIN_WINDOW.1, MAX_WINDOW.1),
        maximized: frame.maximized,
    })
}

/// One unreadable name costs only that name, not the whole settings file.
fn names_that_are_text<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Object(entries) = value else {
        return Ok(BTreeMap::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|(key, name)| match name {
            serde_json::Value::String(name) => Some((key, name)),
            _ => None,
        })
        .collect())
}

/// A speaker hidden with Hide Speaker: its speaker key, and the name and
/// kind its row showed then, so Hidden Speakers can name it while it isn't
/// listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHidden {
    pub key: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
}

/// The hidden speakers as stored, read leniently: an entry that isn't one is
/// skipped, and so is anything that isn't a list.
fn hidden_that_read<'de, D>(deserializer: D) -> Result<Vec<SavedHidden>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Array(entries) = value else {
        return Ok(Vec::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect())
}

/// A curve the user saved in the equalizer: a name and ten band gains in dB,
/// band 0 (32 Hz) first (spec S270).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedCurve {
    pub name: String,
    pub gains_db: Vec<f64>,
}

impl SavedCurve {
    /// The curve with missing bands flat and extra ones dropped; the core
    /// puts the gains on the controls' steps when the list is read.
    fn curve(&self) -> UserCurve {
        let mut gains_db = [0.0; BAND_COUNT];
        for (gain, saved) in gains_db.iter_mut().zip(&self.gains_db) {
            *gain = *saved;
        }
        UserCurve {
            name: self.name.clone(),
            gains_db,
        }
    }
}

impl From<&UserCurve> for SavedCurve {
    fn from(curve: &UserCurve) -> SavedCurve {
        SavedCurve {
            name: curve.name.clone(),
            gains_db: curve.gains_db.to_vec(),
        }
    }
}

/// The saved curves as stored, read leniently: an entry without a text name
/// is skipped, a band that is not a number reads as 0 dB, and anything that
/// isn't a list reads as no curves.
fn curves_that_read<'de, D>(deserializer: D) -> Result<Vec<SavedCurve>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Array(entries) = value else {
        return Ok(Vec::new());
    };
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_owned();
            let gains_db = entry
                .get("gains_db")
                .and_then(serde_json::Value::as_array)
                .map(|gains| gains.iter().map(|g| g.as_f64().unwrap_or(0.0)).collect())
                .unwrap_or_default();
            Some(SavedCurve { name, gains_db })
        })
        .collect())
}

/// The largest input pair index kept: a device with 1024 channels.
const MAX_INPUT_PAIR: u64 = 511;

/// The input channel pairs as stored, read leniently: an entry whose pair
/// isn't a whole number from 1 to [`MAX_INPUT_PAIR`] is skipped, and so is
/// anything that isn't a map.
fn pairs_that_read<'de, D>(deserializer: D) -> Result<BTreeMap<String, u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Object(entries) = value else {
        return Ok(BTreeMap::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|(id, pair)| {
            let pair = pair
                .as_u64()
                .filter(|pair| (1..=MAX_INPUT_PAIR).contains(pair))?;
            Some((id, u16::try_from(pair).ok()?))
        })
        .collect())
}

/// The saved curves cleaned as the core reads them back (spec S270).
pub fn clamp_curves(saved: &[SavedCurve]) -> Vec<UserCurve> {
    sanitized_curves(saved.iter().map(SavedCurve::curve))
}

/// Everything the file keeps. See the module documentation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The format version the file was written in.
    #[serde(deserialize_with = "version_or_current")]
    pub version: u32,
    /// The source; the whole system while [`Settings::input_device`] names
    /// the source, so an older build falls back to it.
    #[serde(deserialize_with = "or_default")]
    pub source: SavedSource,
    /// The input device chosen as the source, if one is.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "or_default"
    )]
    pub input_device: Option<SavedInput>,
    /// The channel pair each input device plays, by endpoint ID, as an
    /// index (1 for channels 3-4); a device without an entry plays 1-2.
    #[serde(
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "pairs_that_read"
    )]
    pub input_pairs: BTreeMap<String, u16>,
    /// Several apps chosen as the source, if they are; the source above is
    /// the whole system meanwhile.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "or_default"
    )]
    pub source_mix: Option<SavedMix>,
    /// The master volume slider position, 0 to 100.
    #[serde(deserialize_with = "number_or_nan")]
    pub master_volume: f64,
    /// The local outputs, by endpoint ID.
    #[serde(deserialize_with = "speakers_that_read")]
    pub outputs: BTreeMap<String, SavedSpeaker>,
    /// The AirPlay speakers, by the ID they announce.
    #[serde(deserialize_with = "speakers_that_read")]
    pub airplay: BTreeMap<String, SavedSpeaker>,
    /// The speakers marked auto-connect, by speaker key.
    #[serde(deserialize_with = "strings_that_read")]
    pub auto_connect: BTreeSet<String>,
    /// The names the user gave speakers, by speaker key (spec S228).
    #[serde(
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "names_that_are_text"
    )]
    pub speaker_names: BTreeMap<String, String>,
    /// The speakers hidden with Hide Speaker, by speaker key; left out of the
    /// file while none is.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "hidden_that_read"
    )]
    pub hidden_speakers: Vec<SavedHidden>,
    /// A group that doesn't read is skipped (S212), so a format 2 file's
    /// other groups still become zones.
    #[serde(deserialize_with = "entries_that_read")]
    pub groups: Vec<SavedGroup>,
    /// The zones, in order, each speaker in at most one (spec S245).
    #[serde(deserialize_with = "entries_that_read")]
    pub speaker_zones: Vec<SavedZone>,
    /// The groups were moved to zones (spec S248). A file without it, as
    /// every version 2 file, has not been migrated; the defaults have, since
    /// there are no groups to move. A mark that isn't a bool was written by
    /// a build that had migrated, so it reads as set.
    #[serde(default, deserialize_with = "bool_or_true")]
    pub speaker_zones_migrated: bool,
    /// The speaker presets, in order (spec S251).
    #[serde(deserialize_with = "entries_that_read")]
    pub speaker_presets: Vec<SavedPreset>,
    /// The groups that lost a member to the zones became presets (spec
    /// S254). A file without it has not been migrated; the defaults have.
    #[serde(default, deserialize_with = "bool_or_true")]
    pub speaker_presets_migrated: bool,
    /// The highest zone and preset ID numbers given out, so a deleted one's
    /// ID is never given again (spec S245, S251); 0 in an older file.
    #[serde(deserialize_with = "or_default")]
    pub last_zone_id: u64,
    #[serde(deserialize_with = "or_default")]
    pub last_preset_id: u64,
    /// Unreadable settings give the defaults (S212).
    #[serde(deserialize_with = "or_default")]
    pub equalizer: SavedEqualizer,
    /// The equalizer curves the user saved, in the picker's order (spec
    /// S270); left out of the file while there are none.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "curves_that_read"
    )]
    pub equalizer_curves: Vec<SavedCurve>,
    /// View > Show Virtual Devices.
    #[serde(deserialize_with = "bool_or_false")]
    pub show_virtual_devices: bool,
    /// View > Appearance.
    pub appearance: Appearance,
    /// The main window's place and size, once it has been shown; an
    /// unreadable one gives the default place and size.
    #[serde(deserialize_with = "or_default")]
    pub window: Option<WindowFrame>,
    /// The AirPlay delay adjustment A in milliseconds, −2000 to +20 000: the
    /// AirPlay speakers are taken to sound L = L_s + A after a frame reaches
    /// the sender (spec S178).
    #[serde(deserialize_with = "number_or_nan")]
    pub airplay_adjustment_ms: f64,
    /// What a session does when the app it captures quits, as the core
    /// stores it: `stop` or `system` (spec S227).
    #[serde(deserialize_with = "or_default")]
    pub when_source_quits: String,
    /// File > Web Remote: whether it is on, and its port.
    #[serde(deserialize_with = "or_default")]
    pub remote: RemoteSettings,
    /// The Google Cast speakers added by address (spec C18), as
    /// `host` or `host:port`.
    #[serde(deserialize_with = "strings_that_read")]
    pub cast_addresses: Vec<String>,
    /// File > AirPlay 2 (Experimental); read at launch only (spec P170).
    #[serde(deserialize_with = "bool_or_false")]
    pub airplay2_experimental: bool,
    /// File > Check for Updates Automatically (spec S305), on by default.
    #[serde(deserialize_with = "bool_or_true")]
    pub check_updates_automatically: bool,
    /// The last update check that read the appcast, in seconds since the
    /// Unix epoch (spec S305).
    #[serde(deserialize_with = "or_default")]
    pub last_update_check: Option<u64>,
    /// The version that failed its first start or was blocked (spec S316);
    /// one that doesn't read is no record.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "record_that_reads"
    )]
    pub update_record: Option<UpdateRecord>,
}

/// A stored number, or NaN for anything else, which `clamped` replaces with
/// the default: a value that isn't a number costs only itself (S212).
fn number_or_nan<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_f64().unwrap_or(f64::NAN))
}

/// The stored gains, NaN for each that isn't a number, which `settings`
/// makes flat; anything but a list is no gains.
fn gains_that_read<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<f64>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_array().map_or_else(Vec::new, |gains| {
        gains
            .iter()
            .map(|gain| gain.as_f64().unwrap_or(f64::NAN))
            .collect()
    }))
}

/// A stored format version; one that isn't a whole number counts as the
/// current format, so a hand-broken current file keeps its offsets instead
/// of going through the format 1 migration (S212).
fn version_or_current<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_u64().map_or(VERSION, |version| {
        u32::try_from(version).unwrap_or(u32::MAX)
    }))
}

fn record_that_reads<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<UpdateRecord>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// A stored value, or its type's default when it doesn't read (S212).
fn or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

pub(crate) fn bool_or_false<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(false))
}

fn bool_or_true<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(true))
}

/// The text entries of a stored list; anything that isn't a list is none.
fn strings_that_read<'de, D, C>(deserializer: D) -> Result<C, D::Error>
where
    D: serde::Deserializer<'de>,
    C: FromIterator<String>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let entries = match value {
        serde_json::Value::Array(entries) => entries,
        _ => Vec::new(),
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            serde_json::Value::String(text) => Some(text),
            _ => None,
        })
        .collect())
}

/// The stored speakers: one that isn't a speaker is dropped, and anything
/// that isn't a map is none.
fn speakers_that_read<'de, D>(deserializer: D) -> Result<BTreeMap<String, SavedSpeaker>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Object(entries) = value else {
        return Ok(BTreeMap::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|(id, speaker)| Some((id, serde_json::from_value(speaker).ok()?)))
        .collect())
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            version: VERSION,
            source: SavedSource::System,
            input_device: None,
            input_pairs: BTreeMap::new(),
            source_mix: None,
            master_volume: DEFAULT_MASTER_POSITION,
            outputs: BTreeMap::new(),
            airplay: BTreeMap::new(),
            auto_connect: BTreeSet::new(),
            speaker_names: BTreeMap::new(),
            hidden_speakers: Vec::new(),
            groups: Vec::new(),
            speaker_zones: Vec::new(),
            speaker_zones_migrated: true,
            speaker_presets: Vec::new(),
            speaker_presets_migrated: true,
            last_zone_id: 0,
            last_preset_id: 0,
            equalizer: SavedEqualizer::default(),
            equalizer_curves: Vec::new(),
            show_virtual_devices: false,
            appearance: Appearance::System,
            window: None,
            airplay_adjustment_ms: 0.0,
            when_source_quits: WhenSourceQuits::Stop.stored().to_owned(),
            remote: RemoteSettings::default(),
            cast_addresses: Vec::new(),
            airplay2_experimental: false,
            check_updates_automatically: true,
            last_update_check: None,
            update_record: None,
        }
    }
}

/// Keeps the first [`MAX_ENTRIES`] speakers whose ID is usable, with their
/// delays clamped to `delays` (smallest, largest) and their volumes as
/// [`SavedSpeaker::clamped`] keeps them.
fn clamp_speakers(
    speakers: BTreeMap<String, SavedSpeaker>,
    delays: (f64, f64),
    airplay: bool,
) -> BTreeMap<String, SavedSpeaker> {
    speakers
        .into_iter()
        .filter(|(id, _)| usable_id(id))
        .take(MAX_ENTRIES)
        .map(|(id, speaker)| (id, speaker.clamped(delays, airplay)))
        .collect()
}

/// The one-time migration of a format 1 file's output delays to offsets
/// relative to the reference output (spec S180), given the reference among
/// the outputs listed when it runs (S174), or `None` without one.
///
/// With t_R the reference's stored trim (0 when it has none or there is no
/// reference), every other output with a stored trim t_x gets the offset
/// t_x − t_R, clamped to −2000 to +20 000 ms; the AirPlay delay adjustment
/// becomes t_R, so the reference keeps sounding t_R after the sender's
/// latency; the reference gets no offset of its own (0), and every AirPlay
/// speaker's own delay stays as it is. The caller saves the result in the
/// new format.
pub fn migrate_delays(settings: &mut Settings, reference: Option<&str>) {
    let reference_trim = reference
        .and_then(|id| settings.outputs.get(id))
        .map_or(0.0, |speaker| speaker.delay_ms);
    for (id, speaker) in settings.outputs.iter_mut() {
        speaker.delay_ms = if Some(id.as_str()) == reference {
            0.0
        } else {
            (speaker.delay_ms - reference_trim).clamp(MIN_OFFSET_MS, MAX_OFFSET_MS)
        };
    }
    settings.airplay_adjustment_ms = reference_trim;
}

fn usable_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_TEXT
}

/// Whether `key` is a key a group or the auto-connect list can hold: `lc:`
/// or `ap:` and an ID. Neither switches a Cast speaker yet.
fn usable_key(key: &str) -> bool {
    usable_name_key(key) && !matches!(SpeakerKey::parse(key), Some(SpeakerKey::Cast(_)))
}

/// A key a speaker's name or hidden entry may be saved under: any speaker
/// key with an ID, a Google Cast speaker's included.
fn usable_name_key(key: &str) -> bool {
    key.len() <= MAX_TEXT && SpeakerKey::parse(key).is_some_and(|key| !key.id().is_empty())
}

/// The first [`MAX_ENTRIES`] hidden speakers with a usable key, one entry
/// per key.
fn clamp_hidden(hidden: Vec<SavedHidden>) -> Vec<SavedHidden> {
    let mut keys = BTreeSet::new();
    hidden
        .into_iter()
        .filter(|entry| usable_name_key(&entry.key) && keys.insert(entry.key.clone()))
        .take(MAX_ENTRIES)
        .map(|entry| SavedHidden {
            key: entry.key,
            name: cut(entry.name),
            kind: cut(entry.kind),
        })
        .collect()
}

/// `text` cut to at most [`MAX_TEXT`] bytes, at a character boundary.
fn cut(mut text: String) -> String {
    if text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

impl Settings {
    /// The settings with every value in range: volumes 0 to 100, delays 0 to
    /// their limit, equalizer gains on the controls' steps, the window
    /// within its limits; unusable IDs, keys and empty groups dropped, and
    /// every list cut to [`MAX_ENTRIES`]. The version becomes [`VERSION`].
    pub fn clamped(self) -> Settings {
        let source = match self.source {
            SavedSource::App { executable, name } if usable_id(&executable) => SavedSource::App {
                executable,
                name: cut(name),
            },
            _ => SavedSource::System,
        };
        let input_device = self
            .input_device
            .filter(|input| usable_id(&input.id))
            .map(|input| SavedInput {
                id: input.id,
                name: cut(input.name),
            });
        let mut group_ids = BTreeSet::new();
        let groups = self
            .groups
            .into_iter()
            .filter_map(|group| {
                let members: Vec<String> = group
                    .members
                    .into_iter()
                    .filter(|key| usable_key(key))
                    .take(MAX_ENTRIES)
                    .collect();
                (usable_id(&group.id) && !members.is_empty() && group_ids.insert(group.id.clone()))
                    .then(|| SavedGroup {
                        id: group.id,
                        name: cut(group.name),
                        members,
                    })
            })
            .take(MAX_ENTRIES)
            .collect();
        let speaker_zones = Zones::from_stored(
            self.speaker_zones
                .into_iter()
                .filter(|zone| usable_id(&zone.id))
                .take(MAX_ENTRIES)
                .map(|zone| Zone {
                    id: zone.id,
                    name: cut(zone.name),
                    members: zone
                        .members
                        .into_iter()
                        .filter(|key| usable_name_key(key))
                        .take(MAX_ENTRIES)
                        .collect(),
                    volume: zone.volume,
                    member_names: zone
                        .member_names
                        .into_iter()
                        .map(|(key, name)| (key, cut(name)))
                        .collect(),
                }),
        )
        .into_list()
        .into_iter()
        .map(SavedZone::from)
        .collect();
        let speaker_presets = presets::from_stored(
            self.speaker_presets
                .iter()
                .filter(|preset| usable_id(&preset.id))
                .take(MAX_ENTRIES)
                .map(|saved| {
                    let preset = saved.preset();
                    Preset {
                        name: cut(preset.name),
                        members: preset
                            .members
                            .into_iter()
                            .filter(|member| usable_name_key(&member.key))
                            .take(MAX_ENTRIES)
                            .collect(),
                        zones: preset
                            .zones
                            .into_iter()
                            .filter(|zone| usable_id(&zone.id))
                            .take(MAX_ENTRIES)
                            .collect(),
                        ..preset
                    }
                }),
        )
        .into_iter()
        .map(SavedPreset::from)
        .collect();
        // A format 1 file kept trims, 0 to 20 000 ms; [`migrate_delays`]
        // turns them into offsets.
        let output_delays = if self.version < OFFSETS_VERSION {
            (0.0, MAX_DELAY_MS)
        } else {
            (MIN_OFFSET_MS, MAX_OFFSET_MS)
        };
        let source_mix = self.source_mix.and_then(|mix| {
            let mut apps: Vec<SavedMixApp> = Vec::new();
            for app in mix.apps {
                if usable_id(&app.executable)
                    && !apps
                        .iter()
                        .any(|kept| kept.executable.eq_ignore_ascii_case(&app.executable))
                    && apps.len() < MAX_ENTRIES
                {
                    apps.push(SavedMixApp {
                        executable: app.executable,
                        name: cut(app.name),
                    });
                }
            }
            (!apps.is_empty()).then_some(SavedMix {
                mode: mix.mode,
                apps,
            })
        });
        Settings {
            version: VERSION,
            source,
            input_device,
            input_pairs: self
                .input_pairs
                .into_iter()
                .filter(|(id, pair)| {
                    usable_id(id) && (1..=MAX_INPUT_PAIR).contains(&u64::from(*pair))
                })
                .take(MAX_ENTRIES)
                .collect(),
            source_mix,
            master_volume: sanitize_position(self.master_volume).unwrap_or(DEFAULT_MASTER_POSITION),
            outputs: clamp_speakers(self.outputs, output_delays, false),
            airplay: clamp_speakers(self.airplay, (0.0, MAX_AIRPLAY_DELAY_MS), true),
            auto_connect: self
                .auto_connect
                .into_iter()
                .filter(|key| usable_key(key))
                .take(MAX_ENTRIES)
                .collect(),
            speaker_names: SpeakerNames::from_saved(
                &self
                    .speaker_names
                    .into_iter()
                    .filter(|(key, _)| usable_name_key(key))
                    .take(MAX_ENTRIES)
                    .collect(),
            )
            .saved(),
            hidden_speakers: clamp_hidden(self.hidden_speakers),
            groups,
            speaker_zones,
            speaker_zones_migrated: self.speaker_zones_migrated,
            speaker_presets,
            speaker_presets_migrated: self.speaker_presets_migrated,
            last_zone_id: self.last_zone_id,
            last_preset_id: self.last_preset_id,
            equalizer: SavedEqualizer::from(self.equalizer.settings()),
            equalizer_curves: clamp_curves(&self.equalizer_curves)
                .iter()
                .map(SavedCurve::from)
                .collect(),
            show_virtual_devices: self.show_virtual_devices,
            appearance: self.appearance,
            window: self.window.and_then(clamp_window),
            airplay_adjustment_ms: sanitize_airplay_adjustment_ms(self.airplay_adjustment_ms)
                .unwrap_or(0.0),
            when_source_quits: WhenSourceQuits::from_stored(Some(&self.when_source_quits))
                .stored()
                .to_owned(),
            remote: self.remote.clamped(),
            cast_addresses: cast_address::clamped(self.cast_addresses),
            airplay2_experimental: self.airplay2_experimental,
            check_updates_automatically: self.check_updates_automatically,
            last_update_check: self.last_update_check,
            update_record: self.update_record,
        }
    }

    /// The one-time move of the groups to zones (spec S248): without the
    /// mark, the zones become the groups' ([`zones::zones_from_groups`]) and
    /// the mark is set; the groups stay. With it, nothing changes.
    pub fn with_zones_migrated(self) -> Settings {
        if self.speaker_zones_migrated {
            return self;
        }
        let speaker_zones =
            zones::zones_from_groups(self.groups.iter().map(|group| zones::SavedGroup {
                id: &group.id,
                name: &group.name,
                members: &group.members,
            }))
            .into_list()
            .into_iter()
            .map(SavedZone::from)
            .collect();
        Settings {
            speaker_zones,
            speaker_zones_migrated: true,
            ..self
        }
    }

    /// The one-time move of the groups that lost a member to the zones to
    /// presets (spec S254), after [`Settings::with_zones_migrated`]: without
    /// the mark, the presets become [`presets::presets_from_groups`]' and the
    /// mark is set, both in the next save. With it, nothing changes.
    pub fn with_presets_migrated(self) -> Settings {
        if self.speaker_presets_migrated {
            return self;
        }
        let groups: Vec<zones::SavedGroup<'_>> = self
            .groups
            .iter()
            .map(|group| zones::SavedGroup {
                id: &group.id,
                name: &group.name,
                members: &group.members,
            })
            .collect();
        let current = Zones::from_stored(self.speaker_zones.iter().map(|zone| Zone {
            id: zone.id.clone(),
            name: zone.name.clone(),
            members: zone.members.clone(),
            volume: zone.volume,
            member_names: zone.member_names.clone(),
        }));
        let volume = |key: &str| {
            match SpeakerKey::parse(key)? {
                SpeakerKey::Local(endpoint_id) => self.outputs.get(&endpoint_id),
                SpeakerKey::AirPlay(id) => self.airplay.get(&id),
                SpeakerKey::Cast(_) => None,
            }
            .map(|speaker| speaker.volume)
        };
        let saved = presets::Saved {
            zones: &current,
            volume: &volume,
            master: self.master_volume,
            gains_db: self.equalizer.settings().gains_db,
        };
        let speaker_presets = presets::presets_from_groups(&groups, &saved)
            .into_iter()
            .take(MAX_ENTRIES)
            .map(SavedPreset::from)
            .collect();
        Settings {
            speaker_presets,
            speaker_presets_migrated: true,
            ..self
        }
    }

    /// The file's contents.
    pub fn to_json(&self) -> String {
        // Serializing these types can't fail: every map key is a string and
        // every number is finite once clamped.
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// The settings in `text`, clamped, with what reading them found; or why
    /// they were ignored.
    pub fn from_json(text: &str) -> Result<(Settings, Loaded), String> {
        /// Only the version, read first so a newer file is turned down
        /// before its fields are looked at.
        #[derive(Deserialize)]
        struct Header {
            #[serde(default, deserialize_with = "version_or_current")]
            version: u32,
        }
        let header: Header =
            serde_json::from_str(text).map_err(|error| format!("it isn't valid ({error})"))?;
        if header.version > VERSION {
            return Err(format!(
                "a newer AudioSync wrote it (format version {}, this one reads up to {VERSION})",
                header.version
            ));
        }
        let read: Settings =
            serde_json::from_str(text).map_err(|error| format!("it isn't valid ({error})"))?;
        // The version a file without one was written in.
        let version = header.version;
        let read = Settings { version, ..read };
        let clamped = read.clone().clamped();
        let adjusted = clamped
            != Settings {
                version: VERSION,
                ..read
            };
        Ok((
            clamped.with_zones_migrated().with_presets_migrated(),
            Loaded::Read {
                version,
                adjusted,
                unreadable: unreadable_fields(text),
            },
        ))
    }
}

/// The fields of `text` whose value the lenient reader replaced with the
/// default, in [`Settings`] order, then the equalizer values that did not
/// read. Entries skipped inside a list or map that reads count toward
/// `adjusted` instead.
fn unreadable_fields(text: &str) -> Vec<String> {
    use serde_json::Value;
    fn reads<T: serde::de::DeserializeOwned>(value: &Value) -> bool {
        T::deserialize(value).is_ok()
    }
    fn is_appearance(value: &Value) -> bool {
        matches!(value.as_str(), Some("system" | "light" | "dark"))
    }
    // The two migration marks are left out: one that isn't a bool reads as
    // set on purpose (spec S248, S254), so nothing falls back.
    type Reads = fn(&Value) -> bool;
    const FIELDS: &[(&str, Reads)] = &[
        ("version", Value::is_u64),
        ("source", reads::<SavedSource>),
        ("input_device", reads::<Option<SavedInput>>),
        ("input_pairs", Value::is_object),
        ("source_mix", reads::<Option<SavedMix>>),
        ("master_volume", Value::is_number),
        ("outputs", Value::is_object),
        ("airplay", Value::is_object),
        ("auto_connect", Value::is_array),
        ("speaker_names", Value::is_object),
        ("hidden_speakers", Value::is_array),
        ("groups", Value::is_array),
        ("speaker_zones", Value::is_array),
        ("speaker_presets", Value::is_array),
        ("last_zone_id", Value::is_u64),
        ("last_preset_id", Value::is_u64),
        ("equalizer", reads::<SavedEqualizer>),
        ("equalizer_curves", Value::is_array),
        ("show_virtual_devices", Value::is_boolean),
        ("appearance", is_appearance),
        ("window", reads::<Option<WindowFrame>>),
        ("airplay_adjustment_ms", Value::is_number),
        ("when_source_quits", Value::is_string),
        ("remote", reads::<RemoteSettings>),
        ("cast_addresses", Value::is_array),
        ("airplay2_experimental", Value::is_boolean),
        ("check_updates_automatically", Value::is_boolean),
        ("last_update_check", reads::<Option<u64>>),
        ("update_record", reads::<Option<UpdateRecord>>),
    ];
    let Ok(Value::Object(stored)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut unreadable: Vec<String> = FIELDS
        .iter()
        .filter(|(name, reads)| stored.get(*name).is_some_and(|value| !reads(value)))
        .map(|(name, _)| (*name).to_owned())
        .collect();
    if let Some(Value::Object(equalizer)) = stored.get("equalizer") {
        let values: [(&str, Reads); 3] = [
            ("enabled", Value::is_boolean),
            ("preamp_db", Value::is_number),
            ("gains_db", Value::is_array),
        ];
        for (name, reads) in values {
            if equalizer.get(name).is_some_and(|value| !reads(value)) {
                unreadable.push(format!("equalizer.{name}"));
            }
        }
        if let Some(Value::Array(gains)) = equalizer.get("gains_db") {
            for (band, gain) in gains.iter().enumerate() {
                if !gain.is_number() {
                    unreadable.push(format!("equalizer.gains_db[{band}]"));
                }
            }
        }
    }
    unreadable
}

/// What reading the file found, for the debug log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    /// There is no file yet.
    NoFile,
    /// The file was read, written in format `version`; `adjusted` when
    /// values out of range were clamped or unusable entries dropped, and
    /// `unreadable` names the fields that fell back to their defaults.
    Read {
        version: u32,
        adjusted: bool,
        unreadable: Vec<String>,
    },
    /// The file was ignored, and why.
    Ignored(String),
}

impl fmt::Display for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Loaded::NoFile => write!(f, "no file yet, the defaults apply"),
            Loaded::Read {
                version, adjusted, ..
            } => {
                write!(f, "read (format version {version}")?;
                if *version < VERSION {
                    write!(f, ", older than this build's {VERSION}")?;
                }
                if *adjusted {
                    write!(f, "; values out of range clamped, unusable entries dropped")?;
                }
                write!(f, ")")
            }
            Loaded::Ignored(reason) => write!(
                f,
                "ignored: {reason}; the defaults apply, and it is kept aside before a new \
                 file replaces it, or applied once it can be read"
            ),
        }
    }
}

/// A settings file [`SettingsFile::save`] copied aside before it first
/// saved over it, and where the copy is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeptAside {
    /// A newer AudioSync's file.
    Newer(PathBuf),
    /// A file the last load ignored: it couldn't be read at launch, or it
    /// was damaged. The settings in effect are the defaults, with what the
    /// user changed since.
    Ignored(PathBuf),
}

impl KeptAside {
    /// The copy's file name, for the log and the user.
    pub fn file_name(&self) -> String {
        let (KeptAside::Newer(path) | KeptAside::Ignored(path)) = self;
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

/// What a later try at the file the last load ignored found.
#[derive(Clone, Debug, PartialEq)]
pub enum Retried {
    /// It reads as settings now (another program held it at launch): these
    /// apply as if read at launch, and nothing was saved over it.
    Read(Box<(Settings, Loaded)>),
    /// It still isn't settings this build reads, so it was copied aside.
    KeptAside(KeptAside),
}

/// Where the settings are kept: the file, or nowhere (this run only).
#[derive(Clone, Debug, Default)]
pub struct SettingsFile {
    path: Option<PathBuf>,
    /// The last load ignored the file there (it couldn't be read, or it
    /// wasn't a settings file this build reads): it is kept aside before
    /// the next save replaces it.
    keep_aside: Cell<bool>,
    /// Polls left before [`SettingsFile::keep_ignored_aside_if_due`] tries
    /// again to keep that file aside.
    keep_aside_wait: Cell<u32>,
}

impl SettingsFile {
    /// Where the app keeps the file: in `%LOCALAPPDATA%\AudioSync`, or
    /// `None` when Windows names no such folder.
    #[cfg(os_backend)]
    pub fn default_path() -> Option<PathBuf> {
        data_folder::folder().map(|folder| folder.join(FILE_NAME))
    }

    /// The file at `path`.
    #[cfg(any(os_backend, test))]
    pub fn at(path: PathBuf) -> SettingsFile {
        SettingsFile {
            path: Some(path),
            keep_aside: Cell::new(false),
            keep_aside_wait: Cell::new(0),
        }
    }

    /// No file: the settings last until the app quits.
    #[cfg(any(os_backend, test))]
    pub fn in_memory() -> SettingsFile {
        SettingsFile::default()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The settings in the file, or the defaults, with what was found. A
    /// read that fails is tried once more after a short pause
    /// ([`data_folder::read_at_launch`]). A file it ignores is kept aside by
    /// [`SettingsFile::keep_ignored_aside`] or the next
    /// [`SettingsFile::save`].
    pub fn load(&self) -> (Settings, Loaded) {
        let Some(path) = &self.path else {
            return (Settings::default(), Loaded::NoFile);
        };
        let read = match data_folder::read_at_launch(path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| "it isn't text".to_owned())
                .and_then(|text| Settings::from_json(&text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.keep_aside.set(false);
                return (Settings::default(), Loaded::NoFile);
            }
            Err(error) => Err(format!("it can't be read, even at a second try ({error})")),
        };
        self.keep_aside.set(read.is_err());
        match read {
            Ok(read) => read,
            Err(reason) => (Settings::default(), Loaded::Ignored(reason)),
        }
    }

    /// Keeps the file the last load ignored aside now, without saving
    /// anything, so the user can be told at launch where it is: copied as
    /// [`SettingsFile::save`] would ([`IGNORED_FILE_NAME`]), and the copy
    /// returned, once; a file that reads as settings now is returned read
    /// instead (S212). A newer AudioSync's file isn't copied here: `save`
    /// keeps one aside whenever it saves over one. While the file still
    /// can't be read, the error is returned and the next try is due
    /// [`data_folder::KEEP_ASIDE_RETRY_POLLS`] polls later
    /// ([`SettingsFile::keep_ignored_aside_if_due`]), or at the next save.
    pub fn keep_ignored_aside(&self) -> io::Result<Option<Retried>> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        if !self.keep_aside.get() {
            return Ok(None);
        }
        let kept = match fs::read(path) {
            Ok(bytes) if written_by_a_newer_build(&bytes) => Ok(None),
            Ok(bytes) => match std::str::from_utf8(&bytes)
                .map_err(|_| String::new())
                .and_then(Settings::from_json)
            {
                Ok(read) => Ok(Some(Retried::Read(Box::new(read)))),
                Err(_) => data_folder::keep_bytes_aside(path, IGNORED_FILE_NAME, &bytes)
                    .map(|kept| Some(Retried::KeptAside(KeptAside::Ignored(kept)))),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        };
        match kept {
            Ok(kept) => {
                self.keep_aside.set(false);
                Ok(kept)
            }
            // The file can't be read yet, or the copy couldn't be written
            // (a full disk, a folder that can't be written): either way the
            // next try waits, rather than coming at every poll.
            Err(error) => {
                self.keep_aside_wait
                    .set(data_folder::KEEP_ASIDE_RETRY_POLLS);
                Err(error)
            }
        }
    }

    /// [`SettingsFile::keep_ignored_aside`] again, once its next try is due:
    /// called at every poll, it tries every
    /// [`data_folder::KEEP_ASIDE_RETRY_POLLS`] polls while the file ignored
    /// at launch couldn't be kept aside yet, and does nothing otherwise.
    pub fn keep_ignored_aside_if_due(&self) -> io::Result<Option<Retried>> {
        if !self.keep_aside.get() {
            return Ok(None);
        }
        let wait = self.keep_aside_wait.get().saturating_sub(1);
        self.keep_aside_wait.set(wait);
        if wait > 0 {
            return Ok(None);
        }
        self.keep_ignored_aside()
    }

    /// Replaces the file with `settings`. Without a file it does nothing.
    ///
    /// A file the last load ignored is first copied next to it, so the
    /// user's settings in it aren't lost: a newer AudioSync's file
    /// (whenever this build saves over one) to a copy named after
    /// [`NEWER_FILE_NAME`], any other to one named after
    /// [`IGNORED_FILE_NAME`], each with the time
    /// ([`data_folder::keep_bytes_aside`]), so no copy ever replaces an
    /// older one; what was kept is returned. While such a file still can't
    /// be read, or the copy fails, the file is left alone and the error
    /// returned; one that reads as settings now is left alone and returned.
    pub fn save(&self, settings: &Settings) -> io::Result<Option<Retried>> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let kept = match fs::read(path) {
            Ok(bytes) if written_by_a_newer_build(&bytes) => {
                Some(Retried::KeptAside(KeptAside::Newer(
                    data_folder::keep_bytes_aside(path, NEWER_FILE_NAME, &bytes)?,
                )))
            }
            Ok(_) | Err(_) if self.keep_aside.get() => match self.keep_ignored_aside()? {
                read @ Some(Retried::Read(_)) => return Ok(read),
                kept => kept,
            },
            _ => None,
        };
        self.keep_aside.set(false);
        data_folder::write_atomically(path, settings.to_json().as_bytes())?;
        Ok(kept)
    }
}

/// Whether `bytes` are a settings file in a format newer than [`VERSION`].
fn written_by_a_newer_build(bytes: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Header {
        #[serde(default, deserialize_with = "version_or_current")]
        version: u32,
    }
    serde_json::from_slice::<Header>(bytes).is_ok_and(|header| header.version > VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_folder::copies_kept_aside;
    use crate::debug_log::test_dir;

    /// Settings with something in every field.
    pub fn sample() -> Settings {
        let mut settings = Settings {
            source: SavedSource::App {
                executable: "spotify.exe".to_owned(),
                name: "Spotify".to_owned(),
            },
            master_volume: 72.5,
            show_virtual_devices: true,
            appearance: Appearance::Dark,
            window: Some(WindowFrame {
                x: -1200,
                y: 40,
                width: 1024.0,
                height: 700.0,
                maximized: true,
            }),
            remote: RemoteSettings {
                on: true,
                port: 8123,
            },
            cast_addresses: vec!["speaker.local".to_owned(), "192.0.2.10:8010".to_owned()],
            ..Settings::default()
        };
        settings.outputs.insert(
            "{0.0.0.00000000}.{speakers}".to_owned(),
            SavedSpeaker {
                enabled: true,
                volume: 45.0,
                delay_ms: 180.0,
            },
        );
        settings.airplay.insert(
            "AA:BB:CC:DD:EE:FF".to_owned(),
            SavedSpeaker {
                enabled: true,
                volume: 30.0,
                delay_ms: 250.0,
            },
        );
        settings
            .auto_connect
            .insert("lc:{0.0.0.00000000}.{speakers}".to_owned());
        settings.speaker_names.insert(
            "lc:{0.0.0.00000000}.{speakers}".to_owned(),
            "Desk".to_owned(),
        );
        settings.groups.push(SavedGroup {
            id: "group:3".to_owned(),
            name: "Downstairs".to_owned(),
            members: vec![
                "lc:{0.0.0.00000000}.{speakers}".to_owned(),
                "ap:AA:BB:CC:DD:EE:FF".to_owned(),
            ],
        });
        settings.equalizer = SavedEqualizer {
            enabled: true,
            preamp_db: -3.0,
            gains_db: vec![4.0, 3.0, 0.0, 0.0, -1.5, 0.0, 0.0, 2.0, 2.5, 3.0],
        };
        settings
    }

    /// Settings with every field away from its default, except the version
    /// and the migration marks: a file without them is migrated as it loads.
    pub fn every_field() -> Settings {
        let sample = sample();
        Settings {
            version: VERSION,
            source: sample.source,
            input_device: Some(SavedInput {
                id: "input".to_owned(),
                name: "Input".to_owned(),
            }),
            input_pairs: BTreeMap::from([("input".to_owned(), 2)]),
            source_mix: Some(SavedMix {
                mode: MixMode::AllExcept,
                apps: vec![SavedMixApp {
                    executable: "player.exe".to_owned(),
                    name: "Player".to_owned(),
                }],
            }),
            master_volume: sample.master_volume,
            outputs: sample.outputs,
            airplay: sample.airplay,
            auto_connect: sample.auto_connect,
            speaker_names: sample.speaker_names,
            hidden_speakers: vec![SavedHidden {
                key: "lc:hidden".to_owned(),
                name: "Hidden".to_owned(),
                kind: "output".to_owned(),
            }],
            groups: sample.groups,
            speaker_zones: vec![SavedZone {
                id: "z1".to_owned(),
                name: "Den".to_owned(),
                members: vec!["lc:{0.0.0.00000000}.{speakers}".to_owned()],
                volume: 60.0,
                member_names: BTreeMap::from([(
                    "lc:{0.0.0.00000000}.{speakers}".to_owned(),
                    "Desk".to_owned(),
                )]),
            }],
            speaker_zones_migrated: true,
            speaker_presets: vec![SavedPreset {
                id: "p1".to_owned(),
                name: "Evening".to_owned(),
                members: vec![SavedPresetMember {
                    key: "lc:{0.0.0.00000000}.{speakers}".to_owned(),
                    volume: Some(40.0),
                }],
                zones: vec![SavedPresetZone {
                    id: "z1".to_owned(),
                    volume: 70.0,
                }],
                master_volume: 80.0,
                gains_db: [1.0; BAND_COUNT],
            }],
            speaker_presets_migrated: true,
            last_zone_id: 1,
            last_preset_id: 1,
            equalizer: sample.equalizer,
            equalizer_curves: vec![SavedCurve {
                name: "Curve".to_owned(),
                gains_db: vec![1.0; BAND_COUNT],
            }],
            show_virtual_devices: sample.show_virtual_devices,
            appearance: sample.appearance,
            window: sample.window,
            airplay_adjustment_ms: 120.0,
            when_source_quits: WhenSourceQuits::SwitchToSystem.stored().to_owned(),
            remote: sample.remote,
            cast_addresses: sample.cast_addresses,
            airplay2_experimental: true,
            check_updates_automatically: false,
            last_update_check: Some(1_800_000_000),
            update_record: Some(UpdateRecord {
                version: "9.9.9".to_owned(),
                reason: crate::ui_model::UpdateRecordReason::Blocked,
            }),
        }
    }

    #[test]
    fn speaker_names_are_kept_cleared_and_unusable_entries_skipped() {
        let file = SettingsFile::at(test_dir("settings-speaker-names").join(FILE_NAME));
        let mut names = SpeakerNames::default();
        names.rename("lc:desk", "Desk", "Speakers");
        names.rename("ap:kitchen", "Kitchen left", "Kitchen");
        names.rename("cc:attic", "Attic", "Living Room speaker");
        let settings = Settings {
            speaker_names: names.saved(),
            ..Settings::default()
        };
        file.save(&settings).expect("saved");
        let (read, _) = file.load();
        let mut names = SpeakerNames::from_saved(&read.speaker_names);
        assert_eq!(
            names.shown("lc:desk", "Speakers"),
            "Desk",
            "kept across a restart"
        );
        assert_eq!(names.shown("ap:kitchen", "Kitchen"), "Kitchen left");
        assert_eq!(
            names.shown("cc:attic", "Living Room speaker"),
            "Attic",
            "a Google Cast speaker's too"
        );
        assert_eq!(names.shown("lc:other", "Other"), "Other", "an unknown key");

        names.rename("lc:desk", "", "Speakers");
        let settings = Settings {
            speaker_names: names.saved(),
            ..Settings::default()
        };
        file.save(&settings).expect("saved");
        let (read, _) = file.load();
        assert!(
            !read.speaker_names.contains_key("lc:desk"),
            "cleared, the key goes"
        );
        assert_eq!(read.speaker_names.len(), 2);

        let text = r#"{"version": 2, "speaker_names": {
            "lc:desk": "  Desk  ", "nonsense": "Name", "ap:": "Name", "lc:blank": "  ",
            "cc:attic": "Attic", "cc:": "Name"
        }}"#;
        let (read, _) = Settings::from_json(text).expect("read");
        assert_eq!(
            read.speaker_names,
            BTreeMap::from([
                ("cc:attic".to_owned(), "Attic".to_owned()),
                ("lc:desk".to_owned(), "Desk".to_owned()),
            ]),
            "unusable keys and blank names are skipped"
        );
        let text = r#"{"version": 2, "master_volume": 40, "speaker_names": {
            "lc:desk": "Desk", "lc:odd": 5, "ap:x": null
        }}"#;
        let (read, _) = Settings::from_json(text).expect("one odd name spoils nothing");
        assert_eq!(read.master_volume, 40.0);
        assert_eq!(
            read.speaker_names,
            BTreeMap::from([("lc:desk".to_owned(), "Desk".to_owned())])
        );
        let (read, _) = Settings::from_json(r#"{"version": 2, "speaker_names": 3}"#).expect("read");
        assert!(read.speaker_names.is_empty());
        let without = Settings::default().to_json();
        assert!(!without.contains("speaker_names"), "no names, no field");
    }

    #[test]
    fn hidden_speakers_are_read_leniently_and_left_out_while_none_is() {
        let text = r#"{"version": 2, "master_volume": 40, "hidden_speakers": [
            {"key": "lc:desk", "name": "Desk", "kind": "Speakers"},
            {"key": "cc:attic"},
            {"name": "No key"},
            {"key": "nonsense", "name": "Bad key"},
            {"key": "lc:desk", "name": "Twice"},
            7
        ]}"#;
        let (read, _) = Settings::from_json(text).expect("one odd entry spoils nothing");
        assert_eq!(read.master_volume, 40.0);
        assert_eq!(
            read.hidden_speakers,
            [
                SavedHidden {
                    key: "lc:desk".into(),
                    name: "Desk".into(),
                    kind: "Speakers".into(),
                },
                SavedHidden {
                    key: "cc:attic".into(),
                    name: String::new(),
                    kind: String::new(),
                },
            ]
        );
        let (read, _) =
            Settings::from_json(r#"{"version": 2, "hidden_speakers": {}}"#).expect("read");
        assert!(read.hidden_speakers.is_empty());
        let without = Settings::default().to_json();
        assert!(
            !without.contains("hidden_speakers"),
            "none hidden, no field"
        );
    }

    #[test]
    fn groups_and_auto_connect_keep_leaving_cast_keys_out() {
        let text = r#"{"version": 2,
            "groups": [{"id": "g1", "name": "Den", "members": ["cc:attic", "lc:desk"]}],
            "auto_connect": ["cc:attic", "ap:pair:5A1B"]
        }"#;
        let (read, _) = Settings::from_json(text).expect("read");
        assert_eq!(read.groups.len(), 1);
        assert_eq!(read.groups[0].members, ["lc:desk"]);
        assert_eq!(
            read.auto_connect,
            BTreeSet::from(["ap:pair:5A1B".to_owned()])
        );
    }

    // covers: S245, S248
    #[test]
    fn a_v2_settings_file_is_read_as_v3_with_its_groups_as_zones() {
        let text = r#"{"version": 2,
            "groups": [
                {"id": "g1", "name": "Living Room", "members": ["lc:desk", "ap:pair:5A1B"]},
                {"id": "g2", "name": "Group 2", "members": ["ap:pair:5A1B", "ap:kitchen"]}
            ]
        }"#;
        let (read, found) = Settings::from_json(text).expect("read");
        assert_eq!(
            found,
            Loaded::Read {
                version: 2,
                adjusted: false,
                unreadable: Vec::new(),
            }
        );
        assert_eq!(read.version, VERSION);
        assert!(read.speaker_zones_migrated);
        assert_eq!(read.groups.len(), 2, "the groups stay");
        let zone = |id: &str, name: &str, members: &[&str]| SavedZone {
            id: id.to_owned(),
            name: name.to_owned(),
            members: members.iter().map(|key| (*key).to_owned()).collect(),
            volume: 100.0,
            member_names: BTreeMap::new(),
        };
        assert_eq!(
            read.speaker_zones,
            [
                zone("g1", "Living Room", &["lc:desk", "ap:pair:5A1B"]),
                zone("g2", "Group 2", &["ap:kitchen"]),
            ]
        );
        // Written as version 3 with the mark, it reads back the same, and the
        // groups never migrate again.
        let json = read.to_json();
        assert!(
            json.contains("\"version\": 3") && json.contains("\"speaker_zones_migrated\": true")
        );
        let (again, _) = Settings::from_json(&json).expect("read");
        assert_eq!(again, read);
        let mut changed = read.clone();
        changed.groups.clear();
        let (later, _) = Settings::from_json(&changed.to_json()).expect("read");
        assert_eq!(later.speaker_zones, read.speaker_zones);
    }

    // covers: S245
    #[test]
    fn zones_keep_cast_keys_and_empty_zones_and_each_speaker_once() {
        let text = r#"{"version": 3, "speaker_zones_migrated": true,
            "speaker_zones": [
                {"id": "z1", "name": "Den", "members": ["cc:attic", "lc:desk", "lc:desk"], "volume": 140},
                {"id": "z2", "name": "Empty", "members": [], "volume": -4},
                {"id": "z3", "name": "Kitchen", "members": ["lc:desk", "bt:x", "ap:kitchen"]},
                {"id": "z1", "name": "Again", "members": ["ap:other"]},
                {"id": "", "name": "No ID", "members": ["ap:third"]},
                {"id": "z4", "volume": "loud"},
                7
            ]
        }"#;
        let (read, _) = Settings::from_json(text).expect("read");
        let shape: Vec<(&str, Vec<&str>, f64)> = read
            .speaker_zones
            .iter()
            .map(|zone| {
                (
                    zone.id.as_str(),
                    zone.members.iter().map(String::as_str).collect(),
                    zone.volume,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("z1", vec!["cc:attic", "lc:desk"], 100.0),
                ("z2", vec![], 0.0),
                ("z3", vec!["ap:kitchen"], 100.0),
                ("z4", vec![], 100.0),
            ]
        );
        assert!(Settings::default().speaker_zones_migrated);
    }

    // covers: S251, S254
    #[test]
    fn a_v3_settings_file_round_trips_presets() {
        let text = r#"{"version": 3, "speaker_zones_migrated": true, "speaker_presets_migrated": true,
            "speaker_presets": [
                {"id": "p1", "name": "Evening",
                 "members": [{"key": "lc:desk", "volume": 140}, {"key": "cc:den", "volume": null},
                             {"key": "cc:attic", "volume": 20}, {"key": "lc:desk", "volume": 5},
                             {"key": "bt:x", "volume": 5}],
                 "zones": [{"id": "z1", "volume": 40}, {"id": "z1", "volume": 90}],
                 "master_volume": 80, "gains_db": [0.3, 0, 0, 0, 0, 0, 0, 0, 0, 13]},
                {"id": "p1", "name": "Again"},
                {"id": "p2", "name": "Short", "gains_db": [1, 2]},
                {"id": "", "name": "No ID"},
                7
            ]
        }"#;
        let (read, _) = Settings::from_json(text).expect("read");
        assert_eq!(
            read.speaker_presets,
            [SavedPreset {
                id: "p1".to_owned(),
                name: "Evening".to_owned(),
                members: vec![
                    SavedPresetMember {
                        key: "lc:desk".to_owned(),
                        volume: Some(100.0),
                    },
                    SavedPresetMember {
                        key: "cc:den".to_owned(),
                        volume: None,
                    },
                ],
                zones: vec![SavedPresetZone {
                    id: "z1".to_owned(),
                    volume: 40.0,
                }],
                master_volume: 80.0,
                gains_db: [0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 12.0],
            }]
        );
        let (again, _) = Settings::from_json(&read.to_json()).expect("read");
        assert_eq!(again, read);

        // A format 3 file written before presets: its groups that lost a
        // member to the zones become presets once, in the same save.
        let before = r#"{"version": 3, "speaker_zones_migrated": true,
            "master_volume": 70,
            "outputs": {"desk": {"enabled": true, "volume": 30, "delay_ms": 0}},
            "airplay": {"kitchen": {"enabled": false, "volume": 55, "delay_ms": 0}},
            "equalizer": {"enabled": true, "preamp_db": 6, "gains_db": [1, 1, 1, 1, 1, 1, 1, 1, 1, 1]},
            "groups": [
                {"id": "g1", "name": "Desk", "members": ["lc:desk", "ap:kitchen"]},
                {"id": "g2", "name": "Group 2", "members": ["ap:kitchen", "lc:usb"]}
            ],
            "speaker_zones": [
                {"id": "g1", "name": "Desk", "members": ["lc:desk", "ap:kitchen"], "volume": 100},
                {"id": "g2", "name": "Group 2", "members": ["lc:usb"], "volume": 60}
            ]
        }"#;
        let (migrated, _) = Settings::from_json(before).expect("read");
        assert!(migrated.speaker_presets_migrated);
        let preset = &migrated.speaker_presets;
        assert_eq!(preset.len(), 1);
        assert_eq!(
            (preset[0].id.as_str(), preset[0].name.as_str()),
            ("g2", "Group 2")
        );
        assert_eq!(
            preset[0].members,
            [
                SavedPresetMember {
                    key: "ap:kitchen".to_owned(),
                    volume: Some(55.0),
                },
                SavedPresetMember {
                    key: "lc:usb".to_owned(),
                    volume: None,
                },
            ]
        );
        let zone_volumes: Vec<(&str, f64)> = preset[0]
            .zones
            .iter()
            .map(|zone| (zone.id.as_str(), zone.volume))
            .collect();
        assert_eq!(zone_volumes, [("g1", 100.0), ("g2", 60.0)]);
        assert_eq!(
            (preset[0].master_volume, preset[0].gains_db),
            (70.0, [1.0; 10])
        );
        let json = migrated.to_json();
        assert!(json.contains("\"speaker_presets_migrated\": true"));
        let mut later = Settings::from_json(&json).expect("read").0;
        assert_eq!(later.speaker_presets, migrated.speaker_presets);
        later.groups.clear();
        later.speaker_presets.clear();
        let (again, _) = Settings::from_json(&later.to_json()).expect("read");
        assert!(
            again.speaker_presets.is_empty(),
            "migrating twice changes nothing"
        );
        assert!(Settings::default().speaker_presets_migrated);
    }

    // covers: S251
    #[test]
    fn a_preset_never_stores_the_preamp() {
        let preset = SavedPreset {
            id: "p1".to_owned(),
            members: vec![SavedPresetMember {
                key: "lc:desk".to_owned(),
                volume: Some(30.0),
            }],
            zones: vec![SavedPresetZone::default()],
            ..SavedPreset::default()
        };
        let value = serde_json::to_value(&preset).expect("a preset serializes");
        let keys = |value: &serde_json::Value| -> Vec<String> {
            let mut keys: Vec<String> = value
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect();
            keys.sort();
            keys
        };
        assert_eq!(
            keys(&value),
            [
                "gains_db",
                "id",
                "master_volume",
                "members",
                "name",
                "zones"
            ]
        );
        assert_eq!(keys(&value["members"][0]), ["key", "volume"]);
        assert_eq!(keys(&value["zones"][0]), ["id", "volume"]);
    }

    #[test]
    fn equalizer_curves_are_read_leniently_and_left_out_while_none_is() {
        let text = r#"{"version": 2, "master_volume": 40, "equalizer_curves": [
            {"name": "  Desk  ", "gains_db": [1.3, "loud", null, 30, -30]},
            {"name": "desk", "gains_db": [6, 6, 6, 6, 6, 6, 6, 6, 6, 6]},
            {"name": "   ", "gains_db": []},
            {"gains_db": [1, 2]},
            {"name": "Late", "gains_db": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]},
            7
        ]}"#;
        let (read, _) = Settings::from_json(text).expect("one odd entry spoils nothing");
        assert_eq!(read.master_volume, 40.0);
        let mut desk = vec![0.0; BAND_COUNT];
        desk[..5].copy_from_slice(&[1.5, 0.0, 0.0, 12.0, -12.0]);
        assert_eq!(
            read.equalizer_curves,
            [
                SavedCurve {
                    name: "Desk".into(),
                    gains_db: desk,
                },
                SavedCurve {
                    name: "Late".into(),
                    gains_db: vec![0.0; BAND_COUNT],
                },
            ]
        );
        let (again, _) = Settings::from_json(&read.to_json()).expect("read");
        assert_eq!(again.equalizer_curves, read.equalizer_curves);
        let (read, _) =
            Settings::from_json(r#"{"version": 2, "equalizer_curves": {}}"#).expect("read");
        assert!(read.equalizer_curves.is_empty());
        assert!(
            !Settings::default().to_json().contains("equalizer_curves"),
            "no curves, no field"
        );
    }

    #[test]
    fn every_field_moves_each_field_from_its_default() {
        let default = Settings::default();
        // No `..`: a new field breaks this until it is checked here too.
        let Settings {
            version: _,
            source,
            input_device,
            input_pairs,
            source_mix,
            master_volume,
            outputs,
            airplay,
            auto_connect,
            speaker_names,
            hidden_speakers,
            groups,
            speaker_zones,
            speaker_zones_migrated: _,
            speaker_presets,
            speaker_presets_migrated: _,
            last_zone_id,
            last_preset_id,
            equalizer,
            equalizer_curves,
            show_virtual_devices,
            appearance,
            window,
            airplay_adjustment_ms,
            when_source_quits,
            remote,
            cast_addresses,
            airplay2_experimental,
            check_updates_automatically,
            last_update_check,
            update_record,
        } = every_field();
        assert_ne!(source, default.source);
        assert_ne!(input_device, default.input_device);
        assert_ne!(input_pairs, default.input_pairs);
        assert_ne!(source_mix, default.source_mix);
        assert_ne!(master_volume, default.master_volume);
        assert_ne!(outputs, default.outputs);
        assert_ne!(airplay, default.airplay);
        assert_ne!(auto_connect, default.auto_connect);
        assert_ne!(speaker_names, default.speaker_names);
        assert_ne!(hidden_speakers, default.hidden_speakers);
        assert_ne!(groups, default.groups);
        assert_ne!(speaker_zones, default.speaker_zones);
        assert_ne!(speaker_presets, default.speaker_presets);
        assert_ne!(last_zone_id, default.last_zone_id);
        assert_ne!(last_preset_id, default.last_preset_id);
        assert_ne!(equalizer, default.equalizer);
        assert_ne!(equalizer_curves, default.equalizer_curves);
        assert_ne!(show_virtual_devices, default.show_virtual_devices);
        assert_ne!(appearance, default.appearance);
        assert_ne!(window, default.window);
        assert_ne!(airplay_adjustment_ms, default.airplay_adjustment_ms);
        assert_ne!(when_source_quits, default.when_source_quits);
        assert_ne!(remote, default.remote);
        assert_ne!(cast_addresses, default.cast_addresses);
        assert_ne!(airplay2_experimental, default.airplay2_experimental);
        assert_ne!(
            check_updates_automatically,
            default.check_updates_automatically
        );
        assert_ne!(last_update_check, default.last_update_check);
        assert_ne!(update_record, default.update_record);
    }

    // covers: S212
    #[test]
    fn settings_survive_a_save_and_a_load() {
        let file = SettingsFile::at(test_dir("settings-roundtrip").join(FILE_NAME));
        assert_eq!(file.load(), (Settings::default(), Loaded::NoFile));
        file.save(&every_field()).expect("saved");
        let (loaded, found) = file.load();
        assert_eq!(loaded, every_field());
        assert_eq!(
            found,
            Loaded::Read {
                version: VERSION,
                adjusted: false,
                unreadable: Vec::new(),
            }
        );
    }

    #[test]
    fn cast_addresses_are_kept_normalized_and_once() {
        let text = r#"{
            "version": 2,
            "cast_addresses": ["Speaker.Local", "speaker.local:8009", "bad address", "192.0.2.10:8010"]
        }"#;
        let (settings, _) = Settings::from_json(text).expect("reads");
        assert_eq!(
            settings.cast_addresses,
            ["speaker.local", "192.0.2.10:8010"]
        );
        assert!(Settings::default().cast_addresses.is_empty());
    }

    #[test]
    fn a_field_that_does_not_read_is_named_and_falls_back_alone() {
        let (read, found) = Settings::from_json(
            r#"{"version": 3, "master_volume": "loud", "show_virtual_devices": true}"#,
        )
        .expect("read");
        assert_eq!(read.master_volume, Settings::default().master_volume);
        assert!(read.show_virtual_devices);
        assert!(
            matches!(&found, Loaded::Read { unreadable, .. } if unreadable == &["master_volume"]),
            "{found:?}"
        );
        let (_, clean) = Settings::from_json(&sample().to_json()).expect("read");
        assert!(
            matches!(&clean, Loaded::Read { unreadable, .. } if unreadable.is_empty()),
            "{clean:?}"
        );
    }

    #[test]
    fn every_stored_field_is_named_when_it_does_not_read() {
        // The fields left out of the file while empty are set too, so each
        // one is checked.
        let every_field = every_field();
        let serde_json::Value::Object(stored) =
            serde_json::from_str(&every_field.to_json()).expect("an object")
        else {
            panic!("the settings are an object");
        };
        let optional = [
            "input_device",
            "input_pairs",
            "source_mix",
            "hidden_speakers",
            "equalizer_curves",
            "update_record",
        ];
        for name in optional {
            assert!(stored.contains_key(name), "{name} is stored");
        }
        let marks = [
            "version",
            "speaker_zones_migrated",
            "speaker_presets_migrated",
        ];
        for name in stored.keys().filter(|name| !marks.contains(&name.as_str())) {
            let named = ["\"x\"", "7.5"].iter().any(|wrong| {
                let text = format!(r#"{{"version": 3, "{name}": {wrong}}}"#);
                unreadable_fields(&text) == [name.as_str()]
            });
            assert!(named, "{name} is never named as unreadable");
        }
    }

    #[test]
    fn the_web_remote_is_off_by_default_and_keeps_a_usable_port() {
        let read = |json: &str| Settings::from_json(json).expect("read");
        let (older, found) = read(r#"{"version": 2}"#);
        assert_eq!(older.remote, RemoteSettings::default());
        assert!(!older.remote.on);
        assert!(matches!(
            found,
            Loaded::Read {
                adjusted: false,
                ..
            }
        ));
        let (on, _) = read(r#"{"version": 2, "remote": {"on": true, "port": 9000}}"#);
        assert_eq!(
            on.remote,
            RemoteSettings {
                on: true,
                port: 9000
            }
        );
        let (low, found) = read(r#"{"version": 2, "remote": {"on": true, "port": 80}}"#);
        assert_eq!(low.remote.port, audiosync_remote::DEFAULT_PORT);
        assert!(matches!(found, Loaded::Read { adjusted: true, .. }));
    }

    #[test]
    fn airplay2_is_off_in_a_file_that_does_not_name_it() {
        let read = |json: &str| Settings::from_json(json).expect("read").0;
        assert!(!Settings::default().airplay2_experimental);
        assert!(!read(r#"{"version": 2}"#).airplay2_experimental);
        let on = read(r#"{"version": 2, "airplay2_experimental": true}"#);
        assert!(on.airplay2_experimental);
        assert!(read(&on.to_json()).airplay2_experimental);
    }

    #[test]
    fn update_checks_are_on_by_default_and_keep_their_time() {
        let read = |json: &str| Settings::from_json(json).expect("read").0;
        assert!(Settings::default().check_updates_automatically);
        assert!(read(r#"{"version": 2}"#).check_updates_automatically);
        let off = read(
            r#"{"version": 2, "check_updates_automatically": false, "last_update_check": 1800000000}"#,
        );
        assert!(!off.check_updates_automatically);
        let again = read(&off.to_json());
        assert_eq!(again.last_update_check, Some(1_800_000_000));
        assert!(!again.check_updates_automatically);
        assert_eq!(again.update_record, None);

        // covers: S316
        let blocked =
            read(r#"{"version": 2, "update_record": {"version": "0.9.0", "reason": "blocked"}}"#);
        let record = Some(crate::ui_model::UpdateRecord {
            version: "0.9.0".to_owned(),
            reason: crate::ui_model::UpdateRecordReason::Blocked,
        });
        assert_eq!(blocked.update_record, record);
        assert_eq!(read(&blocked.to_json()).update_record, record);
        let odd = read(r#"{"version": 2, "update_record": {"version": 9}}"#);
        assert_eq!(
            odd.update_record, None,
            "a record that doesn't read is none"
        );
    }

    // covers: S155 (the stored value; aqua and darkAqua are the Mac's)
    #[test]
    fn an_appearance_this_build_does_not_know_follows_windows() {
        let read = |json: &str| Settings::from_json(json).expect("read").0;
        assert_eq!(read(r#"{"version": 2}"#).appearance, Appearance::System);
        assert_eq!(
            read(r#"{"version": 2, "appearance": "light"}"#).appearance,
            Appearance::Light
        );
        assert_eq!(
            read(r#"{"version": 2, "appearance": "dark"}"#).appearance,
            Appearance::Dark
        );
        for unknown in [r#""sepia""#, "7", "null", r#"["dark"]"#] {
            let settings = read(&format!(
                r#"{{"version": 2, "master_volume": 40, "appearance": {unknown}}}"#
            ));
            assert_eq!(settings.appearance, Appearance::System, "{unknown}");
            assert_eq!(settings.master_volume, 40.0, "{unknown}: the rest is kept");
        }
        let json = Settings {
            appearance: Appearance::Light,
            ..Settings::default()
        }
        .to_json();
        assert!(json.contains(r#""appearance": "light""#), "{json}");
    }

    #[test]
    fn a_corrupted_file_is_ignored_and_replaced_by_the_next_save() {
        let path = test_dir("settings-corrupted").join(FILE_NAME);
        let file = SettingsFile::at(path.clone());
        for contents in [
            &b"{\"version\": 1, \"master_volume\": "[..],
            b"not json at all",
            b"[1, 2, 3]",
            b"\xff\xfe\x00garbage",
            b"",
        ] {
            fs::write(&path, contents).expect("written");
            let (loaded, found) = file.load();
            assert_eq!(loaded, Settings::default(), "{contents:?}");
            assert!(matches!(found, Loaded::Ignored(_)), "{contents:?}: {found}");
        }
        file.save(&sample()).expect("saved over it");
        assert_eq!(file.load().0, sample());
    }

    #[test]
    fn a_newer_version_is_ignored_and_an_older_one_is_read() {
        let path = test_dir("settings-versions").join(FILE_NAME);
        let file = SettingsFile::at(path.clone());
        let mut newer = sample();
        newer.version = VERSION + 1;
        fs::write(&path, newer.to_json()).expect("written");
        let (loaded, found) = file.load();
        assert_eq!(loaded, Settings::default());
        assert!(
            found.to_string().contains("a newer AudioSync wrote it"),
            "{found}"
        );

        // A file without a version, from before versions, with fields this
        // build doesn't know and one it does.
        fs::write(
            &path,
            r#"{"master_volume": 40, "speaker_layout": "grid", "outputs": {"x": {"volume": 10}}}"#,
        )
        .expect("written");
        let (loaded, found) = file.load();
        assert_eq!(
            found,
            Loaded::Read {
                version: 0,
                adjusted: false,
                unreadable: Vec::new(),
            }
        );
        assert!(found.to_string().contains("older than"), "{found}");
        assert_eq!(loaded.version, VERSION);
        assert_eq!(loaded.master_volume, 40.0);
        assert_eq!(
            loaded.outputs.get("x"),
            Some(&SavedSpeaker {
                enabled: false,
                volume: 10.0,
                delay_ms: 0.0
            })
        );
        assert_eq!(loaded.source, SavedSource::System);
    }

    #[test]
    fn a_mix_of_apps_survives_a_round_trip_and_an_old_single_app_source_loads_unchanged() {
        let app = |executable: &str| SavedMixApp {
            executable: executable.to_owned(),
            name: String::new(),
        };
        let chosen = Settings {
            source_mix: Some(SavedMix {
                mode: MixMode::AllExcept,
                apps: vec![app("spotify.exe"), app("chrome.exe")],
            }),
            ..Settings::default()
        };
        let json: serde_json::Value = serde_json::from_str(&chosen.to_json()).expect("JSON");
        assert_eq!(json["source_mix"]["mode"], "all_except");
        assert_eq!(
            json["source"]["kind"], "system",
            "what an older build reads"
        );
        let (loaded, _) = Settings::from_json(&chosen.to_json()).expect("read");
        assert_eq!(loaded.source_mix, chosen.source_mix);

        let (old, _) = Settings::from_json(
            r#"{"version": 2, "source": {"kind": "app", "executable": "spotify.exe", "name": "Spotify"}}"#,
        )
        .expect("read");
        assert_eq!(
            old.source,
            SavedSource::App {
                executable: "spotify.exe".to_owned(),
                name: "Spotify".to_owned()
            }
        );
        assert_eq!(old.source_mix, None);

        // Repeats (in any case) and unusable names go; an empty mix is none.
        let messy = Settings {
            source_mix: Some(SavedMix {
                mode: MixMode::Only,
                apps: vec![app("a.exe"), app("A.EXE"), app("")],
            }),
            ..Settings::default()
        }
        .clamped();
        assert_eq!(
            messy.source_mix.map(|mix| mix.apps),
            Some(vec![app("a.exe")])
        );
        let empty = Settings {
            source_mix: Some(SavedMix::default()),
            ..Settings::default()
        }
        .clamped();
        assert_eq!(empty.source_mix, None);
    }

    #[test]
    fn an_input_device_choice_survives_a_round_trip_and_older_builds_read_the_whole_system() {
        let path = test_dir("settings-input").join(FILE_NAME);
        let file = SettingsFile::at(path.clone());
        let chosen = Settings {
            input_device: Some(SavedInput {
                id: "{0.0.1.00000000}.{mic}".to_owned(),
                name: "Microphone (USB Audio)".to_owned(),
            }),
            ..sample()
        };
        file.save(&chosen).expect("saved");
        assert_eq!(file.load().0, chosen);
        let text = fs::read_to_string(&path).expect("read");
        let json: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(json["input_device"]["id"], "{0.0.1.00000000}.{mic}");

        // What an older build reads: it ignores the field it doesn't know,
        // and the source beside it is the whole system.
        let written = Settings {
            input_device: Some(SavedInput {
                id: "mic".to_owned(),
                name: String::new(),
            }),
            ..Settings::default()
        };
        let json: serde_json::Value = serde_json::from_str(&written.to_json()).expect("JSON");
        assert_eq!(json["source"]["kind"], "system");

        // A file from before input devices has none, and one without a
        // usable ID drops it.
        let (loaded, _) =
            Settings::from_json(r#"{"version": 2, "source": {"kind": "system"}}"#).expect("read");
        assert_eq!(loaded.input_device, None);
        let (loaded, found) =
            Settings::from_json(r#"{"version": 2, "input_device": {"id": "", "name": "x"}}"#)
                .expect("read");
        assert_eq!(loaded.input_device, None);
        assert_eq!(
            found,
            Loaded::Read {
                version: 2,
                adjusted: true,
                unreadable: Vec::new(),
            }
        );
        assert!(!Settings::default().to_json().contains("input_device"));
    }

    #[test]
    fn input_channel_pairs_survive_a_round_trip_and_odd_entries_are_skipped() {
        let path = test_dir("settings-input-pairs").join(FILE_NAME);
        let file = SettingsFile::at(path.clone());
        let chosen = Settings {
            input_pairs: BTreeMap::from([
                ("{0.0.1.00000000}.{interface}".to_owned(), 1),
                ("{0.0.1.00000000}.{mixer}".to_owned(), 3),
            ]),
            ..sample()
        };
        file.save(&chosen).expect("saved");
        assert_eq!(file.load().0, chosen);
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("JSON");
        assert_eq!(json["input_pairs"]["{0.0.1.00000000}.{interface}"], 1);

        let text = r#"{"version": 2, "master_volume": 40, "input_pairs": {
            "kept": 2, "first": 0, "negative": -1, "text": "3", "half": 1.5,
            "beyond": 512, "": 1
        }}"#;
        let (read, found) = Settings::from_json(text).expect("one odd entry spoils nothing");
        assert_eq!(read.master_volume, 40.0);
        assert_eq!(read.input_pairs, BTreeMap::from([("kept".to_owned(), 2)]));
        assert_eq!(
            found,
            Loaded::Read {
                version: 2,
                adjusted: true,
                unreadable: Vec::new(),
            }
        );
        let (read, _) = Settings::from_json(r#"{"version": 2, "input_pairs": [1]}"#).expect("read");
        assert!(read.input_pairs.is_empty());
        assert!(!Settings::default().to_json().contains("input_pairs"));
    }

    // covers: S212 (the master, volume and delay clamps)
    #[test]
    fn values_out_of_range_are_clamped() {
        let text = r#"{
            "version": 1,
            "source": {"kind": "app", "executable": ""},
            "master_volume": 250,
            "outputs": {
                "loud": {"enabled": true, "volume": 1000, "delay_ms": 99999},
                "": {"enabled": true},
                "quiet": {"volume": -5, "delay_ms": -20}
            },
            "airplay": {
                "far": {"delay_ms": 25000, "volume": 158.73},
                "near": {"delay_ms": 13000},
                "low": {"volume": -5}
            },
            "auto_connect": ["lc:loud", "nonsense", "ap:", "zz:loud"],
            "groups": [
                {"id": "group:1", "name": "Empty", "members": ["bad"]},
                {"id": "group:2", "name": "Ok", "members": ["lc:loud", "x"]},
                {"id": "group:2", "name": "Twice", "members": ["lc:loud"]}
            ],
            "equalizer": {"enabled": true, "preamp_db": 40, "gains_db": [30, -30, 1.2]},
            "window": {"x": 99999999, "y": -99999999, "width": 10, "height": 99999}
        }"#;
        let (settings, found) = Settings::from_json(text).expect("read");
        assert_eq!(
            found,
            Loaded::Read {
                version: 1,
                adjusted: true,
                unreadable: Vec::new(),
            }
        );
        assert_eq!(settings.source, SavedSource::System);
        assert_eq!(settings.master_volume, 100.0);
        assert_eq!(
            settings.outputs.get("loud"),
            Some(&SavedSpeaker {
                enabled: true,
                volume: 100.0,
                delay_ms: MAX_DELAY_MS
            })
        );
        assert_eq!(
            settings.outputs.get("quiet"),
            Some(&SavedSpeaker {
                enabled: false,
                volume: 0.0,
                delay_ms: 0.0
            })
        );
        assert!(!settings.outputs.contains_key(""));
        assert_eq!(settings.airplay["far"].delay_ms, 20_000.0);
        assert_eq!(
            settings.airplay["near"].delay_ms, 13_000.0,
            "a calibrated delay stays"
        );
        // An AirPlay own volume may pass 100 (S290).
        assert_eq!(settings.airplay["far"].volume, 158.73);
        assert_eq!(settings.airplay["low"].volume, 0.0);
        assert_eq!(
            settings.auto_connect,
            BTreeSet::from(["lc:loud".to_owned()])
        );
        assert_eq!(
            settings.groups,
            vec![SavedGroup {
                id: "group:2".to_owned(),
                name: "Ok".to_owned(),
                members: vec!["lc:loud".to_owned()],
            }]
        );
        let eq = settings.equalizer.settings();
        assert_eq!(eq.preamp_db, 12.0);
        assert_eq!(&eq.gains_db[..4], &[12.0, -12.0, 1.0, 0.0]);
        assert_eq!(settings.equalizer.gains_db.len(), BAND_COUNT);
        assert_eq!(
            settings.window,
            Some(WindowFrame {
                x: MAX_POSITION,
                y: -MAX_POSITION,
                width: MIN_WINDOW.0,
                height: MAX_WINDOW.1,
                maximized: false,
            })
        );
    }

    // covers: S212 (a value that isn't a number costs only that value)
    #[test]
    fn a_master_or_speaker_value_that_isnt_a_number_is_dropped_and_the_rest_reads() {
        for master in [r#""x""#, "null", "[]"] {
            let text = format!(
                r#"{{
                    "version": 2,
                    "master_volume": {master},
                    "outputs": {{"desk": {{"enabled": true, "volume": "x", "delay_ms": null}}}},
                    "airplay": {{"far": {{"volume": "loud", "delay_ms": "slow"}}}},
                    "auto_connect": ["lc:desk"],
                    "airplay_adjustment_ms": {master}
                }}"#
            );
            let (settings, _) = Settings::from_json(&text).expect("one odd value spoils nothing");
            assert_eq!(settings.master_volume, 100.0, "master {master}");
            assert_eq!(settings.airplay_adjustment_ms, 0.0, "adjustment {master}");
            assert_eq!(
                settings.outputs.get("desk"),
                Some(&SavedSpeaker {
                    enabled: true,
                    ..SavedSpeaker::default()
                })
            );
            assert_eq!(settings.airplay["far"], SavedSpeaker::default());
            assert_eq!(
                settings.auto_connect,
                BTreeSet::from(["lc:desk".to_owned()])
            );
        }
    }

    #[test]
    fn offsets_and_the_airplay_adjustment_are_kept_in_their_ranges() {
        let text = r#"{
            "version": 2,
            "outputs": {
                "late": {"delay_ms": -250},
                "far": {"delay_ms": -9000},
                "high": {"delay_ms": 99999}
            },
            "airplay_adjustment_ms": -7000
        }"#;
        let (settings, found) = Settings::from_json(text).expect("read");
        assert_eq!(
            found,
            Loaded::Read {
                version: 2,
                adjusted: true,
                unreadable: Vec::new(),
            }
        );
        assert_eq!(settings.outputs["late"].delay_ms, -250.0);
        assert_eq!(settings.outputs["far"].delay_ms, -2000.0);
        assert_eq!(settings.outputs["high"].delay_ms, 20_000.0);
        assert_eq!(settings.airplay_adjustment_ms, -2000.0);
        let (settings, _) =
            Settings::from_json(r#"{"version": 2, "airplay_adjustment_ms": 250}"#).expect("read");
        assert_eq!(settings.airplay_adjustment_ms, 250.0);
        assert_eq!(Settings::default().airplay_adjustment_ms, 0.0);
    }

    /// A format 1 file's outputs and AirPlay speaker: the PC's speakers
    /// (the reference) with a trim of 250 ms, a Bluetooth speaker at 0, a
    /// TV at 120, a far speaker at 3000 and Kitchen with its own delay.
    fn trims() -> Settings {
        let mut settings = Settings {
            version: 1,
            ..Settings::default()
        };
        for (id, delay_ms) in [
            ("builtin", 250.0),
            ("bluetooth", 0.0),
            ("tv", 120.0),
            ("far", 3000.0),
        ] {
            settings.outputs.insert(
                id.to_owned(),
                SavedSpeaker {
                    enabled: true,
                    delay_ms,
                    ..SavedSpeaker::default()
                },
            );
        }
        settings.airplay.insert(
            "kitchen".to_owned(),
            SavedSpeaker {
                enabled: true,
                delay_ms: 300.0,
                ..SavedSpeaker::default()
            },
        );
        settings
    }

    // covers: S180
    #[test]
    fn old_trims_become_offsets_relative_to_the_reference() {
        let mut settings = trims();
        migrate_delays(&mut settings, Some("builtin"));
        let offsets: Vec<(&str, f64)> = settings
            .outputs
            .iter()
            .map(|(id, speaker)| (id.as_str(), speaker.delay_ms))
            .collect();
        assert_eq!(
            offsets,
            [
                ("bluetooth", -250.0),
                ("builtin", 0.0),
                ("far", 2750.0),
                ("tv", -130.0)
            ]
        );
        assert!(settings.outputs["builtin"].enabled, "only its delay goes");
        // Without a reference every offset is the old trim, and A is 0.
        let mut settings = trims();
        migrate_delays(&mut settings, None);
        assert_eq!(settings.outputs["builtin"].delay_ms, 250.0);
        assert_eq!(settings.outputs["tv"].delay_ms, 120.0);
        assert_eq!(settings.airplay_adjustment_ms, 0.0);
        // An offset below −2000 ms is clamped.
        let mut settings = trims();
        settings.outputs.get_mut("builtin").unwrap().delay_ms = 2600.0;
        migrate_delays(&mut settings, Some("builtin"));
        assert_eq!(settings.outputs["bluetooth"].delay_ms, -2000.0);
    }

    // covers: S180
    #[test]
    fn the_references_old_trim_becomes_the_airplay_adjustment_and_airplay_delays_stay() {
        let mut settings = trims();
        migrate_delays(&mut settings, Some("builtin"));
        assert_eq!(settings.airplay_adjustment_ms, 250.0);
        assert_eq!(settings.airplay, trims().airplay);
        // A reference that has no stored trim: A is 0.
        let mut settings = trims();
        migrate_delays(&mut settings, Some("hdmi"));
        assert_eq!(settings.airplay_adjustment_ms, 0.0);
        assert_eq!(settings.outputs["builtin"].delay_ms, 250.0);
    }

    // covers: S180
    #[test]
    fn a_late_airplay_speaker_lined_up_by_the_references_trim_stays_lined_up() {
        use audiosync_core::policy::delay::{Participant, hold_backs, native_airplay_latency_ms};
        let mut settings = trims();
        migrate_delays(&mut settings, Some("builtin"));
        let taking_part: Vec<Participant> = ["builtin", "bluetooth", "tv"]
            .into_iter()
            .map(|id| Participant {
                id,
                offset_ms: settings.outputs[id].delay_ms,
                is_reference: id == "builtin",
            })
            .collect();
        let held = |airplay: Option<f64>| -> Vec<f64> {
            hold_backs(&taking_part, airplay)
                .into_iter()
                .map(|(_, held)| held)
                .collect()
        };
        // Before, without AirPlay: each output at its own trim.
        assert_eq!(held(None), [250.0, 0.0, 120.0]);
        // Before, with AirPlay (L_s = 1750 ms): L_s + trim, and Kitchen at
        // L_s + 0. Now L = L_s + A, and the window shows 2000.
        let latency = native_airplay_latency_ms(1750.0, settings.airplay_adjustment_ms);
        assert_eq!(latency, 2000.0);
        assert_eq!(held(Some(latency)), [2000.0, 1750.0, 1870.0]);
    }

    #[test]
    fn a_huge_file_is_cut_to_the_limits() {
        let mut settings = Settings::default();
        for n in 0..(MAX_ENTRIES + 10) {
            settings
                .outputs
                .insert(format!("output {n:04}"), SavedSpeaker::default());
        }
        settings.source = SavedSource::App {
            executable: "app.exe".to_owned(),
            name: "é".repeat(MAX_TEXT),
        };
        let clamped = settings.clamped();
        assert_eq!(clamped.outputs.len(), MAX_ENTRIES);
        let SavedSource::App { name, .. } = clamped.source else {
            panic!("still an app");
        };
        assert!(name.len() <= MAX_TEXT && name.chars().all(|c| c == 'é'));
    }

    #[test]
    fn without_a_file_nothing_is_written() {
        let file = SettingsFile::in_memory();
        file.save(&sample()).expect("nothing to do");
        assert_eq!(file.load(), (Settings::default(), Loaded::NoFile));
        assert!(file.path().is_none());
    }

    #[test]
    fn a_save_leaves_no_temporary_file_and_a_stale_one_is_harmless() {
        let path = test_dir("settings-atomic").join(FILE_NAME);
        let file = SettingsFile::at(path.clone());
        file.save(&sample()).expect("saved");
        assert!(!data_folder::temporary_path(&path).exists());

        // A half-written temporary file from a crash: the settings file is
        // whole, and the next save replaces the leftover.
        fs::write(
            data_folder::temporary_path(&path),
            b"{\"version\": 1, \"mas",
        )
        .expect("w");
        assert_eq!(file.load().0, sample());
        file.save(&Settings::default()).expect("saved");
        assert!(!data_folder::temporary_path(&path).exists());
        assert_eq!(file.load().0, Settings::default());
    }

    #[test]
    fn a_save_that_fails_keeps_the_last_file_whole() {
        let path = test_dir("settings-failed-save").join(FILE_NAME);
        let file = SettingsFile::at(path.clone());
        file.save(&sample()).expect("saved");
        fs::create_dir(data_folder::temporary_path(&path)).expect("blocked");
        assert!(file.save(&Settings::default()).is_err());
        assert_eq!(file.load().0, sample());
    }

    #[test]
    fn a_maximized_window_is_kept_with_its_last_normal_frame() {
        let window = r#""window": {"x": 10, "y": 20, "width": 1000.0, "height": 700.0"#;
        let text = format!(r#"{{"version": 1, {window}, "maximized": true}}}}"#);
        let (read, _) = Settings::from_json(&text).expect("read");
        let written = read.to_json();
        assert!(written.contains(r#""maximized": true"#), "{written}");
        assert!(written.contains(r#""width": 1000.0"#), "{written}");
        // A file from before the flag reads as not maximized.
        let text = format!(r#"{{"version": 1, {window}}}}}"#);
        let (read, _) = Settings::from_json(&text).expect("read");
        assert!(read.to_json().contains(r#""maximized": false"#));
    }

    #[test]
    fn a_newer_file_is_kept_aside_before_the_first_save_over_it() {
        let folder = test_dir("settings-newer-kept");
        let path = folder.join(FILE_NAME);
        let mut newer = sample();
        newer.version = VERSION + 1;
        let newer_text = newer.to_json();
        fs::write(&path, &newer_text).expect("written");
        let file = SettingsFile::at(path.clone());
        assert!(matches!(file.load().1, Loaded::Ignored(_)));

        file.save(&Settings::default()).expect("saved");
        let kept = || copies_kept_aside(&folder, "settings.newer");
        assert_eq!(kept(), [newer_text.clone().into_bytes()]);
        assert_eq!(file.load().0, Settings::default());

        // The file is this build's now: the next save leaves the copy alone.
        file.save(&sample()).expect("saved again");
        assert_eq!(kept(), [newer_text.clone().into_bytes()]);

        // The newer AudioSync saved over it again, starting from what this
        // build wrote: that file gets a copy of its own, and the first copy
        // stays.
        let again_text = Settings {
            version: VERSION + 1,
            ..Settings::default()
        }
        .to_json();
        fs::write(&path, &again_text).expect("written");
        let file = SettingsFile::at(path.clone());
        assert!(matches!(file.load().1, Loaded::Ignored(_)));
        file.save(&sample()).expect("saved");
        let mut both = kept();
        both.sort();
        let mut expected = vec![newer_text.into_bytes(), again_text.into_bytes()];
        expected.sort();
        assert_eq!(both, expected);
    }

    #[test]
    fn a_file_that_cant_be_read_at_launch_is_never_saved_over() {
        let folder = test_dir("settings-unreadable");
        let path = folder.join(FILE_NAME);
        let kept = || copies_kept_aside(&folder, "settings.ignored");
        // A folder where the file should be: reading it fails, but not with
        // "not found", as when another program holds the file at launch.
        fs::create_dir(&path).expect("the folder is made");
        let file = SettingsFile::at(path.clone());
        let (loaded, found) = file.load();
        assert_eq!(loaded, Settings::default());
        assert!(matches!(found, Loaded::Ignored(_)), "{found}");
        // Still unreadable: the save is refused, nothing replaced.
        assert!(file.save(&Settings::default()).is_err());
        assert!(path.is_dir());

        // Readable again, holding the user's settings: the save leaves the
        // file alone and returns them read, and keeps nothing aside.
        fs::remove_dir(&path).expect("the folder is removed");
        let good = sample().to_json();
        fs::write(&path, &good).expect("written");
        let read = Settings::from_json(&good).expect("settings");
        assert_eq!(
            file.save(&Settings::default()).expect("not saved"),
            Some(Retried::Read(Box::new(read)))
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), good);
        // Once read, the next save replaces the file.
        assert_eq!(file.save(&Settings::default()).expect("saved"), None);
        assert_eq!(file.load().0, Settings::default());
        assert!(kept().is_empty());
    }

    #[test]
    fn settings_unreadable_at_two_launches_in_a_row_are_never_lost() {
        let folder = test_dir("settings-unreadable-twice");
        let path = folder.join(FILE_NAME);
        let users = sample().to_json();
        fs::write(&path, &users).expect("written");
        // A launch while another program holds the file (a folder stands in
        // its place), which lets go of it before the first save.
        let launch = |settings: &Settings| {
            let held = folder.join("held");
            fs::rename(&path, &held).expect("moved");
            fs::create_dir(&path).expect("the folder is made");
            let file = SettingsFile::at(path.clone());
            let (loaded, found) = file.load();
            assert_eq!(loaded, Settings::default());
            assert!(matches!(found, Loaded::Ignored(_)), "{found}");
            fs::remove_dir(&path).expect("the folder is removed");
            fs::rename(&held, &path).expect("moved back");
            let read = file.save(settings).expect("not saved");
            assert!(matches!(read, Some(Retried::Read(_))), "{read:?}");
        };
        let mut changed = Settings {
            master_volume: 12.0,
            ..Settings::default()
        };
        launch(&changed);
        changed.master_volume = 13.0;
        launch(&changed);
        // The user's settings are still the file, and nothing is kept aside.
        assert_eq!(fs::read_to_string(&path).expect("read"), users);
        assert!(copies_kept_aside(&folder, "settings.ignored").is_empty());
    }

    #[test]
    fn a_damaged_file_is_kept_aside_before_the_first_save_over_it() {
        let folder = test_dir("settings-damaged-kept");
        let path = folder.join(FILE_NAME);
        fs::write(&path, b"{\"version\": 2, \"master_volu").expect("written");
        let file = SettingsFile::at(path.clone());
        assert!(matches!(file.load().1, Loaded::Ignored(_)));
        file.save(&sample()).expect("saved");
        assert_eq!(
            copies_kept_aside(&folder, "settings.ignored"),
            [b"{\"version\": 2, \"master_volu".to_vec()]
        );
        assert_eq!(file.load().0, sample());
    }

    #[cfg(windows)]
    #[test]
    fn a_file_another_program_holds_for_a_moment_at_launch_is_read_at_the_second_try() {
        use std::os::windows::fs::OpenOptionsExt;
        let path = test_dir("settings-held").join(FILE_NAME);
        fs::write(&path, sample().to_json()).expect("written");
        // Shared with no one, as an antivirus scan may hold it.
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("held");
        let loader = {
            let path = path.clone();
            std::thread::spawn(move || SettingsFile::at(path).load())
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(held);
        let (loaded, found) = loader.join().expect("loaded");
        assert!(matches!(found, Loaded::Read { .. }), "{found}");
        assert_eq!(loaded, sample());
    }

    #[test]
    fn a_file_this_build_reads_is_not_kept_aside() {
        let folder = test_dir("settings-not-newer");
        let path = folder.join(FILE_NAME);
        let file = SettingsFile::at(path.clone());
        file.save(&sample()).expect("saved");
        file.save(&Settings::default()).expect("saved again");
        assert!(!folder.join("settings.newer.json").exists());
    }

    /// `fields`, beside a master of 40, read; the master shows the rest of
    /// the file was kept.
    fn read_beside_a_kept_master(fields: &str) -> Settings {
        let text = format!(r#"{{"version": 3, "master_volume": 40, {fields}}}"#);
        let (settings, _) =
            Settings::from_json(&text).unwrap_or_else(|why| panic!("{fields}: {why}"));
        assert_eq!(settings.master_volume, 40.0, "{fields}");
        settings
    }

    // covers: S212 (a value that isn't a number costs only that value, an equalizer value too; unreadable groups, IDs and an equalizer that isn't an object fall back to their defaults)
    #[test]
    fn an_unreadable_value_in_any_field_costs_only_that_value() {
        let outputs = read_beside_a_kept_master(
            r#""outputs": {"desk": {"enabled": "yes", "volume": 30}, "gone": null, "odd": 7},
               "airplay": {"far": {"enabled": null, "delay_ms": 300}}"#,
        );
        assert_eq!(
            outputs.outputs,
            BTreeMap::from([(
                "desk".to_owned(),
                SavedSpeaker {
                    volume: 30.0,
                    ..SavedSpeaker::default()
                }
            )])
        );
        assert_eq!(outputs.airplay["far"].delay_ms, 300.0);
        for whole in [r#""outputs": null"#, r#""airplay": 5"#] {
            read_beside_a_kept_master(whole);
        }

        let first_band = |enabled, preamp_db| {
            let mut gains_db = vec![0.0; BAND_COUNT];
            gains_db[0] = 1.0;
            SavedEqualizer {
                enabled,
                preamp_db,
                gains_db,
            }
        };
        for (equalizer, expected) in [
            (
                r#"{"enabled": true, "preamp_db": -3, "gains_db": [1, null, "x"]}"#,
                first_band(true, -3.0),
            ),
            (
                r#"{"enabled": "on", "preamp_db": -3, "gains_db": [1]}"#,
                first_band(false, -3.0),
            ),
            (
                r#"{"enabled": true, "preamp_db": "loud", "gains_db": [1]}"#,
                first_band(true, 0.0),
            ),
            (
                r#"{"enabled": true, "preamp_db": -3, "gains_db": "flat"}"#,
                SavedEqualizer {
                    enabled: true,
                    preamp_db: -3.0,
                    ..SavedEqualizer::default()
                },
            ),
            ("null", SavedEqualizer::default()),
        ] {
            let read = read_beside_a_kept_master(&format!(r#""equalizer": {equalizer}"#));
            assert_eq!(read.equalizer, expected, "{equalizer}");
        }
        assert_eq!(
            unreadable_fields(
                r#"{"version": 3, "equalizer": {"enabled": "on", "gains_db": [1, null, "x"]}}"#
            ),
            [
                "equalizer.enabled",
                "equalizer.gains_db[1]",
                "equalizer.gains_db[2]"
            ]
        );

        for groups in [r#""none""#, "null"] {
            let read = read_beside_a_kept_master(&format!(r#""groups": {groups}"#));
            assert!(read.groups.is_empty(), "{groups}");
        }
        let groups = read_beside_a_kept_master(
            r#""groups": [{"id": "g", "name": "G", "members": ["lc:a", 5]}, 7, {"id": 8}]"#,
        );
        assert_eq!(
            groups.groups,
            [SavedGroup {
                id: "g".to_owned(),
                name: "G".to_owned(),
                members: vec!["lc:a".to_owned()],
            }]
        );
        // A format 2 file's groups become its zones once: one bad member
        // must not cost the group before the mark is set.
        let (older, _) = Settings::from_json(
            r#"{"version": 2, "groups": [{"id": "g", "name": "G", "members": ["lc:a", null]}]}"#,
        )
        .expect("read");
        assert!(older.speaker_zones_migrated);
        assert_eq!(
            older
                .speaker_zones
                .iter()
                .map(|zone| zone.members.clone())
                .collect::<Vec<_>>(),
            [vec!["lc:a".to_owned()]]
        );

        for window in [
            r#"{"x": 100.5, "y": 0, "width": 800, "height": 600}"#,
            r#"{"x": 9999999999, "y": 0, "width": 800, "height": 600}"#,
            r#"{"x": 0, "y": 0, "height": 600}"#,
            "7",
        ] {
            let read = read_beside_a_kept_master(&format!(r#""window": {window}"#));
            assert_eq!(read.window, None, "{window}");
        }

        for (remote, kept) in [
            (
                r#"{"on": true, "port": 70000}"#,
                RemoteSettings {
                    on: true,
                    ..RemoteSettings::default()
                },
            ),
            (
                r#"{"on": true, "port": -1}"#,
                RemoteSettings {
                    on: true,
                    ..RemoteSettings::default()
                },
            ),
            (
                r#"{"on": true, "port": 2048.5}"#,
                RemoteSettings {
                    on: true,
                    ..RemoteSettings::default()
                },
            ),
            (
                r#"{"on": "yes", "port": 2048}"#,
                RemoteSettings {
                    on: false,
                    port: 2048,
                },
            ),
            ("null", RemoteSettings::default()),
        ] {
            let read = read_beside_a_kept_master(&format!(r#""remote": {remote}"#));
            assert_eq!(read.remote, kept, "{remote}");
        }

        let ids = read_beside_a_kept_master(
            r#""auto_connect": ["lc:desk", 5, null], "cast_addresses": ["192.0.2.10:8010", 7]"#,
        );
        assert_eq!(ids.auto_connect, BTreeSet::from(["lc:desk".to_owned()]));
        assert_eq!(ids.cast_addresses, vec!["192.0.2.10:8010".to_owned()]);
        for whole in [r#""auto_connect": "lc:desk""#, r#""cast_addresses": null"#] {
            read_beside_a_kept_master(whole);
        }

        for source in [
            r#"{"kind": "playlist"}"#,
            r#"{"kind": "app", "executable": 5}"#,
            "null",
        ] {
            let read = read_beside_a_kept_master(&format!(r#""source": {source}"#));
            assert_eq!(read.source, SavedSource::System, "{source}");
        }
        let input = read_beside_a_kept_master(r#""input_device": {"id": 5, "name": "Mic"}"#);
        assert_eq!(input.input_device, None);
        let mix = read_beside_a_kept_master(
            r#""source_mix": {"mode": "shuffle", "apps": [{"executable": "a.exe"}]}"#,
        );
        assert_eq!(mix.source_mix, None);
        let mix = read_beside_a_kept_master(
            r#""source_mix": {"mode": "all_except", "apps": [{"executable": "a.exe"}, 5, {"executable": 6}]}"#,
        );
        assert_eq!(
            mix.source_mix,
            Some(SavedMix {
                mode: MixMode::AllExcept,
                apps: vec![SavedMixApp {
                    executable: "a.exe".to_owned(),
                    name: String::new(),
                }],
            })
        );

        let flags = read_beside_a_kept_master(
            r#""speaker_zones_migrated": "yes", "speaker_presets_migrated": null,
               "show_virtual_devices": 1, "airplay2_experimental": "on",
               "check_updates_automatically": "no", "when_source_quits": 5,
               "last_update_check": -5"#,
        );
        assert!(flags.speaker_zones_migrated);
        assert!(flags.speaker_presets_migrated);
        assert!(!flags.show_virtual_devices);
        assert!(!flags.airplay2_experimental);
        assert!(flags.check_updates_automatically);
        assert_eq!(
            flags.when_source_quits,
            Settings::default().when_source_quits
        );
        assert_eq!(flags.last_update_check, None);
        let late = read_beside_a_kept_master(r#""last_update_check": 1.5"#);
        assert_eq!(late.last_update_check, None);
    }

    // covers: S212 (a file that isn't settings at all still gives the defaults)
    #[test]
    fn a_file_that_isnt_json_or_isnt_an_object_is_still_refused() {
        for text in ["not json", "null"] {
            assert!(Settings::from_json(text).is_err(), "{text}");
        }
    }

    // covers: S212 (a version that isn't a whole number costs only itself)
    #[test]
    fn a_version_that_isnt_a_whole_number_reads_as_the_current_format() {
        for version in [r#""3""#, r#""x""#, "1.5", "-2", "null", "true"] {
            let text = format!(
                r#"{{"version": {version}, "master_volume": 40,
                    "outputs": {{"desk": {{"enabled": true, "volume": 50, "delay_ms": -200}}}}}}"#
            );
            let (read, found) =
                Settings::from_json(&text).unwrap_or_else(|why| panic!("{version}: {why}"));
            assert_eq!(read.master_volume, 40.0, "{version}");
            assert_eq!(read.outputs["desk"].delay_ms, -200.0, "{version}");
            assert!(
                matches!(
                    found,
                    Loaded::Read {
                        version: VERSION,
                        ..
                    }
                ),
                "{version}: {found:?}"
            );
            assert!(!written_by_a_newer_build(text.as_bytes()), "{version}");
            assert_eq!(unreadable_fields(&text), ["version"], "{version}");
        }
        let beyond = r#"{"version": 5000000000, "master_volume": 40}"#;
        assert!(Settings::from_json(beyond).is_err_and(|why| why.contains("newer")));
        assert!(written_by_a_newer_build(beyond.as_bytes()));
    }

    // covers: S245 (a zone volume that isn't a number is the default), S212
    #[test]
    fn a_zone_with_an_unreadable_volume_member_or_name_keeps_the_rest() {
        let text = r#"{"version": 3, "speaker_zones_migrated": true,
            "speaker_zones": [
                {"id": "z1", "name": "Den", "members": ["lc:desk", 5, "ap:far"], "volume": "x",
                 "member_names": {"lc:desk": "Desk", "ap:far": 9}},
                {"id": "z2", "name": 4, "members": ["lc:hall"], "volume": null}
            ]
        }"#;
        let (read, _) = Settings::from_json(text).expect("read");
        assert_eq!(
            read.speaker_zones,
            [
                SavedZone {
                    id: "z1".to_owned(),
                    name: "Den".to_owned(),
                    members: vec!["lc:desk".to_owned(), "ap:far".to_owned()],
                    volume: DEFAULT_ZONE_VOLUME,
                    member_names: BTreeMap::from([("lc:desk".to_owned(), "Desk".to_owned())]),
                },
                SavedZone {
                    id: "z2".to_owned(),
                    members: vec!["lc:hall".to_owned()],
                    ..SavedZone::default()
                },
            ]
        );
    }

    // covers: S251 (an own volume that isn't a number is none, a master that isn't one is 100)
    #[test]
    fn a_preset_with_an_unreadable_volume_or_gain_keeps_the_rest() {
        let text = r#"{"version": 3, "speaker_zones_migrated": true, "speaker_presets_migrated": true,
            "speaker_presets": [
                {"id": "p1", "name": "Evening",
                 "members": [{"key": "lc:desk", "volume": "x"}, {"key": 5}, {"key": "ap:far", "volume": 30}],
                 "zones": [{"id": "z1", "volume": "loud"}, 3],
                 "master_volume": null, "gains_db": [1, null, 0, 0, 0, 0, 0, 0, 0, "x"]}
            ]
        }"#;
        let (read, _) = Settings::from_json(text).expect("read");
        assert_eq!(
            read.speaker_presets,
            [SavedPreset {
                id: "p1".to_owned(),
                name: "Evening".to_owned(),
                members: vec![
                    SavedPresetMember {
                        key: "lc:desk".to_owned(),
                        volume: None,
                    },
                    SavedPresetMember {
                        key: "ap:far".to_owned(),
                        volume: Some(30.0),
                    },
                ],
                zones: vec![SavedPresetZone {
                    id: "z1".to_owned(),
                    volume: DEFAULT_ZONE_VOLUME,
                }],
                master_volume: 100.0,
                gains_db: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            }]
        );
    }
}
