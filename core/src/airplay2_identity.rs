//! The AirPlay 2 identity a host keeps across launches: the random device
//! ID of the session SETUP (P5, P43), the HomeKit credentials of each
//! receiver paired on the AirPlay 2 path (P152) and the timing entry of
//! each receiver (P217), by receiver ID, so a relaunch verifies instead of
//! pairing again and asks for the timing the receiver took last time.
//!
//! [`Airplay2Identity`] is the contents and their byte form, which the
//! Windows app seals with DPAPI next to its pairings file. [`IdentityStore`]
//! keeps them for the macOS app in a file of their own, next to the pairings
//! file and handled the same way ([`crate::airplay_pairings`]): mode 0600,
//! written through a temporary file renamed over the old one, a file that
//! can't be read or parsed ignored as a whole and replaced by the next
//! write, a file of a newer format version left as it is.
//!
//! The byte form holds a magic line, a format version byte, the 6-byte
//! device ID, then per receiver its ID and the credential blob the sender
//! defines ([`HomeKitCredentials::to_bytes`]), each with a 16-bit
//! little-endian length. Version 2 puts the number of credential records,
//! 16-bit little-endian, before them, and after them per receiver its ID
//! and its timing entry ([`TimingMemory::to_bytes`]), with the same
//! lengths. An identity without timing entries is written as version 1, so
//! an older AudioSync still reads it. The plaintext lives only in buffers wiped when
//! dropped, and neither `Debug` nor [`Loaded`] shows a secret.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::PathBuf;

use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::sender::{TimingMemory, random_airplay2_device_id};
use zeroize::Zeroizing;

use crate::airplay_pairings::{
    Access, Contents, Loaded, read_private, take_field, write_atomically,
};

/// The byte form's first bytes.
pub const MAGIC: &[u8] = b"AudioSync AirPlay 2 identity\n";

/// The newest format version this build writes and reads.
pub const VERSION: u8 = 2;

/// The version written when no receiver has a timing entry.
const VERSION_WITHOUT_TIMING: u8 = 1;

/// Why bytes aren't an identity this build can use. Never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreadable {
    /// A newer AudioSync wrote them, in this format version.
    Newer(u8),
    /// Not in the expected format.
    Format,
}

/// The device ID, and the HomeKit credentials and timing entries by
/// receiver ID.
#[derive(Clone)]
pub struct Airplay2Identity {
    device_id: [u8; 6],
    homekit: BTreeMap<String, HomeKitCredentials>,
    timing: BTreeMap<String, TimingMemory>,
}

impl fmt::Debug for Airplay2Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Airplay2Identity")
            .field("receivers", &self.homekit.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Airplay2Identity {
    /// An identity with this device ID and no receiver paired.
    pub fn new(device_id: [u8; 6]) -> Airplay2Identity {
        Airplay2Identity {
            device_id,
            homekit: BTreeMap::new(),
            timing: BTreeMap::new(),
        }
    }

    pub fn device_id(&self) -> [u8; 6] {
        self.device_id
    }

    /// How many receivers it holds credentials for.
    pub fn len(&self) -> usize {
        self.homekit.len()
    }

    pub fn is_empty(&self) -> bool {
        self.homekit.is_empty()
    }

    /// The receiver's HomeKit credentials, if it is paired.
    pub fn get(&self, id: &str) -> Option<&HomeKitCredentials> {
        self.homekit.get(id)
    }

    /// Keeps the receiver's credentials; false when it held these already.
    pub fn insert(&mut self, id: &str, credentials: HomeKitCredentials) -> bool {
        self.homekit.insert(id.to_owned(), credentials.clone()) != Some(credentials)
    }

    /// Forgets the receiver's credentials and its timing entry, as a
    /// forgotten pairing does (P217); false when it held neither.
    pub fn remove(&mut self, id: &str) -> bool {
        let homekit = self.homekit.remove(id).is_some();
        let timing = self.timing.remove(id).is_some();
        homekit || timing
    }

    /// The receiver's timing entry (P217), if one is kept.
    pub fn timing(&self, id: &str) -> Option<&TimingMemory> {
        self.timing.get(id)
    }

    /// Keeps the receiver's timing entry; false when it held this one
    /// already.
    pub fn keep_timing(&mut self, id: &str, memory: TimingMemory) -> bool {
        self.timing.insert(id.to_owned(), memory.clone()) != Some(memory)
    }

    /// Drops the receiver's timing entry; false when it held none.
    pub fn forget_timing(&mut self, id: &str) -> bool {
        self.timing.remove(id).is_some()
    }

    /// The byte form. It holds the credentials' seeds, so a host keeps it
    /// where secrets go.
    pub fn to_bytes(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        let blobs: Vec<(&String, Zeroizing<Vec<u8>>)> = self
            .homekit
            .iter()
            .map(|(id, credentials)| (id, credentials.to_bytes()))
            .collect();
        let entries: Vec<(&String, Vec<u8>)> = self
            .timing
            .iter()
            .map(|(id, memory)| (id, memory.to_bytes()))
            .collect();
        let with_timing = !entries.is_empty();
        // Allocated once at its full size, so growing never leaves a copy of
        // a secret behind in freed memory.
        let size = MAGIC.len()
            + 1
            + self.device_id.len()
            + if with_timing { 2 } else { 0 }
            + blobs
                .iter()
                .map(|(id, blob)| 4 + id.len() + blob.len())
                .sum::<usize>()
            + entries
                .iter()
                .map(|(id, entry)| 4 + id.len() + entry.len())
                .sum::<usize>();
        let mut bytes = Zeroizing::new(Vec::with_capacity(size));
        bytes.extend_from_slice(MAGIC);
        if with_timing {
            bytes.push(VERSION);
            bytes.extend_from_slice(&self.device_id);
            let count = u16::try_from(blobs.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "too many paired receivers")
            })?;
            bytes.extend_from_slice(&count.to_le_bytes());
        } else {
            bytes.push(VERSION_WITHOUT_TIMING);
            bytes.extend_from_slice(&self.device_id);
        }
        for (id, blob) in &blobs {
            push_record(&mut bytes, id, blob)?;
        }
        for (id, entry) in &entries {
            push_record(&mut bytes, id, entry)?;
        }
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Airplay2Identity, Unreadable> {
        let Some([version, rest @ ..]) = bytes.strip_prefix(MAGIC) else {
            return Err(Unreadable::Format);
        };
        if *version > VERSION {
            return Err(Unreadable::Newer(*version));
        }
        if *version < VERSION_WITHOUT_TIMING {
            return Err(Unreadable::Format);
        }
        let (device_id, mut rest) = rest.split_first_chunk::<6>().ok_or(Unreadable::Format)?;
        let mut identity = Airplay2Identity::new(*device_id);
        // Version 1 holds credential records only.
        let mut credential_records = None;
        if *version > VERSION_WITHOUT_TIMING {
            let (count, after) = rest.split_first_chunk::<2>().ok_or(Unreadable::Format)?;
            credential_records = Some(usize::from(u16::from_le_bytes(*count)));
            rest = after;
        }
        while !rest.is_empty() {
            let id = take_field(&mut rest)
                .and_then(|id| std::str::from_utf8(id).ok())
                .filter(|id| !id.is_empty())
                .ok_or(Unreadable::Format)?;
            let field = take_field(&mut rest).ok_or(Unreadable::Format)?;
            let timing_record =
                credential_records.is_some_and(|count| identity.homekit.len() == count);
            let fresh = if timing_record {
                let memory = TimingMemory::from_bytes(field).ok_or(Unreadable::Format)?;
                identity.timing.insert(id.to_owned(), memory).is_none()
            } else {
                let credentials =
                    HomeKitCredentials::from_bytes(field).map_err(|_| Unreadable::Format)?;
                identity
                    .homekit
                    .insert(id.to_owned(), credentials)
                    .is_none()
            };
            if !fresh {
                return Err(Unreadable::Format);
            }
        }
        if credential_records.is_some_and(|count| identity.homekit.len() != count) {
            return Err(Unreadable::Format);
        }
        Ok(identity)
    }
}

/// Appends a receiver ID and its field, each after its 16-bit
/// little-endian length.
fn push_record(bytes: &mut Vec<u8>, id: &str, field: &[u8]) -> io::Result<()> {
    let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "a receiver ID is too long");
    let id_len = u16::try_from(id.len()).map_err(|_| too_long())?;
    let field_len = u16::try_from(field.len()).map_err(|_| too_long())?;
    bytes.extend_from_slice(&id_len.to_le_bytes());
    bytes.extend_from_slice(id.as_bytes());
    bytes.extend_from_slice(&field_len.to_le_bytes());
    bytes.extend_from_slice(field);
    Ok(())
}

/// The identity and the file it lives in, for the macOS app.
pub struct IdentityStore {
    /// The file, or `None` for a store that lives in memory only.
    path: Option<PathBuf>,
    /// False when the file belongs to a newer AudioSync, or no random device
    /// ID could be made: nothing is written then.
    writable: bool,
    /// How private the file was found.
    access: Access,
    identity: Airplay2Identity,
}

impl fmt::Debug for IdentityStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityStore")
            .field("path", &self.path)
            .field("writable", &self.writable)
            .field("access", &self.access)
            .field("identity", &self.identity)
            .finish()
    }
}

impl Default for IdentityStore {
    fn default() -> IdentityStore {
        IdentityStore::in_memory()
    }
}

/// A fresh identity, and whether its device ID came from the random source.
fn fresh() -> (Airplay2Identity, bool) {
    match random_airplay2_device_id() {
        Some(id) => (Airplay2Identity::new(id), true),
        None => (Airplay2Identity::new([0; 6]), false),
    }
}

fn left_as_it_is(error: &std::io::Error) -> Loaded {
    Loaded::Ignored(format!(
        "the file can't be read, so it is left as it is: {error}"
    ))
}

impl IdentityStore {
    /// A store with no file and a fresh device ID, which forgets everything
    /// when dropped.
    pub fn in_memory() -> IdentityStore {
        IdentityStore {
            path: None,
            writable: true,
            access: Access::Private,
            identity: fresh().0,
        }
    }

    /// The store kept in the file at `path`, with what was found there. A
    /// new device ID is written at once when the file is missing or was
    /// ignored, so it stays the same from then on (P5). A file that can't
    /// be read now, or that a newer AudioSync wrote, is never written over.
    /// A file other accounts may read or write is made owner-only first;
    /// one another account owns is ignored and replaced ([`read_private`]).
    pub fn open(path: PathBuf) -> (IdentityStore, Loaded) {
        let mut access = Access::Unread;
        let (identity, writable, loaded) = match read_private(&path) {
            Ok(Contents::Foreign(reason)) => {
                let (identity, random) = fresh();
                (identity, random, Loaded::Ignored(reason))
            }
            Ok(Contents::Read(bytes, found)) => {
                access = found;
                match Airplay2Identity::from_bytes(&bytes) {
                    Ok(identity) => {
                        let count = identity.len();
                        (identity, true, Loaded::Receivers(count))
                    }
                    Err(Unreadable::Newer(version)) => (fresh().0, false, Loaded::Newer(version)),
                    Err(Unreadable::Format) => {
                        let (identity, random) = fresh();
                        let reason = "the file isn't in the expected format".to_owned();
                        (identity, random, Loaded::Ignored(reason))
                    }
                }
            }
            Ok(Contents::Missing) => {
                access = Access::Private;
                let (identity, random) = fresh();
                (identity, random, Loaded::NoFile)
            }
            // A permission or disk error may pass, and the file may still
            // hold the device ID and credentials.
            Ok(Contents::Failed(error, found)) => {
                access = found;
                (fresh().0, false, left_as_it_is(&error))
            }
            Err(error) => (fresh().0, false, left_as_it_is(&error)),
        };
        let store = IdentityStore {
            path: Some(path),
            writable,
            access,
            identity,
        };
        if store.writable && matches!(loaded, Loaded::NoFile | Loaded::Ignored(_)) {
            // When it can't be written now, the next pairing tries again.
            let _ = store.save();
        }
        (store, loaded)
    }

    /// How private the file was found when the store was opened.
    pub fn access(&self) -> Access {
        self.access
    }

    pub fn device_id(&self) -> [u8; 6] {
        self.identity.device_id()
    }

    pub fn get(&self, id: &str) -> Option<&HomeKitCredentials> {
        self.identity.get(id)
    }

    /// Keeps the receiver's new credentials and writes the file. When the
    /// file can't be written they are still kept until the store is
    /// dropped, and the error says why.
    pub fn insert(&mut self, id: &str, credentials: HomeKitCredentials) -> io::Result<()> {
        if self.identity.insert(id, credentials) {
            self.save()
        } else {
            Ok(())
        }
    }

    /// Forgets the receiver's credentials and writes the file; when it can't
    /// be written they are forgotten until the store is dropped all the same.
    pub fn remove(&mut self, id: &str) -> io::Result<()> {
        if self.identity.remove(id) {
            self.save()
        } else {
            Ok(())
        }
    }

    pub fn timing(&self, id: &str) -> Option<&TimingMemory> {
        self.identity.timing(id)
    }

    /// Keeps the receiver's timing entry (P217) and writes the file when it
    /// changed; when it can't be written the entry lasts until the store is
    /// dropped.
    pub fn keep_timing(&mut self, id: &str, memory: TimingMemory) -> io::Result<()> {
        if self.identity.keep_timing(id, memory) {
            self.save()
        } else {
            Ok(())
        }
    }

    /// Drops the receiver's timing entry and writes the file.
    pub fn forget_timing(&mut self, id: &str) -> io::Result<()> {
        if self.identity.forget_timing(id) {
            self.save()
        } else {
            Ok(())
        }
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.writable {
            return Err(io::Error::other(
                "the AirPlay 2 identity file is left as it is",
            ));
        }
        write_atomically(path, &self.identity.to_bytes()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn credentials(fill: u8) -> HomeKitCredentials {
        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[fill; 64]);
        blob.extend_from_slice(b"receiver");
        HomeKitCredentials::from_bytes(&blob).expect("a credential blob")
    }

    fn scratch(name: &str) -> PathBuf {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join("airplay2-identity-store");
        fs::create_dir_all(&folder).expect("a scratch folder");
        let path = folder.join(name);
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn the_device_id_and_credentials_survive_a_relaunch() {
        let path = scratch("relaunch.dat");
        let (mut store, loaded) = IdentityStore::open(path.clone());
        assert_eq!(loaded, Loaded::NoFile);
        assert!(path.exists(), "the new device ID is written at once");
        let device_id = store.device_id();
        store.insert("A1", credentials(7)).expect("written");
        let (again, loaded) = IdentityStore::open(path.clone());
        assert_eq!(loaded, Loaded::Receivers(1));
        assert_eq!(again.device_id(), device_id);
        assert_eq!(again.get("A1"), Some(&credentials(7)));
        let text = format!("{again:?}");
        assert!(text.contains("A1") && !text.contains(&format!("{:?}", [7u8; 32])));
        let _ = fs::remove_file(&path);
    }

    fn ptp_entry(srcvers: &str) -> TimingMemory {
        TimingMemory {
            timing: audiosync_airplay::ap2::Timing::Ptp,
            features: 0x1_0240_0000_0000,
            srcvers: Some(srcvers.to_owned()),
        }
    }

    #[test]
    fn timing_entries_survive_a_relaunch_and_go_with_a_forgotten_pairing() {
        let path = scratch("timing.dat");
        let (mut store, _) = IdentityStore::open(path.clone());
        store.insert("A1", credentials(7)).expect("written");
        let version_at = MAGIC.len();
        let written = fs::read(&path).expect("the file");
        assert_eq!(written[version_at], VERSION_WITHOUT_TIMING);
        store
            .keep_timing("A1", ptp_entry("366.0"))
            .expect("written");
        store
            .keep_timing("B2", ptp_entry("377.1"))
            .expect("written");
        let written = fs::read(&path).expect("the file");
        assert_eq!(written[version_at], VERSION);

        let (mut again, loaded) = IdentityStore::open(path.clone());
        assert_eq!(loaded, Loaded::Receivers(1));
        assert_eq!(again.get("A1"), Some(&credentials(7)));
        assert_eq!(again.timing("A1"), Some(&ptp_entry("366.0")));
        assert_eq!(again.timing("B2"), Some(&ptp_entry("377.1")));
        assert_eq!(again.get("B2"), None);

        again.remove("A1").expect("written");
        again.forget_timing("B2").expect("written");
        let (last, _) = IdentityStore::open(path.clone());
        assert_eq!(last.timing("A1"), None);
        assert_eq!(last.timing("B2"), None);
        let written = fs::read(&path).expect("the file");
        assert_eq!(written[version_at], VERSION_WITHOUT_TIMING);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_timing_record_is_checked_like_a_credential_record() {
        let mut identity = Airplay2Identity::new([1, 2, 3, 4, 5, 6]);
        identity.insert("A1", credentials(3));
        identity.keep_timing("A1", ptp_entry("366.0"));
        let bytes = identity.to_bytes().expect("bytes");
        assert!(Airplay2Identity::from_bytes(&bytes).is_ok());
        for cut in [MAGIC.len() + 8, bytes.len() - 1] {
            assert_eq!(
                Airplay2Identity::from_bytes(&bytes[..cut]).err(),
                Some(Unreadable::Format),
                "{cut}"
            );
        }
        // A count naming more credential records than the bytes hold.
        let mut miscounted = bytes.to_vec();
        miscounted[MAGIC.len() + 7] = 2;
        assert_eq!(
            Airplay2Identity::from_bytes(&miscounted).err(),
            Some(Unreadable::Format)
        );
    }

    #[test]
    fn a_foreign_file_is_replaced_and_a_newer_one_left_alone() {
        let path = scratch("foreign.dat");
        fs::write(&path, b"not an identity").expect("written");
        let (store, loaded) = IdentityStore::open(path.clone());
        assert!(matches!(loaded, Loaded::Ignored(_)));
        let (again, _) = IdentityStore::open(path.clone());
        assert_eq!(again.device_id(), store.device_id());

        let mut newer = MAGIC.to_vec();
        newer.push(VERSION + 1);
        fs::write(&path, &newer).expect("written");
        let (mut store, loaded) = IdentityStore::open(path.clone());
        assert_eq!(loaded, Loaded::Newer(VERSION + 1));
        assert!(store.insert("A1", credentials(1)).is_err());
        assert_eq!(fs::read(&path).expect("read"), newer);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_cant_be_read_is_never_written_over() {
        // A folder in the file's place reads as an error other than NotFound,
        // as a permission or disk error would.
        let path = scratch("unreadable.dat");
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("a folder");
        let (mut store, loaded) = IdentityStore::open(path.clone());
        assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
        assert!(!store.writable);
        assert!(store.insert("A1", credentials(2)).is_err());
        assert_eq!(store.get("A1"), Some(&credentials(2)), "kept for this run");
        assert!(path.is_dir());
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn the_byte_form_is_checked_field_by_field() {
        let mut identity = Airplay2Identity::new([1, 2, 3, 4, 5, 6]);
        identity.insert("A1", credentials(3));
        identity.insert("B2", credentials(4));
        let bytes = identity.to_bytes().expect("bytes");
        let back = Airplay2Identity::from_bytes(&bytes).expect("parsed");
        assert_eq!(back.device_id(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(back.get("B2"), Some(&credentials(4)));
        for cut in [MAGIC.len(), MAGIC.len() + 3, bytes.len() - 1] {
            assert_eq!(
                Airplay2Identity::from_bytes(&bytes[..cut]).err(),
                Some(Unreadable::Format),
                "{cut}"
            );
        }
        assert!(Airplay2Identity::from_bytes(&bytes[..MAGIC.len() + 7]).is_ok());
    }

    // covers: S224
    #[cfg(unix)]
    #[test]
    fn a_file_others_may_write_is_made_owner_only_and_still_used() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("open-to-others.dat");
        let (store, _) = IdentityStore::open(path.clone());
        let device_id = store.device_id();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("mode set");
        let (again, loaded) = IdentityStore::open(path.clone());
        assert_eq!(loaded, Loaded::Receivers(0));
        assert_eq!(again.access(), Access::Tightened);
        assert_eq!(again.device_id(), device_id);
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_file(&path);
    }

    // covers: S224
    #[cfg(unix)]
    #[test]
    fn a_file_that_isnt_read_is_reported_unread_and_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let target = scratch("link-target.dat");
        let _ = IdentityStore::open(target.clone());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("mode set");
        let path = scratch("link.dat");
        std::os::unix::fs::symlink(&target, &path).expect("a symbolic link");
        let (store, loaded) = IdentityStore::open(path.clone());
        assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
        assert_eq!(store.access(), Access::Unread);
        let mode = fs::metadata(&target)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644, "the link's target is left alone");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&target);
    }
}
