//! The credentials of the Apple TVs the user paired, kept per speaker so a
//! PIN is asked only once.
//!
//! The AirPlay sender hands the host each pairing's credentials and never
//! stores them itself; this store keeps them in one file, by speaker ID,
//! encrypted for the current Windows user with DPAPI (`CryptProtectData`),
//! so another account on the PC, or a copy of the file on another PC, can't
//! read them. The file lives in the app's data folder,
//! `%LOCALAPPDATA%\AudioSync\airplay-pairings.dat`. A file that can't be
//! read, decrypted or parsed is ignored as a whole: the speakers in it are
//! paired again, and it is kept aside in a copy named after
//! [`IGNORED_FILE_NAME`] with the time, which no later copy replaces: at
//! launch when it can be read then, so the user is told at once, and in any
//! case before the next pairing writes a fresh file. A read that fails is
//! tried once more at launch (another program may hold the file for a
//! moment), then every few seconds, and while the file still can't be read
//! nothing is written over it: new pairings last until AudioSync quits.
//! A file whose framing holds but whose entry doesn't read (a credential
//! blob a later build wrote, an empty, repeated or garbled ID) costs only
//! that entry: the file is kept aside the same way, without the notice,
//! and rewritten at once with the entries that read.
//!
//! Next to it, [`AIRPLAY2_FILE_NAME`] keeps the AirPlay 2 path's device ID
//! and the HomeKit credentials its pairings give, sealed the same way, in
//! the byte form [`Airplay2Identity`] defines. It is read and written only
//! once the hidden AirPlay 2 switch is on ([`PairingStore::open_airplay2`]),
//! and by Forget Pairing ([`PairingStore::forget_airplay2_closed`]);
//! one that can't be decrypted or parsed is kept aside and replaced, one
//! that can't be read is never written over.
//! The AirPlay 2 timing entries (P217) sit unsealed beside it in
//! [`airplay2_timing::FILE_NAME`], a cache holding no secret.
//!
//! Before encryption the file holds a magic tag, then per speaker its ID and
//! the credential blob the sender defines ([`Credentials::to_bytes`]), each
//! with a 16-bit little-endian length. The plaintext lives only in buffers
//! that are wiped when dropped, and nothing here logs a secret: callers log
//! what [`Loaded`] and the I/O errors say, which never holds one.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::pairing::Credentials;
use audiosync_airplay::sender::{TimingMemory, random_airplay2_device_id};
use audiosync_core::airplay2_identity::{Airplay2Identity, Unreadable};
use audiosync_core::airplay2_timing::{self, TimingStore};
use zeroize::Zeroizing;

use crate::data_folder;

/// The file's name in the app's data folder.
pub const FILE_NAME: &str = "airplay-pairings.dat";

/// What the copy of a file the store ignored at launch is named after
/// (`airplay-pairings.ignored-<time>.dat`), kept before the store first
/// writes over it.
pub const IGNORED_FILE_NAME: &str = "airplay-pairings.ignored.dat";

/// The first bytes of the decrypted file, with its format version.
const MAGIC: &[u8] = b"AudioSync pairings 1\n";

/// The AirPlay 2 identity's file name, next to [`FILE_NAME`].
pub const AIRPLAY2_FILE_NAME: &str = "airplay2-identity.dat";

/// What the copy of an AirPlay 2 identity file that was ignored is named
/// after.
const AIRPLAY2_IGNORED_FILE_NAME: &str = "airplay2-identity.ignored.dat";

/// The AirPlay 2 identity, once the AirPlay 2 path asked for it.
struct Airplay2File {
    identity: Airplay2Identity,
    /// False when the file couldn't be read, or a newer AudioSync wrote it,
    /// or no random device ID could be made: nothing is written then.
    writable: bool,
}

/// How the system encrypts the file's contents for the current user only
/// (`crate::os::sealer`).
pub trait Seal: Sync {
    fn seal(&self, plain: &[u8]) -> io::Result<Vec<u8>>;
    fn open(&self, sealed: &[u8]) -> io::Result<Zeroizing<Vec<u8>>>;
}

/// Encrypts and decrypts the file's contents: DPAPI for the current user in
/// the app, something simpler in tests that run off Windows.
#[derive(Clone, Copy)]
pub struct Sealer(&'static dyn Seal);

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sealer")
    }
}

impl Sealer {
    pub const fn new(seal: &'static dyn Seal) -> Sealer {
        Sealer(seal)
    }

    pub fn seal(&self, plain: &[u8]) -> io::Result<Vec<u8>> {
        self.0.seal(plain)
    }

    pub fn open(&self, sealed: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
        self.0.open(sealed)
    }
}

/// What opening the store found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    /// No file yet: nothing was paired.
    NoFile,
    /// The credentials of this many speakers.
    Speakers(usize),
    /// The credentials of `speakers` speakers; `skipped` entries didn't
    /// read and those speakers are paired again.
    Partly { speakers: usize, skipped: usize },
    /// The file was ignored, for this reason (never a secret).
    Ignored(String),
}

impl fmt::Display for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Loaded::NoFile => f.write_str("no paired speakers yet"),
            Loaded::Speakers(1) => f.write_str("credentials loaded for 1 speaker"),
            Loaded::Speakers(n) => write!(f, "credentials loaded for {n} speakers"),
            Loaded::Partly { speakers, skipped } => write!(
                f,
                "credentials loaded for {speakers} speakers; {skipped} unreadable entries \
                 skipped and the file kept aside, so those speakers pair again"
            ),
            Loaded::Ignored(reason) => {
                write!(
                    f,
                    "the stored credentials were ignored ({reason}); pair again"
                )
            }
        }
    }
}

/// The credentials of every paired speaker, by speaker ID, and the file they
/// are kept in. See the module documentation.
pub struct PairingStore {
    /// `None`: kept in memory only (no data folder, or a test).
    path: Option<PathBuf>,
    sealer: Sealer,
    entries: BTreeMap<String, Credentials>,
    /// The file was ignored when the store opened: it is kept aside before
    /// the next write replaces it.
    keep_aside: bool,
    /// Where a write kept that file aside, until
    /// [`PairingStore::take_kept_aside`] takes it.
    kept_aside: Option<PathBuf>,
    /// The copy is for [`PairingStore::take_kept_aside`]: off when only
    /// some entries didn't read, since the notice says every pairing is lost.
    tell_kept_aside: bool,
    /// Polls left before [`PairingStore::keep_aside_if_due`] tries again to
    /// keep that file aside.
    keep_aside_wait: u32,
    /// `None` until [`PairingStore::open_airplay2`].
    airplay2: Option<Airplay2File>,
    /// The AirPlay 2 timing entries (P217), in memory until
    /// [`PairingStore::open_airplay2`] opens their file: unsealed, since
    /// they hold no secret, and apart from the identity file, which stays
    /// at the format older builds read.
    timing: TimingStore,
    /// The file or its folder was open to other accounts at launch and was
    /// made owner-only ([`make_private`]).
    tightened: bool,
    /// How a copy kept aside is deleted; tests make it fail.
    remove_copy: fn(&Path) -> io::Result<()>,
}

impl fmt::Debug for PairingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Credentials' Debug shows only their client ID.
        f.debug_struct("PairingStore")
            .field("path", &self.path)
            .field("speakers", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for PairingStore {
    fn default() -> PairingStore {
        PairingStore::in_memory()
    }
}

impl PairingStore {
    /// A store that keeps credentials for this run only.
    pub fn in_memory() -> PairingStore {
        PairingStore {
            path: None,
            sealer: crate::os::sealer(),
            entries: BTreeMap::new(),
            keep_aside: false,
            kept_aside: None,
            tell_kept_aside: true,
            keep_aside_wait: 0,
            airplay2: None,
            timing: TimingStore::in_memory(),
            tightened: false,
            remove_copy: |path| fs::remove_file(path),
        }
    }

    /// Where the app keeps the file: in `%LOCALAPPDATA%\AudioSync`, or
    /// `None` when Windows names no such folder.
    #[cfg(os_backend)]
    pub fn default_path() -> Option<PathBuf> {
        data_folder::folder().map(|folder| folder.join(FILE_NAME))
    }

    /// The store kept in `path`, with what it holds now. A read that fails
    /// is tried once more after a short pause
    /// ([`data_folder::read_at_launch`]). A file it ignores is kept aside at
    /// once when it can be read, so the user can be told at launch
    /// ([`PairingStore::take_kept_aside`]); one that can't be read yet is
    /// tried again every few seconds ([`PairingStore::keep_aside_if_due`])
    /// and before the first write over it.
    pub fn open(path: PathBuf, sealer: Sealer) -> (PairingStore, Loaded) {
        let (tightened, read) = match make_private(&path) {
            Ok(tightened) => (tightened, read(&path, sealer)),
            Err(reason) => (false, Err(reason)),
        };
        let (entries, loaded) = match read {
            Ok(Some((entries, 0))) => {
                let count = entries.len();
                (entries, Loaded::Speakers(count))
            }
            Ok(Some((entries, skipped))) => {
                let speakers = entries.len();
                (entries, Loaded::Partly { speakers, skipped })
            }
            Ok(None) => (BTreeMap::new(), Loaded::NoFile),
            Err(reason) => (BTreeMap::new(), Loaded::Ignored(reason)),
        };
        let mut store = PairingStore {
            path: Some(path),
            sealer,
            entries,
            keep_aside: matches!(loaded, Loaded::Ignored(_) | Loaded::Partly { .. }),
            kept_aside: None,
            tell_kept_aside: matches!(loaded, Loaded::Ignored(_)),
            keep_aside_wait: 0,
            airplay2: None,
            timing: TimingStore::in_memory(),
            tightened,
            remove_copy: |path| fs::remove_file(path),
        };
        // While it can't be read, the next try is due a few seconds later.
        let _ = if matches!(loaded, Loaded::Partly { .. }) {
            store.save()
        } else {
            store.keep_ignored_aside()
        };
        (store, loaded)
    }

    /// Keeps the file ignored at launch aside now, when it can be read, for
    /// [`PairingStore::take_kept_aside`]. While it can't, the error is
    /// returned and the next try is due
    /// [`data_folder::KEEP_ASIDE_RETRY_POLLS`] polls later.
    fn keep_ignored_aside(&mut self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.keep_aside {
            return Ok(());
        }
        match data_folder::keep_aside(path, IGNORED_FILE_NAME) {
            Ok(kept) => {
                if self.tell_kept_aside {
                    self.kept_aside = kept;
                }
                self.keep_aside = false;
                Ok(())
            }
            Err(error) => {
                self.keep_aside_wait = data_folder::KEEP_ASIDE_RETRY_POLLS;
                Err(error)
            }
        }
    }

    /// Called at every poll: while the file ignored at launch couldn't be
    /// kept aside yet, tries again every
    /// [`data_folder::KEEP_ASIDE_RETRY_POLLS`] polls, so the user is told
    /// without pairing anything first.
    pub fn keep_aside_if_due(&mut self) {
        if !self.keep_aside {
            return;
        }
        self.keep_aside_wait = self.keep_aside_wait.saturating_sub(1);
        if self.keep_aside_wait == 0 {
            let _ = self.keep_ignored_aside();
        }
    }

    /// Where the file ignored at launch was kept aside, once that was done
    /// (at launch, at a later poll, or before the first write over it);
    /// `None` after that, so the user is told once.
    pub fn take_kept_aside(&mut self) -> Option<PathBuf> {
        self.kept_aside.take()
    }

    /// The debug line when the file or its folder was open to other
    /// accounts at launch and was made owner-only.
    pub fn tightened(&self) -> Option<&'static str> {
        self.tightened.then_some(OPEN_TO_OTHERS)
    }

    /// Whether the speaker `id` is paired.
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// The speaker's credentials, if it is paired.
    pub fn get(&self, id: &str) -> Option<&Credentials> {
        self.entries.get(id)
    }

    /// Keeps the speaker's new credentials and writes the file. When the
    /// file can't be written they are still kept for this run, and the error
    /// says why.
    pub fn insert(&mut self, id: &str, credentials: Credentials) -> io::Result<()> {
        self.entries.insert(id.to_owned(), credentials);
        self.save()
    }

    /// Forgets the speaker's credentials and writes the file, then deletes
    /// the copies of this file and of the AirPlay 2 identity file kept aside
    /// that hold the speaker's ID. Returns whether it was paired.
    pub fn remove(&mut self, id: &str) -> io::Result<bool> {
        let removed = self.entries.remove(id).is_some();
        let saved = if removed { self.save() } else { Ok(()) };
        let pruned = self.path.as_deref().map_or(Ok(()), |path| {
            let copies = |name| prune_copies(path, name, id, self.sealer, self.remove_copy);
            copies(IGNORED_FILE_NAME).and(copies(AIRPLAY2_IGNORED_FILE_NAME))
        });
        saved.and(pruned).map(|()| removed)
    }

    /// Reads the AirPlay 2 identity next to the file, once, when the hidden
    /// AirPlay 2 switch is on, and returns its device ID with what was found
    /// for the log (never a secret). A missing or ignored file gets a new
    /// random device ID, written at once so it stays the same (P5).
    pub fn open_airplay2(&mut self) -> ([u8; 6], String) {
        if let Some(file) = &self.airplay2 {
            return (file.identity.device_id(), "already open".to_owned());
        }
        let path = self.airplay2_path();
        let fresh = || match random_airplay2_device_id() {
            Some(id) => (Airplay2Identity::new(id), true),
            None => (Airplay2Identity::new([0; 6]), false),
        };
        let mut tightened = false;
        let read = match &path {
            None => Ok(None),
            Some(path) => match make_private(path) {
                Ok(made_private) => {
                    tightened = made_private;
                    read_airplay2(path, self.sealer)
                }
                Err(reason) => Err(Airplay2Read::Ignored(reason)),
            },
        };
        let (identity, writable, found) = match read {
            Ok(Some(identity)) => {
                let found = format!("credentials for {} speaker(s)", identity.len());
                (identity, true, found)
            }
            Ok(None) => {
                let (identity, random) = fresh();
                (identity, random, "no file yet".to_owned())
            }
            Err(Airplay2Read::Unreadable(reason)) => {
                let identity = fresh().0;
                (identity, false, format!("{reason}; left as it is"))
            }
            Err(Airplay2Read::Ignored(reason)) => {
                let (identity, random) = fresh();
                let kept = path
                    .as_deref()
                    .map(|path| data_folder::keep_aside(path, AIRPLAY2_IGNORED_FILE_NAME));
                let writable = random && !matches!(kept, Some(Err(_)));
                (
                    identity,
                    writable,
                    format!("{reason}; kept aside and replaced"),
                )
            }
        };
        let fresh_file = !found.starts_with("credentials");
        let found = if tightened {
            format!("{OPEN_TO_OTHERS}; {found}")
        } else {
            found
        };
        let device_id = identity.device_id();
        self.airplay2 = Some(Airplay2File { identity, writable });
        if let Some(path) = &self.path {
            self.timing = TimingStore::open(path.with_file_name(airplay2_timing::FILE_NAME));
        }
        if fresh_file && let Err(error) = self.save_airplay2() {
            return (
                device_id,
                format!("{found} (the file couldn't be written: {error})"),
            );
        }
        (device_id, found)
    }

    /// The speaker's HomeKit credentials, once [`PairingStore::open_airplay2`]
    /// read them.
    pub fn homekit(&self, id: &str) -> Option<&HomeKitCredentials> {
        self.airplay2.as_ref()?.identity.get(id)
    }

    /// Keeps the speaker's new HomeKit credentials and writes the AirPlay 2
    /// identity file. When it can't be written they are still kept for this
    /// run, and the error says why.
    pub fn insert_homekit(&mut self, id: &str, credentials: HomeKitCredentials) -> io::Result<()> {
        let Some(file) = &mut self.airplay2 else {
            return Err(io::Error::other("the AirPlay 2 identity isn't open"));
        };
        if !file.identity.insert(id, credentials) {
            return Ok(());
        }
        self.save_airplay2()
    }

    /// Forgets HomeKit credentials the speaker turned down (P154), and its
    /// timing entry with them (P217), and writes the AirPlay 2 identity
    /// file; they are gone for this run even when it can't be written.
    pub fn remove_homekit(&mut self, id: &str) -> io::Result<()> {
        let _ = self.timing.forget(id);
        let Some(file) = &mut self.airplay2 else {
            return Ok(());
        };
        if !file.identity.remove(id) {
            return Ok(());
        }
        self.save_airplay2()
    }

    /// Drops the speaker's HomeKit credentials and timing entry from their
    /// files while the hidden AirPlay 2 switch is off and they stay closed:
    /// credentials left there would verify with no PIN once it is on again.
    /// Only a file this build reads is written: a missing one isn't created,
    /// and one that can't be read, decrypted or parsed, or that another
    /// account owns, is left as it is. An open one is left to
    /// [`PairingStore::remove_homekit`]. Its copies kept aside are left to
    /// [`PairingStore::remove`]. Returns whether the file held credentials
    /// for the speaker, and how the write went.
    pub fn forget_airplay2_closed(&mut self, id: &str) -> (bool, io::Result<()>) {
        let Some(path) = self.airplay2_path() else {
            return (false, Ok(()));
        };
        if self.airplay2.is_some() {
            return (false, Ok(()));
        }
        let _ = TimingStore::open(path.with_file_name(airplay2_timing::FILE_NAME)).forget(id);
        let read = match make_private(&path) {
            Ok(_) => read_airplay2(&path, self.sealer),
            Err(reason) => Err(Airplay2Read::Ignored(reason)),
        };
        let Ok(Some(mut identity)) = read else {
            return (false, Ok(()));
        };
        if !identity.remove(id) {
            return (false, Ok(()));
        }
        let written = identity
            .to_bytes()
            .and_then(|plain| self.sealer.seal(&plain))
            .and_then(|sealed| data_folder::write_atomically(&path, &sealed));
        (true, written)
    }

    /// The speaker's AirPlay 2 timing entry (P217), once
    /// [`PairingStore::open_airplay2`] read it.
    pub fn timing(&self, id: &str) -> Option<&TimingMemory> {
        self.timing.get(id)
    }

    /// Keeps the speaker's timing entry (P217) and writes the timing file
    /// when it changed; when it can't be written the entry lasts for this
    /// run.
    pub fn keep_timing(&mut self, id: &str, memory: TimingMemory) -> io::Result<()> {
        self.timing.keep(id, memory)
    }

    /// Drops the speaker's timing entry (P217) and writes the timing file.
    pub fn forget_timing(&mut self, id: &str) -> io::Result<()> {
        self.timing.forget(id)
    }

    fn airplay2_path(&self) -> Option<PathBuf> {
        Some(self.path.as_deref()?.with_file_name(AIRPLAY2_FILE_NAME))
    }

    fn save_airplay2(&self) -> io::Result<()> {
        let (Some(path), Some(file)) = (self.airplay2_path(), &self.airplay2) else {
            return Ok(());
        };
        if !file.writable {
            return Err(io::Error::other(
                "the AirPlay 2 identity file is left as it is",
            ));
        }
        let sealed = self.sealer.seal(&file.identity.to_bytes()?)?;
        data_folder::write_atomically(&path, &sealed)
    }

    /// Writes every entry to the file, through a temporary file renamed over
    /// it; removes the file once nothing is paired. A file ignored at launch
    /// is first kept aside ([`data_folder::keep_aside`] with
    /// [`IGNORED_FILE_NAME`]); while it can't be read, it is
    /// left alone and the error returned.
    fn save(&mut self) -> io::Result<()> {
        self.keep_ignored_aside()?;
        let Some(path) = &self.path else {
            return Ok(());
        };
        if self.entries.is_empty() {
            return match fs::remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
        }
        let plain = encode(&self.entries)?;
        let sealed = self.sealer.seal(&plain)?;
        // Flushed to disk before it replaces the old file, so a power cut
        // leaves the old file or the new one, never an empty one.
        data_folder::write_atomically(path, &sealed)
    }
}

/// Why the AirPlay 2 identity file can't be used (never a secret).
enum Airplay2Read {
    /// It can't be read now, or a newer AudioSync wrote it: never replaced.
    Unreadable(String),
    /// It can't be decrypted or parsed: kept aside and replaced.
    Ignored(String),
}

/// The AirPlay 2 identity in the file at `path`, `None` without a file.
fn read_airplay2(path: &Path, sealer: Sealer) -> Result<Option<Airplay2Identity>, Airplay2Read> {
    let sealed = match data_folder::read_at_launch(path) {
        Ok(sealed) => sealed,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Airplay2Read::Unreadable(format!(
                "the AirPlay 2 identity file can't be read: {error}"
            )));
        }
    };
    let plain = sealer.open(&sealed).map_err(|error| {
        Airplay2Read::Ignored(format!(
            "the AirPlay 2 identity file can't be decrypted for this Windows user: {error}"
        ))
    })?;
    match Airplay2Identity::from_bytes(&plain) {
        Ok(identity) => Ok(Some(identity)),
        Err(Unreadable::Newer(version)) => Err(Airplay2Read::Unreadable(format!(
            "a newer AudioSync wrote the AirPlay 2 identity file (format {version})"
        ))),
        Err(Unreadable::Format) => Err(Airplay2Read::Ignored(
            "the AirPlay 2 identity file isn't in the expected format".to_owned(),
        )),
    }
}

/// Deletes the copies kept aside next to `path` under names made from
/// `name` ([`data_folder::keep_bytes_aside`]) that hold the speaker ID `id`
/// as both files frame it (16-bit little-endian length first), so a
/// forgotten speaker's secret doesn't stay behind. A copy that can't be
/// read or decrypted is left.
fn prune_copies(
    path: &Path,
    name: &str,
    id: &str,
    sealer: Sealer,
    remove: fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let Ok(id_len) = u16::try_from(id.len()) else {
        return Ok(());
    };
    let framed = [&id_len.to_le_bytes()[..], id.as_bytes()].concat();
    let prefix = format!("{}-", name.trim_end_matches(".dat"));
    let Some(folder) = path.parent() else {
        return Ok(());
    };
    let listing = match fs::read_dir(folder) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    let mut result = Ok(());
    for found in listing {
        let copy = found?.path();
        let named = copy
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".dat"));
        if !named {
            continue;
        }
        let Ok(plain) = fs::read(&copy).and_then(|sealed| sealer.open(&sealed)) else {
            continue;
        };
        if plain.windows(framed.len()).any(|window| window == framed) {
            result = result.and(remove(&copy));
        }
    }
    result
}

/// What the log says when [`make_private`] tightened a file or its folder.
const OPEN_TO_OTHERS: &str =
    "the file or its folder was readable by other accounts; set to owner-only";

/// On Linux the file isn't encrypted, so before it is read, a file other
/// accounts may read gets 0600 and its folder 0700; returns whether either
/// needed it. `Err`, read as an ignored file, when another account owns
/// the file. A folder another account owns is left as it is.
#[cfg(target_os = "linux")]
fn make_private(path: &Path) -> Result<bool, String> {
    use crate::os::xdg::tighten;
    let folder = path
        .parent()
        .is_some_and(|folder| tighten(folder, 0o700).unwrap_or(false));
    match tighten(path, 0o600) {
        Ok(file) => Ok(file || folder),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Err(format!(
            "another account owns the file and others may read it: {error}"
        )),
        Err(_) => Ok(folder),
    }
}

/// Elsewhere the file is encrypted for the user.
#[cfg(not(target_os = "linux"))]
fn make_private(_path: &Path) -> Result<bool, String> {
    Ok(false)
}

/// The entries in the file at `path` and how many didn't read: `None`
/// without a file, or why it was ignored.
fn read(path: &Path, sealer: Sealer) -> Result<Option<Decoded>, String> {
    let sealed = match data_folder::read_at_launch(path) {
        Ok(sealed) => sealed,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "the file can't be read, even at a second try: {error}"
            ));
        }
    };
    let plain = sealer
        .open(&sealed)
        .map_err(|error| format!("the file can't be decrypted for this Windows user: {error}"))?;
    decode(&plain)
        .map(Some)
        .ok_or_else(|| "the file isn't in the expected format".to_owned())
}

/// The plaintext of the file.
fn encode(entries: &BTreeMap<String, Credentials>) -> io::Result<Zeroizing<Vec<u8>>> {
    let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "a speaker ID is too long");
    let blobs: Vec<(&String, Zeroizing<Vec<u8>>)> = entries
        .iter()
        .map(|(id, credentials)| (id, credentials.to_bytes()))
        .collect();
    // Allocated once at its full size, so growing never leaves a copy of a
    // secret behind in freed memory.
    let size = MAGIC.len()
        + blobs
            .iter()
            .map(|(id, blob)| 4 + id.len() + blob.len())
            .sum::<usize>();
    let mut plain = Zeroizing::new(Vec::with_capacity(size));
    plain.extend_from_slice(MAGIC);
    for (id, blob) in &blobs {
        let id_len = u16::try_from(id.len()).map_err(|_| too_long())?;
        let blob_len = u16::try_from(blob.len()).map_err(|_| too_long())?;
        plain.extend_from_slice(&id_len.to_le_bytes());
        plain.extend_from_slice(id.as_bytes());
        plain.extend_from_slice(&blob_len.to_le_bytes());
        plain.extend_from_slice(blob);
    }
    Ok(plain)
}

/// The next length-prefixed field of `rest`, which moves past it.
fn take_field<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (len, after) = rest.split_first_chunk::<2>()?;
    let len = usize::from(u16::from_le_bytes(*len));
    let (field, after) = after.split_at_checked(len)?;
    *rest = after;
    Some(field)
}

/// The entries a file held, and how many of its entries didn't read.
type Decoded = (BTreeMap<String, Credentials>, usize);

/// The entries of a plaintext and how many were skipped: one whose ID is
/// empty, repeated or not UTF-8, or whose blob doesn't parse. `None` when
/// the tag or the framing is off.
fn decode(plain: &[u8]) -> Option<Decoded> {
    let mut rest = plain.strip_prefix(MAGIC)?;
    let mut entries = BTreeMap::new();
    let mut skipped = 0;
    while !rest.is_empty() {
        let id = take_field(&mut rest)?;
        let blob = take_field(&mut rest)?;
        let read = std::str::from_utf8(id)
            .ok()
            .filter(|id| !id.is_empty() && !entries.contains_key(*id))
            .zip(Credentials::from_bytes(blob).ok());
        match read {
            Some((id, credentials)) => {
                entries.insert(id.to_owned(), credentials);
            }
            None => skipped += 1,
        }
    }
    Some((entries, skipped))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::data_folder::copies_kept_aside;
    use crate::debug_log::test_dir;

    /// Credentials whose secret is `fill` repeated.
    pub fn credentials(client: &str, fill: u8) -> Credentials {
        Credentials::new(client, [fill; 32]).expect("valid credentials")
    }

    /// A stand-in for DPAPI off Windows: XORs each byte with a mask that
    /// changes with its place, and refuses data that doesn't start with its
    /// tag. A mask that stays the same would turn one speaker's secret into
    /// another's (0x5A into 0xA5), and the test that no secret is on disk
    /// as is would fail.
    struct Scramble;

    fn mask(index: usize) -> u8 {
        (index as u8).wrapping_mul(37) ^ 0x5C
    }

    impl Seal for Scramble {
        fn seal(&self, plain: &[u8]) -> io::Result<Vec<u8>> {
            let mut sealed = b"SCRM".to_vec();
            sealed.extend(plain.iter().enumerate().map(|(i, byte)| byte ^ mask(i)));
            Ok(sealed)
        }

        fn open(&self, sealed: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
            let body = sealed
                .strip_prefix(b"SCRM")
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not sealed"))?;
            Ok(Zeroizing::new(
                body.iter()
                    .enumerate()
                    .map(|(i, byte)| byte ^ mask(i))
                    .collect(),
            ))
        }
    }

    #[test]
    fn the_stand_in_sealer_leaves_no_run_of_one_byte() {
        for fill in [0x00, 0x5A, 0xA5, 0xFF] {
            let sealed = Scramble.seal(&[fill; 32]).unwrap();
            assert!(
                !sealed
                    .windows(8)
                    .any(|w| w.iter().all(|byte| *byte == w[0])),
                "{fill:#04x}"
            );
            assert_eq!(*Scramble.open(&sealed).unwrap(), [fill; 32]);
        }
    }

    /// Makes every delete of a copy kept aside fail, as when another
    /// program holds it.
    pub fn failing_copy_deletes(store: &mut PairingStore) {
        store.remove_copy = |_| Err(io::Error::other("held by another program"));
    }

    /// The sealer the tests use: DPAPI on Windows, the stand-in elsewhere.
    pub fn sealer() -> Sealer {
        if cfg!(windows) {
            crate::os::sealer()
        } else {
            Sealer::new(&Scramble)
        }
    }

    #[test]
    fn credentials_survive_a_restart_and_are_encrypted_on_disk() {
        let path = test_dir("pairings-roundtrip").join(FILE_NAME);
        let (mut store, loaded) = PairingStore::open(path.clone(), sealer());
        assert_eq!(loaded, Loaded::NoFile);
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 0xA5))
            .expect("saved");
        store
            .insert("raop:Den TV", credentials("FEDCBA9876543210", 0x5A))
            .expect("saved");

        let on_disk = fs::read(&path).expect("the file is there");
        let secret = [0xA5u8; 32];
        assert!(
            !on_disk.windows(secret.len()).any(|w| w == secret),
            "the secret isn't in the file as is"
        );
        assert!(
            !on_disk.windows(16).any(|w| w == b"0123456789ABCDEF"),
            "nor the client ID"
        );

        let (store, loaded) = PairingStore::open(path.clone(), sealer());
        assert_eq!(loaded, Loaded::Speakers(2));
        assert_eq!(
            store.get("02005E005301"),
            Some(&credentials("0123456789ABCDEF", 0xA5))
        );
        assert!(store.contains("raop:Den TV"));
        assert!(!store.contains("02005E005302"));
    }

    pub fn homekit(fill: u8) -> HomeKitCredentials {
        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[fill; 64]);
        blob.extend_from_slice(b"receiver");
        HomeKitCredentials::from_bytes(&blob).expect("a credential blob")
    }

    #[test]
    fn the_airplay2_identity_survives_a_restart_sealed_and_only_once_asked_for() {
        let folder = test_dir("pairings-airplay2");
        let path = folder.join(FILE_NAME);
        let identity = folder.join(AIRPLAY2_FILE_NAME);
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 0xA5))
            .expect("saved");
        assert!(!identity.exists(), "untouched while the switch is off");
        assert!(store.insert_homekit("02005E005302", homekit(1)).is_err());

        let (device_id, found) = store.open_airplay2();
        assert_eq!(found, "no file yet");
        assert!(identity.exists(), "the new device ID is written at once");
        store
            .insert_homekit("02005E005302", homekit(0xC3))
            .expect("saved");
        let on_disk = fs::read(&identity).expect("the file is there");
        let secret = [0xC3u8; 32];
        assert!(!on_disk.windows(secret.len()).any(|w| w == secret));

        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        assert_eq!(store.homekit("02005E005302"), None, "read only once asked");
        let (again, found) = store.open_airplay2();
        assert_eq!(again, device_id);
        assert_eq!(found, "credentials for 1 speaker(s)");
        assert_eq!(store.homekit("02005E005302"), Some(&homekit(0xC3)));
        assert!(
            store.contains("02005E005301"),
            "the pairings file is its own"
        );
    }

    #[test]
    fn an_airplay2_identity_that_cant_be_opened_is_kept_aside_and_replaced() {
        let folder = test_dir("pairings-airplay2-ignored");
        let identity = folder.join(AIRPLAY2_FILE_NAME);
        fs::write(&identity, b"not sealed").expect("written");
        let (mut store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        let (device_id, found) = store.open_airplay2();
        assert!(found.ends_with("kept aside and replaced"), "{found}");
        assert_eq!(
            copies_kept_aside(&folder, "airplay2-identity.ignored"),
            [b"not sealed".to_vec()]
        );
        let (mut store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        assert_eq!(store.open_airplay2().0, device_id);
    }

    // covers: S224
    #[test]
    fn forgetting_with_airplay2_closed_leaves_an_identity_file_it_cant_use_as_it_is() {
        let folder = test_dir("pairings-airplay2-forget-unusable");
        let path = folder.join(AIRPLAY2_FILE_NAME);
        let mut newer = audiosync_core::airplay2_identity::MAGIC.to_vec();
        newer.push(u8::MAX);
        newer.extend_from_slice(b"\x0c\x0002005E005302");
        for bytes in [
            b"not sealed".to_vec(),
            sealer().seal(b"not an identity").expect("sealed"),
            sealer().seal(&newer).expect("sealed"),
        ] {
            fs::write(&path, &bytes).expect("written");
            let (mut store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
            let (held, written) = store.forget_airplay2_closed("02005E005302");
            assert!(!held && written.is_ok());
            assert_eq!(fs::read(&path).expect("read"), bytes, "left as it is");
            assert!(copies_kept_aside(&folder, "airplay2-identity.ignored").is_empty());
        }
    }

    // covers: S224
    #[test]
    fn forgetting_deletes_the_identity_copies_kept_aside_that_hold_the_speaker() {
        let folder = test_dir("pairings-airplay2-forget-copies");
        let path = folder.join(AIRPLAY2_FILE_NAME);
        let sealed_with = |ids: &[&str]| {
            let mut identity = Airplay2Identity::new([1; 6]);
            for (fill, id) in ids.iter().enumerate() {
                identity.insert(id, homekit(fill as u8));
            }
            sealer()
                .seal(&identity.to_bytes().expect("bytes"))
                .expect("sealed")
        };
        let both = sealed_with(&["02005E005301", "02005E005302"]);
        let other = sealed_with(&["02005E005301"]);
        for bytes in [&both, &other, &b"not sealed".to_vec()] {
            data_folder::keep_bytes_aside(&path, AIRPLAY2_IGNORED_FILE_NAME, bytes).expect("kept");
        }
        fs::write(&path, &both).expect("written");
        let (mut store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        let (held, written) = store.forget_airplay2_closed("02005E005302");
        assert!(held && written.is_ok());
        assert!(!store.remove("02005E005302").expect("deleted"));
        let mut kept = copies_kept_aside(&folder, "airplay2-identity.ignored");
        kept.sort();
        let mut expected = vec![other.clone(), b"not sealed".to_vec()];
        expected.sort();
        assert_eq!(kept, expected, "only the copies without it stay");

        // With the identity open the file is left to remove_homekit, the
        // copies still go.
        data_folder::keep_bytes_aside(&path, AIRPLAY2_IGNORED_FILE_NAME, &both).expect("kept");
        store.open_airplay2();
        assert!(!store.forget_airplay2_closed("02005E005302").0);
        assert!(!store.remove("02005E005302").expect("deleted"));
        assert_eq!(
            copies_kept_aside(&folder, "airplay2-identity.ignored").len(),
            2
        );
    }

    // covers: S224
    #[test]
    fn forgetting_deletes_the_pairings_copies_kept_aside_that_hold_the_speaker() {
        let folder = test_dir("pairings-forget-copies");
        let path = folder.join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 1))
            .expect("saved");
        let other = fs::read(&path).expect("written");
        store
            .insert("02005E005302", credentials("0123456789ABCDEE", 2))
            .expect("saved");
        let both = fs::read(&path).expect("written");
        for bytes in [&both, &other, &b"not sealed".to_vec()] {
            data_folder::keep_bytes_aside(&path, IGNORED_FILE_NAME, bytes).expect("kept");
        }
        fs::write(folder.join("airplay-pairings.ignored-1.bak"), &both).expect("written");
        assert!(store.remove("02005E005302").expect("saved"));
        let mut kept = copies_kept_aside(&folder, "airplay-pairings.ignored");
        kept.sort();
        let mut expected = vec![other.clone(), b"not sealed".to_vec(), both.clone()];
        expected.sort();
        assert_eq!(
            kept, expected,
            "the copies without it and the misnamed one stay"
        );

        // Also once the file no longer holds it.
        data_folder::keep_bytes_aside(&path, IGNORED_FILE_NAME, &both).expect("kept");
        assert!(!store.remove("02005E005302").expect("saved"));
        assert_eq!(
            copies_kept_aside(&folder, "airplay-pairings.ignored").len(),
            3
        );
    }

    #[test]
    fn forgetting_the_last_pairing_removes_the_file() {
        let path = test_dir("pairings-forget").join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 1))
            .unwrap();
        store
            .insert("02005E005302", credentials("0123456789ABCDEE", 2))
            .unwrap();
        assert!(store.remove("02005E005301").unwrap());
        assert!(!store.remove("02005E005301").unwrap());
        let (reopened, loaded) = PairingStore::open(path.clone(), sealer());
        assert_eq!(loaded, Loaded::Speakers(1));
        assert!(reopened.contains("02005E005302"));
        assert!(store.remove("02005E005302").unwrap());
        assert!(!path.exists(), "nothing paired, no file");
    }

    #[test]
    fn a_corrupted_or_foreign_file_is_ignored_and_replaced_by_the_next_pairing() {
        let dir = test_dir("pairings-corrupt");
        let path = dir.join(FILE_NAME);
        fs::write(&path, b"not sealed at all").unwrap();
        let (mut store, loaded) = PairingStore::open(path.clone(), sealer());
        assert!(
            matches!(&loaded, Loaded::Ignored(reason) if reason.contains("decrypted")),
            "{loaded:?}"
        );
        assert!(!store.contains("02005E005301"));
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 3))
            .unwrap();
        assert_eq!(
            PairingStore::open(path.clone(), sealer()).1,
            Loaded::Speakers(1)
        );

        // Sealed correctly, but not what the store writes.
        let sealed = sealer().seal(b"AudioSync pairings 1\n\x05\x00abc").unwrap();
        fs::write(&path, sealed).unwrap();
        let (_, loaded) = PairingStore::open(path.clone(), sealer());
        assert_eq!(
            loaded,
            Loaded::Ignored("the file isn't in the expected format".to_owned())
        );
    }

    /// `id` and `blob` as the file frames an entry.
    fn entry(id: &[u8], blob: &[u8]) -> Vec<u8> {
        let mut framed = (id.len() as u16).to_le_bytes().to_vec();
        framed.extend_from_slice(id);
        framed.extend_from_slice(&(blob.len() as u16).to_le_bytes());
        framed.extend_from_slice(blob);
        framed
    }

    #[test]
    fn an_unreadable_entry_costs_only_its_own_pairing_and_the_file_is_kept_aside_first() {
        let dir = test_dir("pairings-one-bad-entry");
        let path = dir.join(FILE_NAME);
        let first = credentials("0123456789ABCDEF", 1);
        let second = credentials("0123456789ABCDEE", 2);
        let mut plain = MAGIC.to_vec();
        plain.extend(entry(b"02005E005301", &first.to_bytes()));
        plain.extend(entry(b"02005E005309", b"\xEEa blob a later build wrote"));
        plain.extend(entry(b"", &first.to_bytes()));
        plain.extend(entry(b"\xFF\xFE", &first.to_bytes()));
        plain.extend(entry(b"02005E005301", &second.to_bytes()));
        plain.extend(entry(b"02005E005302", &second.to_bytes()));
        let original = sealer().seal(&plain).unwrap();
        fs::write(&path, &original).unwrap();

        let (mut store, loaded) = PairingStore::open(path.clone(), sealer());
        assert_eq!(
            loaded,
            Loaded::Partly {
                speakers: 2,
                skipped: 4
            }
        );
        // The notice says every pairing was lost, which isn't so here.
        assert_eq!(store.take_kept_aside(), None);
        assert_eq!(store.get("02005E005301"), Some(&first));
        assert_eq!(store.get("02005E005302"), Some(&second));
        assert!(!store.contains("02005E005309"));
        assert_eq!(
            copies_kept_aside(&dir, "airplay-pairings.ignored"),
            [original],
            "the file is copied before anything replaces it"
        );
        assert_eq!(
            PairingStore::open(path.clone(), sealer()).1,
            Loaded::Speakers(2)
        );
    }

    /// The bytes of a store file holding one pairing.
    fn good_file(name: &str) -> Vec<u8> {
        let path = test_dir(name).join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 9))
            .expect("saved");
        fs::read(&path).expect("written")
    }

    #[test]
    fn a_file_that_cant_be_read_at_launch_is_never_saved_over() {
        let dir = test_dir("pairings-unreadable");
        let path = dir.join(FILE_NAME);
        let kept = || copies_kept_aside(&dir, "airplay-pairings.ignored");
        // A folder where the file should be: reading it fails, but not with
        // "not found", as when another program holds the file at launch.
        fs::create_dir(&path).unwrap();
        let (mut store, loaded) = PairingStore::open(path.clone(), sealer());
        assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
        // Still unreadable: the new pairing is kept for this run only.
        assert!(
            store
                .insert("02005E005302", credentials("0123456789ABCDEE", 4))
                .is_err()
        );
        assert!(store.contains("02005E005302"));
        assert!(path.is_dir());

        // Readable again, with the pairings made before: kept aside before
        // the first write replaces the file.
        let good = good_file("pairings-unreadable-good");
        fs::remove_dir(&path).unwrap();
        fs::write(&path, &good).unwrap();
        store
            .insert("02005E005303", credentials("0123456789ABCDED", 5))
            .expect("saved");
        assert_eq!(kept(), std::slice::from_ref(&good));
        assert_eq!(
            PairingStore::open(path.clone(), sealer()).1,
            Loaded::Speakers(2)
        );
        // Once replaced, the next write leaves the copy alone.
        store.remove("02005E005303").expect("saved");
        assert_eq!(kept(), [good]);
    }

    #[test]
    fn pairings_unreadable_at_two_launches_in_a_row_are_never_lost() {
        let dir = test_dir("pairings-unreadable-twice");
        let path = dir.join(FILE_NAME);
        let users = good_file("pairings-unreadable-twice-good");
        fs::write(&path, &users).unwrap();
        // A launch while another program holds the file (a folder stands in
        // its place), which lets go of it before the first pairing.
        let launch = |fill: u8| {
            let held = dir.join("held");
            fs::rename(&path, &held).unwrap();
            fs::create_dir(&path).unwrap();
            let (mut store, loaded) = PairingStore::open(path.clone(), sealer());
            assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
            fs::remove_dir(&path).unwrap();
            fs::rename(&held, &path).unwrap();
            store
                .insert("02005E005304", credentials("0123456789ABCDEC", fill))
                .expect("saved");
        };
        launch(6);
        launch(7);
        // The user's pairings are still kept, next to the first launch's.
        let kept = copies_kept_aside(&dir, "airplay-pairings.ignored");
        assert!(kept.contains(&users), "{} copies", kept.len());
        assert_eq!(kept.len(), 2);
        assert_eq!(
            PairingStore::open(path.clone(), sealer()).1,
            Loaded::Speakers(1)
        );
    }

    #[test]
    fn a_file_that_cant_be_decrypted_is_kept_aside_before_the_next_pairing() {
        let dir = test_dir("pairings-kept");
        let path = dir.join(FILE_NAME);
        fs::write(&path, b"sealed for another user").unwrap();
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 3))
            .expect("saved");
        assert_eq!(
            copies_kept_aside(&dir, "airplay-pairings.ignored"),
            [b"sealed for another user".to_vec()]
        );
    }

    #[test]
    fn the_plaintext_is_checked_field_by_field() {
        let mut entries = BTreeMap::new();
        entries.insert("a".to_owned(), credentials("0123456789ABCDEF", 7));
        let plain = encode(&entries).unwrap();
        assert_eq!(
            decode(&plain).map(|(e, skipped)| (e.len(), skipped)),
            Some((1, 0))
        );
        // Every truncation is refused.
        for len in MAGIC.len() + 1..plain.len() {
            assert!(decode(&plain[..len]).is_none(), "{len} bytes");
        }
        assert!(decode(b"something else").is_none());
        assert_eq!(
            decode(MAGIC).map(|(e, skipped)| (e.len(), skipped)),
            Some((0, 0))
        );
    }

    #[test]
    fn the_load_summary_never_holds_a_secret() {
        assert_eq!(
            Loaded::Speakers(1).to_string(),
            "credentials loaded for 1 speaker"
        );
        let ignored = Loaded::Ignored("the file isn't in the expected format".to_owned());
        assert!(ignored.to_string().contains("pair again"));
        let store_debug = format!("{:?}", {
            let mut store = PairingStore::in_memory();
            store
                .insert("02005E005301", credentials("0123456789ABCDEF", 0xA5))
                .unwrap();
            store
        });
        assert!(!store_debug.contains("a5") && !store_debug.contains("165"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_others_may_read_is_made_owner_only_and_still_loaded() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = |path: &Path| fs::metadata(path).expect("there").mode() & 0o777;
        let dir = test_dir("pairings-owner-only");
        let path = dir.join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path.clone(), crate::os::sealer());
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 0xA5))
            .expect("saved");
        assert_eq!(mode(&path), 0o600);
        assert!(fs::read(&path).expect("there").starts_with(MAGIC));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        let (store, loaded) = PairingStore::open(path.clone(), crate::os::sealer());
        assert_eq!(loaded, Loaded::Speakers(1));
        assert_eq!(store.tightened(), Some(OPEN_TO_OTHERS));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);

        let (store, loaded) = PairingStore::open(path, crate::os::sealer());
        assert_eq!(loaded, Loaded::Speakers(1));
        assert_eq!(store.tightened(), None);
    }
}
