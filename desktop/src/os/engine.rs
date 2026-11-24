//! What every system's audio backend starts with, whatever its driver: the
//! AirPlay sender with the paired speakers, the Google Cast host with the
//! speakers' file, the playing track, and the adapter that keeps the UI's
//! own state.

use std::rc::Rc;

use crate::audio::controller::{Controller, Driver};
use crate::audio::now_playing::{ReceiverLabels, SharedLabels, TrackReader};
use crate::debug_log::{self, DebugLog};
use crate::now_playing_card::MediaControls;
use crate::view::App;
use crate::{Strings, adapter, audio, data_folder, settings, ui_model};

/// What the windows show and drive: `audio`, with the AirPlay sender and
/// the Google Cast host, through the adapter that keeps the UI's own state.
/// Both log to `log`.
pub fn backend<D: Driver + 'static>(
    mut audio: Controller<D>,
    log: &DebugLog,
    labels: &SharedLabels,
    track_reader: Box<dyn TrackReader>,
) -> Box<dyn ui_model::Backend + Send> {
    audio.set_debug_log(log.clone());
    let settings_file = settings::SettingsFile::default_path().map_or_else(
        settings::SettingsFile::in_memory,
        settings::SettingsFile::at,
    );
    if crate::test_instance::current().is_some() {
        audio.leave_system_outputs();
    }
    // Before the settings are read, so a record restored now isn't read.
    #[cfg(windows)]
    restore_records(&mut audio, log, settings_file.path());
    // Waits on a setup that may still run, off the launch (VC42).
    #[cfg(windows)]
    std::thread::spawn({
        let log = log.clone();
        move || audio::wasapi::restore_kept(&log)
    });
    #[cfg(windows)]
    audio.set_restore_guard(crate::restore_guard::keeper());
    // S473 while S477's own sink isn't the default.
    #[cfg(target_os = "linux")]
    audio.hold_out_default_sink();
    #[cfg(target_os = "linux")]
    if let Some(endpoints) = audio.driver_default_endpoints() {
        audio.set_default_switch(
            endpoints,
            Box::new(settings::DefaultRecordFile(
                settings_file.path().map(std::path::Path::to_path_buf),
            )),
        );
    }
    let read = settings_file.load();
    start_airplay(&mut audio, log);
    let (cast_file, cast_unread) = start_cast(&mut audio, log);
    audio.set_track_reader(track_reader);
    audio.set_receiver_labels(labels.clone());
    let mut adapter = adapter::AudioAdapter::launched(
        audio,
        super::notice_time,
        log.clone(),
        read,
        settings_file,
        crate::auto_connect_skipped_by(
            debug_log::env_skips_auto_connect(),
            crate::preview_play_env(log).as_deref(),
            crate::test_instance::current(),
        ),
    );
    adapter.set_cast_file(cast_file, cast_unread);
    #[cfg(windows)]
    adapter.set_silent_output(std::sync::Arc::new(audio::wasapi::VbCablePlatform {
        user_agent: crate::updater::fetch::user_agent(
            "Windows",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH,
        ),
        data_folder: data_folder::folder(),
        kept: audio::wasapi::kept_file(),
    }));
    Box::new(adapter)
}

/// Restores what S469's and S471's records in the settings file at `path`
/// say and keeps their seams in `audio`: at launch, and in S474's guard.
#[cfg(windows)]
pub fn restore_records<D: Driver>(
    audio: &mut Controller<D>,
    log: &DebugLog,
    path: Option<&std::path::Path>,
) {
    audio.set_default_switch(
        Box::new(audio::wasapi::WasapiDefaults),
        Box::new(crate::restore_guard::WithSignIn::in_settings(
            settings::DefaultRecordFile(path.map(std::path::Path::to_path_buf)),
            path,
        )),
    );
    // S471; after S469's restore, so its record's silent endpoint is read.
    match crate::platform::windows_version()
        .and_then(|version| audio::app_output::variant(version.build))
    {
        Some(variant) => {
            let outputs = audio::wasapi::WasapiAppOutputs(variant);
            match outputs.probe() {
                Ok(()) => audio.set_app_output(
                    Box::new(outputs),
                    Box::new(crate::restore_guard::WithSignIn::in_settings(
                        settings::AppRecordFile(path.map(std::path::Path::to_path_buf)),
                        path,
                    )),
                ),
                Err(error) => log.log(|| {
                    format!("app output: apps aren't moved, the per-app interface failed: {error}")
                }),
            }
        }
        None => log.log(|| "app output: apps aren't moved on this Windows build".to_owned()),
    }
}

/// What the app hands the backend's media parts once its windows exist: the
/// AirPlay receivers' labels from the strings, and the now-playing card's
/// controls.
pub struct Media {
    labels: SharedLabels,
    controls: Box<dyn MediaControls>,
}

impl Media {
    pub fn new(labels: SharedLabels, controls: Box<dyn MediaControls>) -> Media {
        Media { labels, controls }
    }

    pub fn attach(self, app: &Rc<App>) {
        self.labels.set(receiver_labels(&app.strings()));
        app.set_media_controls(self.controls);
    }
}

/// The file that keeps each Google Cast speaker's offset and audio format.
const CAST_FILE_NAME: &str = "cast-speakers.json";
/// The file that keeps each Google Cast speaker's last certificate
/// fingerprint, so a change across a relaunch is logged (C21).
const CAST_CERTIFICATES_FILE_NAME: &str = "cast-certificates.txt";
/// The device key pins of Cast device authentication (spec C129).
const CAST_DEVICE_KEYS_FILE_NAME: &str = "cast-device-keys.txt";

/// Starts the Google Cast host with the speakers' offsets and formats from
/// their file, and returns that file for the changes, `None` when the system
/// names no data folder, so they last until AudioSync quits; with whether the
/// file was there but couldn't be read. Without a host no Cast speaker is
/// listed.
fn start_cast<D: Driver>(
    audio: &mut Controller<D>,
    log: &DebugLog,
) -> (Option<std::path::PathBuf>, bool) {
    use audiosync_cast::client::FingerprintBook;
    use audiosync_cast::device_keys::DeviceKeyBook;
    use audiosync_cast::host::DeviceMemory;
    let path = data_folder::folder().map(|folder| folder.join(CAST_FILE_NAME));
    let mut unread = false;
    let memory = match path.as_deref().map(data_folder::read_at_launch) {
        None => DeviceMemory::default(),
        Some(Ok(bytes)) => DeviceMemory::from_json(&String::from_utf8_lossy(&bytes)),
        Some(Err(error)) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log.log(|| format!("Cast: the speakers' file couldn't be read: {error}"));
                unread = true;
            }
            DeviceMemory::default()
        }
    };
    let fingerprints = match data_folder::folder() {
        Some(folder) => {
            let (book, error) = FingerprintBook::open(
                folder.join(CAST_CERTIFICATES_FILE_NAME),
                data_folder::read_at_launch,
                data_folder::write_atomically,
            );
            if let Some(error) = error {
                log.log(|| format!("Cast: the certificates file couldn't be read: {error}"));
            }
            let save_log = log.clone();
            book.on_save_error(move |error| {
                save_log
                    .log(|| format!("Cast: the certificates file couldn't be written: {error}"));
            })
        }
        None => FingerprintBook::new(),
    };
    let device_keys = match data_folder::folder() {
        Some(folder) => {
            let (book, error) = DeviceKeyBook::open(
                folder.join(CAST_DEVICE_KEYS_FILE_NAME),
                data_folder::read_at_launch,
                data_folder::write_atomically,
            );
            if let Some(error) = error {
                log.log(|| format!("Cast: the device keys file couldn't be read: {error}"));
            }
            let save_log = log.clone();
            book.on_save_error(move |error| {
                save_log.log(|| format!("Cast: the device keys file couldn't be written: {error}"));
            })
        }
        None => DeviceKeyBook::new(),
    };
    match audio::cast::HostLink::start(memory, fingerprints, device_keys) {
        Ok((link, browse_error)) => {
            match browse_error {
                Some(error) => log.log(|| format!("Cast: discovery couldn't start: {error}")),
                None => log.log(|| "Cast: browsing for speakers".to_owned()),
            }
            audio.set_cast(Box::new(link));
        }
        Err(error) => log.log(|| format!("Cast: the host couldn't start: {error}")),
    }
    (path, unread)
}

/// What AirPlay receivers show while no track counts, from the strings.
fn receiver_labels(strings: &Strings<'_>) -> ReceiverLabels {
    ReceiverLabels {
        title: strings.get_receiver_untitled().into(),
        album: strings.get_receiver_album().into(),
        whole_system: strings.get_source_system().into(),
        unnamed_source: strings.get_receiver_unnamed_source().into(),
        list_separator: strings.get_list_separator().into(),
        all_except: strings.invoke_source_all_except("{}".into()).into(),
    }
}

/// Starts the AirPlay sender and its discovery through the audio backend,
/// which keeps the way to start a new one when the user retries. Without a
/// sender the app runs on with local outputs only; without discovery no
/// AirPlay speaker is listed.
fn start_airplay<D: Driver>(audio: &mut Controller<D>, log: &DebugLog) {
    let starter_log = log.clone();
    audio.start_airplay(
        Box::new(move || start_sender(&starter_log)),
        open_pairings(log),
    );
}

/// A new AirPlay sender, browsing for receivers when its discovery could
/// start, and the feed for its input; `None` when it couldn't start.
fn start_sender(log: &DebugLog) -> Option<audio::airplay::StartedSender> {
    use audio::airplay::{FeedSlot, SenderLink, StartedSender};
    let (link, input) = match SenderLink::start() {
        Ok(started) => started,
        Err(error) => {
            log.log(|| format!("AirPlay: the sender couldn't start: {error}"));
            return None;
        }
    };
    let browsing = match link.start_discovery() {
        Ok(()) => {
            log.log(|| "AirPlay: browsing for receivers".to_owned());
            true
        }
        Err(error) => {
            log.log(|| format!("AirPlay: discovery couldn't start: {error}"));
            false
        }
    };
    Some(StartedSender {
        link: Box::new(link),
        feed: Some(FeedSlot::new(input)),
        browsing,
    })
}

/// The paired Apple TVs' credentials, from the file in the app's data
/// folder; kept for this run only when the system names no such folder.
fn open_pairings(log: &DebugLog) -> audio::pairings::PairingStore {
    use audio::pairings::PairingStore;
    let Some(path) = PairingStore::default_path() else {
        log.log(|| {
            "AirPlay pairings: no app data folder; pairings last until AudioSync quits".to_owned()
        });
        return PairingStore::in_memory();
    };
    let (store, loaded) = PairingStore::open(path, super::sealer());
    log.log(|| format!("AirPlay pairings: {loaded}"));
    if let Some(tightened) = store.tightened() {
        log.log(|| format!("AirPlay pairings: {tightened}"));
    }
    store
}
