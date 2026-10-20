//! The credentials of the Apple TVs paired through AudioSync's own AirPlay
//! sender on the Mac, kept per receiver so a PIN is asked only once.
//!
//! The sender hands each pairing's credentials to its host and never stores
//! them itself (P54); this store keeps them in one file, by receiver ID,
//! readable and writable by the current user only (mode 0600). The macOS
//! app keeps it at `~/Library/Application Support/AudioSync/airplay-pairings.dat`
//! and not in the Keychain, which would ask for access again after every
//! ad-hoc or locally signed rebuild; it keeps that folder out of Time
//! Machine backups, since the file is plaintext. Passwords are never stored
//! here: the host hands them over for one run only.
//!
//! The file holds a magic line, a format version byte, then per receiver its
//! ID and the credential blob the sender defines
//! ([`Credentials::to_bytes`]), each with a 16-bit little-endian length. A
//! file that can't be read or parsed is ignored as a whole: its receivers
//! are paired again, and the next pairing writes a fresh file. A file of a
//! newer format version is ignored too but left as it is, so a newer
//! AudioSync finds its pairings again; pairings made meanwhile last until
//! the app quits. Every write goes to a temporary file created new, with
//! mode 0600, flushed to disk and renamed over the old file, so a crash
//! leaves the old file or the new one, never a partial one.
//!
//! The plaintext lives only in buffers that are wiped when dropped, and
//! nothing here logs a secret: callers log what [`Loaded`] and the I/O
//! errors say, which never hold one.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use audiosync_airplay::pairing::Credentials;
use zeroize::Zeroizing;

/// The file's first bytes.
pub const MAGIC: &[u8] = b"AudioSync AirPlay pairings\n";

/// The format version this build writes and reads.
pub const VERSION: u8 = 1;

/// What opening the file found, for the debug log. Never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// No file yet: nothing is paired.
    NoFile,
    /// The credentials of this many receivers.
    Receivers(usize),
    /// The file was ignored, for this reason; the next pairing replaces it.
    Ignored(String),
    /// A newer AudioSync wrote the file (its format version): it is left as
    /// it is, and pairings made now last until the app quits.
    Newer(u8),
}

impl fmt::Display for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Loaded::NoFile => f.write_str("no pairings file yet"),
            Loaded::Receivers(count) => write!(f, "{count} paired receiver(s)"),
            Loaded::Ignored(reason) => write!(f, "pairings file ignored: {reason}"),
            Loaded::Newer(version) => write!(
                f,
                "pairings file left as it is: a newer AudioSync wrote it (format {version})"
            ),
        }
    }
}

/// The paired receivers' credentials, by receiver ID, and the file they
/// live in.
pub struct PairingStore {
    /// The file, or `None` for a store that lives in memory only.
    path: Option<PathBuf>,
    /// False when the file belongs to a newer AudioSync.
    writable: bool,
    entries: BTreeMap<String, Credentials>,
}

impl fmt::Debug for PairingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The IDs only: never a credential.
        f.debug_struct("PairingStore")
            .field("path", &self.path)
            .field("writable", &self.writable)
            .field("receivers", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for PairingStore {
    fn default() -> PairingStore {
        PairingStore::in_memory()
    }
}

impl PairingStore {
    /// A store with no file, which forgets everything when dropped.
    pub fn in_memory() -> PairingStore {
        PairingStore {
            path: None,
            writable: true,
            entries: BTreeMap::new(),
        }
    }

    /// The store kept in the file at `path`, with what was found there.
    pub fn open(path: PathBuf) -> (PairingStore, Loaded) {
        let (entries, writable, loaded) = match read(&path) {
            Ok(Some(entries)) => {
                let count = entries.len();
                (entries, true, Loaded::Receivers(count))
            }
            Ok(None) => (BTreeMap::new(), true, Loaded::NoFile),
            Err(Unreadable::Newer(version)) => (BTreeMap::new(), false, Loaded::Newer(version)),
            Err(Unreadable::Ignored(reason)) => (BTreeMap::new(), true, Loaded::Ignored(reason)),
        };
        let store = PairingStore {
            path: Some(path),
            writable,
            entries,
        };
        (store, loaded)
    }

    /// How many receivers are paired.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is paired.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the receiver `id` is paired.
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// The receiver's credentials, if it is paired.
    pub fn get(&self, id: &str) -> Option<&Credentials> {
        self.entries.get(id)
    }

    /// Keeps the receiver's new credentials and writes the file. When the
    /// file can't be written they are still kept until the store is
    /// dropped, and the error says why.
    pub fn insert(&mut self, id: &str, credentials: Credentials) -> io::Result<()> {
        self.entries.insert(id.to_owned(), credentials);
        self.save()
    }

    /// Forgets the receiver's credentials and writes the file. Returns
    /// whether it was paired; when the file can't be written they are
    /// forgotten all the same, and the error says why.
    pub fn remove(&mut self, id: &str) -> io::Result<bool> {
        if self.entries.remove(id).is_none() {
            return Ok(false);
        }
        self.save().map(|()| true)
    }

    /// Writes every entry to the file, through a temporary file renamed over
    /// it; removes the file once nothing is paired.
    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.writable {
            return Err(io::Error::other(
                "a newer AudioSync wrote the pairings file, which is left as it is",
            ));
        }
        if self.entries.is_empty() {
            return match fs::remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
        }
        let plain = encode(&self.entries)?;
        write_atomically(path, &plain)
    }
}

/// Why the file's entries can't be used.
enum Unreadable {
    Newer(u8),
    Ignored(String),
}

/// The entries in the file at `path`, `None` without a file.
fn read(path: &Path) -> Result<Option<BTreeMap<String, Credentials>>, Unreadable> {
    let plain = match fs::read(path) {
        Ok(bytes) => Zeroizing::new(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Unreadable::Ignored(format!(
                "the file can't be read: {error}"
            )));
        }
    };
    let format = || Unreadable::Ignored("the file isn't in the expected format".to_owned());
    let Some([version, rest @ ..]) = plain.strip_prefix(MAGIC) else {
        return Err(format());
    };
    if *version > VERSION {
        return Err(Unreadable::Newer(*version));
    }
    if *version != VERSION {
        return Err(format());
    }
    decode(rest).map(Some).ok_or_else(format)
}

/// The file's contents for `entries`.
fn encode(entries: &BTreeMap<String, Credentials>) -> io::Result<Zeroizing<Vec<u8>>> {
    let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "a receiver ID is too long");
    let blobs: Vec<(&String, Zeroizing<Vec<u8>>)> = entries
        .iter()
        .map(|(id, credentials)| (id, credentials.to_bytes()))
        .collect();
    // Allocated once at its full size, so growing never leaves a copy of a
    // secret behind in freed memory.
    let size = MAGIC.len()
        + 1
        + blobs
            .iter()
            .map(|(id, blob)| 4 + id.len() + blob.len())
            .sum::<usize>();
    let mut plain = Zeroizing::new(Vec::with_capacity(size));
    plain.extend_from_slice(MAGIC);
    plain.push(VERSION);
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

/// The entries after the version byte, or `None` when anything is off.
fn decode(mut rest: &[u8]) -> Option<BTreeMap<String, Credentials>> {
    let mut entries = BTreeMap::new();
    while !rest.is_empty() {
        let id = std::str::from_utf8(take_field(&mut rest)?).ok()?.to_owned();
        let credentials = Credentials::from_bytes(take_field(&mut rest)?).ok()?;
        if id.is_empty() || entries.insert(id, credentials).is_some() {
            return None;
        }
    }
    Some(entries)
}

/// A new file, readable and writable by its owner only.
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Writes `bytes` to `path` through `<path>.tmp`, created new (a leftover
/// from a crash is removed first), flushed to disk and renamed over `path`.
/// The folder is created, for its owner only, when missing.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(folder) = path
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
    {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(folder)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut file = match create_private(&temporary) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(&temporary)?;
            create_private(&temporary)?
        }
        other => other?,
    };
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|()| fs::rename(&temporary, path)) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}
