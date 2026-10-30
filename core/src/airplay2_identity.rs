//! The AirPlay 2 identity a host keeps across launches: the random device
//! ID of the session SETUP (P5, P43) and the HomeKit credentials of each
//! receiver paired on the AirPlay 2 path (P152), by receiver ID, so a
//! relaunch verifies instead of pairing again.
//!
//! [`Airplay2Identity`] is the contents and their byte form, which the
//! Windows app seals with DPAPI next to its pairings file. [`IdentityStore`]
//! keeps them for the macOS app in a file of their own, next to the pairings
//! file and handled the same way ([`crate::airplay_pairings`]): mode 0600,
//! written through a temporary file renamed over the old one, a file that
//! can't be read or parsed ignored as a whole, a file that can't be parsed
//! copied aside first as `<stem>.ignored-<seconds since 1970>.<extension>`
//! (one that can't be copied is left as it is) and replaced, a file of a
//! newer format version left as it is.
//!
//! The byte form holds a magic line, a format version byte, the 6-byte
//! device ID, then per receiver its ID and the credential blob the sender
//! defines ([`HomeKitCredentials::to_bytes`]), each with a 16-bit
//! little-endian length. The plaintext lives only in buffers wiped when
//! dropped, and neither `Debug` nor [`Loaded`] shows a secret.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::sender::random_airplay2_device_id;
use zeroize::Zeroizing;

use crate::airplay_pairings::{
    Access, Contents, Loaded, framed_id, holds, keep_aside, prune_copies, read_private, take_field,
    write_atomically,
};

/// The byte form's first bytes.
pub const MAGIC: &[u8] = b"AudioSync AirPlay 2 identity\n";

/// The format version this build writes and reads.
pub const VERSION: u8 = 1;

/// Why bytes aren't an identity this build can use. Never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreadable {
    /// A newer AudioSync wrote them, in this format version.
    Newer(u8),
    /// Not in the expected format.
    Format,
}

/// The device ID and the HomeKit credentials by receiver ID.
#[derive(Clone)]
pub struct Airplay2Identity {
    device_id: [u8; 6],
    homekit: BTreeMap<String, HomeKitCredentials>,
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

    /// Forgets the receiver's credentials; false when it held none.
    pub fn remove(&mut self, id: &str) -> bool {
        self.homekit.remove(id).is_some()
    }

    /// The byte form. It holds the credentials' seeds, so a host keeps it
    /// where secrets go.
    pub fn to_bytes(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "a receiver ID is too long");
        let blobs: Vec<(&String, Zeroizing<Vec<u8>>)> = self
            .homekit
            .iter()
            .map(|(id, credentials)| (id, credentials.to_bytes()))
            .collect();
        // Allocated once at its full size, so growing never leaves a copy of
        // a secret behind in freed memory.
        let size = MAGIC.len()
            + 1
            + self.device_id.len()
            + blobs
                .iter()
                .map(|(id, blob)| 4 + id.len() + blob.len())
                .sum::<usize>();
        let mut bytes = Zeroizing::new(Vec::with_capacity(size));
        bytes.extend_from_slice(MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(&self.device_id);
        for (id, blob) in &blobs {
            let id_len = u16::try_from(id.len()).map_err(|_| too_long())?;
            let blob_len = u16::try_from(blob.len()).map_err(|_| too_long())?;
            bytes.extend_from_slice(&id_len.to_le_bytes());
            bytes.extend_from_slice(id.as_bytes());
            bytes.extend_from_slice(&blob_len.to_le_bytes());
            bytes.extend_from_slice(blob);
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
        if *version != VERSION {
            return Err(Unreadable::Format);
        }
        let (device_id, mut rest) = rest.split_first_chunk::<6>().ok_or(Unreadable::Format)?;
        let mut identity = Airplay2Identity::new(*device_id);
        while !rest.is_empty() {
            let id = take_field(&mut rest).and_then(|id| std::str::from_utf8(id).ok());
            let credentials =
                take_field(&mut rest).and_then(|blob| HomeKitCredentials::from_bytes(blob).ok());
            let (Some(id), Some(credentials)) = (id, credentials) else {
                return Err(Unreadable::Format);
            };
            if id.is_empty()
                || identity
                    .homekit
                    .insert(id.to_owned(), credentials)
                    .is_some()
            {
                return Err(Unreadable::Format);
            }
        }
        Ok(identity)
    }
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
                        let kept = keep_aside(&path, &bytes).is_ok();
                        let reason = "the file isn't in the expected format".to_owned();
                        (identity, random && kept, Loaded::Ignored(reason))
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

    /// Forgets the receiver's credentials, writes the file and deletes the
    /// copies kept aside that hold its ID; when the file can't be written
    /// they are forgotten until the store is dropped all the same.
    pub fn remove(&mut self, id: &str) -> io::Result<()> {
        let saved = if self.identity.remove(id) {
            self.save()
        } else {
            Ok(())
        };
        let pruned = self
            .path
            .as_deref()
            .map_or(Ok(()), |path| prune_copies(path, id));
        saved.and(pruned)
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

/// Drops the receiver's credentials from the identity file at `path`
/// without opening it as a store, for Forget Pairing while the AirPlay 2
/// switch is off, and deletes the copies kept aside that hold its ID. Only
/// a file this build reads is written: a missing one isn't created, and one
/// that can't be read or parsed, or that another account owns, is left as
/// it is. Returns whether the file held the credentials, and how the write
/// went; a newer AudioSync's file that holds the ID counts as held and not
/// written.
pub fn forget_in_file(path: &Path, id: &str) -> (bool, io::Result<()>) {
    let pruned = prune_copies(path, id);
    let Ok(Contents::Read(bytes, _)) = read_private(path) else {
        return (false, pruned);
    };
    match Airplay2Identity::from_bytes(&bytes) {
        Ok(mut identity) => {
            if !identity.remove(id) {
                return (false, pruned);
            }
            let written = identity
                .to_bytes()
                .and_then(|plain| write_atomically(path, &plain));
            (true, written.and(pruned))
        }
        Err(Unreadable::Newer(_)) if framed_id(id).is_some_and(|framed| holds(&bytes, &framed)) => {
            let left = io::Error::other("the AirPlay 2 identity file is left as it is");
            (true, Err(left))
        }
        Err(_) => (false, pruned),
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

    // covers: S224
    #[test]
    fn a_file_with_a_bad_entry_is_kept_aside_before_it_is_replaced() {
        let path = scratch("kept-aside.dat");
        let folder = path.parent().expect("a folder").to_owned();
        let copies = || -> Vec<PathBuf> {
            fs::read_dir(&folder)
                .expect("the folder")
                .flatten()
                .map(|entry| entry.path())
                .filter(|copy| {
                    let name = copy.file_name().unwrap_or_default().to_string_lossy();
                    name.starts_with("kept-aside.ignored-")
                })
                .collect()
        };
        for copy in copies() {
            fs::remove_file(copy).expect("an old copy is removed");
        }
        let (mut store, _) = IdentityStore::open(path.clone());
        store.insert("A1", credentials(5)).expect("written");
        let mut bytes = fs::read(&path).expect("read");
        // The credential blob's own version byte, after the device ID and
        // the receiver ID's field.
        bytes[MAGIC.len() + 1 + 6 + 2 + 2 + 2] = 0xEE;
        fs::write(&path, &bytes).expect("written");
        let (_, loaded) = IdentityStore::open(path.clone());
        assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
        let kept: Vec<_> = copies()
            .into_iter()
            .map(|copy| fs::read(copy).expect("read"))
            .collect();
        assert_eq!(kept, [bytes], "the original bytes survive in a copy");
        for copy in copies() {
            let _ = fs::remove_file(copy);
        }
        let _ = fs::remove_file(&path);
    }

    fn copies_of(path: &Path) -> Vec<PathBuf> {
        let stem = path.file_stem().expect("a name").to_string_lossy();
        let prefix = format!("{stem}.ignored-");
        fs::read_dir(path.parent().expect("a folder"))
            .expect("the folder")
            .flatten()
            .map(|entry| entry.path())
            .filter(|copy| {
                let name = copy.file_name().unwrap_or_default().to_string_lossy();
                name.starts_with(&prefix)
            })
            .collect()
    }

    // covers: S224
    #[test]
    fn forget_in_a_closed_file_leaves_a_file_it_cant_use_as_it_is() {
        let path = scratch("closed-unusable.dat");
        let (mut store, _) = IdentityStore::open(path.clone());
        store.insert("A1", credentials(2)).expect("written");
        drop(store);
        let mut broken = fs::read(&path).expect("read");
        broken[MAGIC.len() + 1 + 6 + 2 + 2 + 2] = 0xEE;
        let mut newer = fs::read(&path).expect("read");
        newer[MAGIC.len()] = VERSION + 1;
        for (bytes, held) in [
            (b"not an identity A1".to_vec(), false),
            (broken, false),
            (newer.clone(), true),
        ] {
            fs::write(&path, &bytes).expect("written");
            let (found, written) = forget_in_file(&path, "A1");
            assert_eq!(found, held);
            assert_eq!(
                written.is_err(),
                held,
                "a newer file holding it isn't written"
            );
            assert_eq!(fs::read(&path).expect("read"), bytes, "left as it is");
            assert!(copies_of(&path).is_empty(), "nothing kept aside either");
        }
        fs::write(&path, &newer).expect("written");
        let (found, written) = forget_in_file(&path, "B2");
        assert!(!found && written.is_ok(), "a newer file without it");
        let _ = fs::remove_file(&path);
    }

    // covers: S224
    #[test]
    fn forget_deletes_the_identity_copies_kept_aside_that_hold_the_receiver() {
        let path = scratch("closed-copies.dat");
        for copy in copies_of(&path) {
            fs::remove_file(copy).expect("an old copy is removed");
        }
        let (mut store, _) = IdentityStore::open(path.clone());
        store.insert("A1", credentials(3)).expect("written");
        store.insert("B2", credentials(4)).expect("written");
        keep_aside(&path, &fs::read(&path).expect("read")).expect("kept");
        store.remove("A1").expect("written");
        let without = fs::read(&path).expect("read");
        keep_aside(&path, &without).expect("kept");
        store.insert("A1", credentials(3)).expect("written");
        drop(store);

        let (found, written) = forget_in_file(&path, "A1");
        assert!(found && written.is_ok());
        let kept: Vec<_> = copies_of(&path)
            .into_iter()
            .map(|copy| fs::read(copy).expect("read"))
            .collect();
        assert_eq!(
            kept,
            std::slice::from_ref(&without),
            "only the copy without it stays"
        );

        keep_aside(&path, &fs::read(&path).expect("read")).expect("kept");
        let mut held = without.clone();
        held.extend_from_slice(b"\x02\x00A1");
        keep_aside(&path, &held).expect("kept");
        let (mut store, _) = IdentityStore::open(path.clone());
        store
            .remove("A1")
            .expect("the store didn't hold it, a copy did");
        assert_eq!(copies_of(&path).len(), 2, "copies without it stay");
        for copy in copies_of(&path) {
            let _ = fs::remove_file(copy);
        }
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
