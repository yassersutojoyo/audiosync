//! The user's choices, kept across restarts.
//!
//! One small JSON file in the app's data folder,
//! `%LOCALAPPDATA%\AudioSync\settings.json`, next to the AirPlay pairings:
//! the source (the whole system, or an app by its executable's file name,
//! since its process ID changes every time it starts), each output's and
//! AirPlay speaker's switch, volume and delay (outputs by endpoint ID,
//! AirPlay speakers by the ID they announce), the master volume, the
//! auto-connect marks, the groups, the equalizer, View > Show Virtual
//! Devices, and the main window's place and size.
//!
//! The file carries a format version ([`VERSION`]). A file from an older
//! version is read for the fields it has; one from a newer version, one that
//! can't be read or one that isn't valid JSON is ignored as a whole (the
//! defaults apply, and the next change writes a fresh file). Fields this
//! version doesn't know are ignored, missing ones take their defaults, and
//! values out of range are clamped ([`Settings::clamped`]). Nothing here
//! panics on a bad file, and the caller logs what [`Loaded`] says.
//!
//! The file is replaced as a whole through a temporary file
//! (`crate::data_folder::write_atomically`), so a crash while saving leaves
//! the old file or the new one.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use audiosync_core::eq::{BAND_COUNT, EqSettings};
use audiosync_core::policy::delay::MAX_DELAY_MS;
use audiosync_core::policy::speakers::SpeakerKey;
use audiosync_core::volume::{DEFAULT_LOCAL_POSITION, DEFAULT_MASTER_POSITION, sanitize_position};
use serde::{Deserialize, Serialize};

use crate::data_folder;
use crate::ui_model::WindowFrame;

/// The file's name in the app's data folder.
pub const FILE_NAME: &str = "settings.json";

/// The format version this build writes, and the newest it reads.
pub const VERSION: u32 = 1;

/// The longest own delay an AirPlay speaker takes, in milliseconds (the
/// sender's limit).
pub const MAX_AIRPLAY_DELAY_MS: f64 = 3000.0;

/// The most entries kept in each list or map: far more than anyone has
/// devices, small enough that a damaged file can't make the app slow.
pub const MAX_ENTRIES: usize = 256;

/// The longest ID or name kept, in bytes; an entry with a longer ID is
/// dropped, a longer name is cut.
pub const MAX_TEXT: usize = 1024;

/// The main window's smallest size in logical pixels, as `ui/main.slint`
/// sets it.
pub const MIN_WINDOW: (f64, f64) = (860.0, 520.0);

/// The main window's largest size kept, in logical pixels.
pub const MAX_WINDOW: (f64, f64) = (16_384.0, 16_384.0);

/// The farthest a saved window position may be from the origin, in physical
/// pixels: Windows' own coordinate range.
pub const MAX_POSITION: i32 = 32_000;

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

/// An output's or AirPlay speaker's settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSpeaker {
    /// Whether the user switched it on.
    pub enabled: bool,
    /// The volume slider position, 0 to 100.
    pub volume: f64,
    /// The delay in milliseconds: 0 to 20 000 for an output, 0 to 3000 for
    /// an AirPlay speaker's own delay.
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

    fn clamped(self, max_delay_ms: f64) -> SavedSpeaker {
        SavedSpeaker {
            enabled: self.enabled,
            volume: sanitize_position(self.volume).unwrap_or(DEFAULT_LOCAL_POSITION),
            delay_ms: if self.delay_ms.is_finite() {
                self.delay_ms.clamp(0.0, max_delay_ms)
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
    pub name: String,
    pub members: Vec<String>,
}

/// The equalizer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedEqualizer {
    pub enabled: bool,
    pub preamp_db: f64,
    /// Band 0 (32 Hz) to band 9 (16 kHz), in dB.
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
    })
}

/// Everything the file keeps. See the module documentation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The format version the file was written in.
    pub version: u32,
    pub source: SavedSource,
    /// The master volume slider position, 0 to 100.
    pub master_volume: f64,
    /// The local outputs, by endpoint ID.
    pub outputs: BTreeMap<String, SavedSpeaker>,
    /// The AirPlay speakers, by the ID they announce.
    pub airplay: BTreeMap<String, SavedSpeaker>,
    /// The speakers marked auto-connect, by speaker key.
    pub auto_connect: BTreeSet<String>,
    pub groups: Vec<SavedGroup>,
    pub equalizer: SavedEqualizer,
    /// View > Show Virtual Devices.
    pub show_virtual_devices: bool,
    /// The main window's place and size, once it has been shown.
    pub window: Option<WindowFrame>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            version: VERSION,
            source: SavedSource::System,
            master_volume: DEFAULT_MASTER_POSITION,
            outputs: BTreeMap::new(),
            airplay: BTreeMap::new(),
            auto_connect: BTreeSet::new(),
            groups: Vec::new(),
            equalizer: SavedEqualizer::default(),
            show_virtual_devices: false,
            window: None,
        }
    }
}

/// Keeps the first [`MAX_ENTRIES`] speakers whose ID is usable, clamped.
fn clamp_speakers(
    speakers: BTreeMap<String, SavedSpeaker>,
    max_delay_ms: f64,
) -> BTreeMap<String, SavedSpeaker> {
    speakers
        .into_iter()
        .filter(|(id, _)| usable_id(id))
        .take(MAX_ENTRIES)
        .map(|(id, speaker)| (id, speaker.clamped(max_delay_ms)))
        .collect()
}

fn usable_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_TEXT
}

/// Whether `key` is a speaker key the app can use (`lc:` or `ap:` and an ID).
fn usable_key(key: &str) -> bool {
    key.len() <= MAX_TEXT && SpeakerKey::parse(key).is_some_and(|key| !key.id().is_empty())
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
        Settings {
            version: VERSION,
            source,
            master_volume: sanitize_position(self.master_volume).unwrap_or(DEFAULT_MASTER_POSITION),
            outputs: clamp_speakers(self.outputs, MAX_DELAY_MS),
            airplay: clamp_speakers(self.airplay, MAX_AIRPLAY_DELAY_MS),
            auto_connect: self
                .auto_connect
                .into_iter()
                .filter(|key| usable_key(key))
                .take(MAX_ENTRIES)
                .collect(),
            groups,
            equalizer: SavedEqualizer::from(self.equalizer.settings()),
            show_virtual_devices: self.show_virtual_devices,
            window: self.window.and_then(clamp_window),
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
            #[serde(default)]
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
        Ok((clamped, Loaded::Read { version, adjusted }))
    }
}

/// What reading the file found, for the debug log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    /// There is no file yet.
    NoFile,
    /// The file was read, written in format `version`; `adjusted` when
    /// values out of range were clamped or unusable entries dropped.
    Read { version: u32, adjusted: bool },
    /// The file was ignored, and why.
    Ignored(String),
}

impl fmt::Display for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Loaded::NoFile => write!(f, "no file yet, the defaults apply"),
            Loaded::Read { version, adjusted } => {
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
                "ignored: {reason}; the defaults apply, and the next change writes a new file"
            ),
        }
    }
}

/// Where the settings are kept: the file, or nowhere (this run only).
#[derive(Clone, Debug, Default)]
pub struct SettingsFile {
    path: Option<PathBuf>,
}

impl SettingsFile {
    /// Where the app keeps the file: in `%LOCALAPPDATA%\AudioSync`, or
    /// `None` when Windows names no such folder.
    pub fn default_path() -> Option<PathBuf> {
        data_folder::folder().map(|folder| folder.join(FILE_NAME))
    }

    /// The file at `path`.
    pub fn at(path: PathBuf) -> SettingsFile {
        SettingsFile { path: Some(path) }
    }

    /// No file: the settings last until the app quits.
    pub fn in_memory() -> SettingsFile {
        SettingsFile { path: None }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The settings in the file, or the defaults, with what was found.
    pub fn load(&self) -> (Settings, Loaded) {
        let Some(path) = &self.path else {
            return (Settings::default(), Loaded::NoFile);
        };
        let text = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return (Settings::default(), Loaded::NoFile);
            }
            Err(error) => {
                return (
                    Settings::default(),
                    Loaded::Ignored(format!("it can't be read ({error})")),
                );
            }
        };
        let read = String::from_utf8(text)
            .map_err(|_| "it isn't text".to_owned())
            .and_then(|text| Settings::from_json(&text));
        match read {
            Ok(read) => read,
            Err(reason) => (Settings::default(), Loaded::Ignored(reason)),
        }
    }

    /// Replaces the file with `settings`. Without a file it does nothing.
    pub fn save(&self, settings: &Settings) -> io::Result<()> {
        match &self.path {
            Some(path) => data_folder::write_atomically(path, settings.to_json().as_bytes()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            window: Some(WindowFrame {
                x: -1200,
                y: 40,
                width: 1024.0,
                height: 700.0,
            }),
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

    #[test]
    fn settings_survive_a_save_and_a_load() {
        let file = SettingsFile::at(test_dir("settings-roundtrip").join(FILE_NAME));
        assert_eq!(file.load(), (Settings::default(), Loaded::NoFile));
        file.save(&sample()).expect("saved");
        let (loaded, found) = file.load();
        assert_eq!(loaded, sample());
        assert_eq!(
            found,
            Loaded::Read {
                version: VERSION,
                adjusted: false
            }
        );
    }

    #[test]
    fn a_corrupted_file_is_ignored_and_replaced_by_the_next_save() {
        let path = test_dir("settings-corrupted").join(FILE_NAME);
        let file = SettingsFile::at(path.clone());
        for contents in [
            &b"{\"version\": 1, \"master_volume\": "[..],
            b"not json at all",
            b"[1, 2, 3]",
            b"{\"version\": 1, \"master_volume\": \"loud\"}",
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
                adjusted: false
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
            "airplay": {"far": {"delay_ms": 5000, "volume": 101}},
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
                adjusted: true
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
        assert_eq!(settings.airplay["far"].delay_ms, MAX_AIRPLAY_DELAY_MS);
        assert_eq!(settings.airplay["far"].volume, 100.0);
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
            })
        );
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
        let kept = folder.join("settings.newer.json");
        let mut newer = sample();
        newer.version = VERSION + 1;
        let newer_text = newer.to_json();
        fs::write(&path, &newer_text).expect("written");
        // An older copy kept earlier is replaced.
        fs::write(&kept, b"an older copy").expect("written");
        let file = SettingsFile::at(path.clone());
        assert!(matches!(file.load().1, Loaded::Ignored(_)));

        file.save(&Settings::default()).expect("saved");
        assert_eq!(fs::read_to_string(&kept).expect("kept"), newer_text);
        assert_eq!(file.load().0, Settings::default());

        // The file is this build's now: the next save leaves the copy alone.
        file.save(&sample()).expect("saved again");
        assert_eq!(fs::read_to_string(&kept).expect("kept"), newer_text);
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
}
