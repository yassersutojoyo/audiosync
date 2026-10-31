//! The AirPlay 2 timing entry of each receiver (P217), by receiver ID, in a
//! file of its own next to the AirPlay 2 identity. It stays out of the
//! identity file so that file keeps the format an older AudioSync reads: one
//! it can't read costs it every AirPlay 2 pairing.
//!
//! The entries are a cache that holds no secret. A missing, unreadable or
//! foreign file means no entries, and each receiver's next session asks for
//! NTP first, as P157 does without an entry; it never keeps a pairing from
//! working. An entry that doesn't read costs only that receiver's entry, and
//! an ID read twice keeps its first entry.
//!
//! The byte form holds a magic line, a format version byte, then per
//! receiver its ID and its entry ([`TimingMemory::to_bytes`]), each with a
//! 16-bit little-endian length. Both hosts write it through
//! [`write_atomically`]: owner-only on the Mac, unsealed with the folder's
//! permissions on Windows.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use audiosync_airplay::sender::TimingMemory;

use crate::airplay_pairings::{Contents, read_private, take_field, write_atomically};

/// The file's name, next to the AirPlay 2 identity file.
pub const FILE_NAME: &str = "airplay2-timing.dat";

/// The byte form's first bytes.
pub const MAGIC: &[u8] = b"AudioSync AirPlay 2 timing\n";

/// The format version this build writes and reads.
pub const VERSION: u8 = 1;

/// The timing entries by receiver ID, and the file they live in.
#[derive(Debug)]
pub struct TimingStore {
    /// The file, or `None` for a store that lives in memory only.
    path: Option<PathBuf>,
    /// False when the file belongs to a newer AudioSync or couldn't be read
    /// now: it is left for that AudioSync, or for the next launch.
    writable: bool,
    entries: BTreeMap<String, TimingMemory>,
}

impl Default for TimingStore {
    fn default() -> TimingStore {
        TimingStore::in_memory()
    }
}

impl TimingStore {
    /// A store with no file, which forgets everything when dropped.
    pub fn in_memory() -> TimingStore {
        TimingStore {
            path: None,
            writable: true,
            entries: BTreeMap::new(),
        }
    }

    /// The entries kept in the file at `path`; none when it is missing or
    /// can't be used. Nothing is written until an entry changes.
    pub fn open(path: PathBuf) -> TimingStore {
        let (entries, writable) = match read_private(&path) {
            Ok(Contents::Read(bytes, _)) => match decode(&bytes) {
                Decoded::Entries(entries) => (entries, true),
                Decoded::Newer => (BTreeMap::new(), false),
                Decoded::Format => (BTreeMap::new(), true),
            },
            Ok(Contents::Missing | Contents::Foreign(_)) => (BTreeMap::new(), true),
            Ok(Contents::Failed(..)) | Err(_) => (BTreeMap::new(), false),
        };
        TimingStore {
            path: Some(path),
            writable,
            entries,
        }
    }

    pub fn get(&self, id: &str) -> Option<&TimingMemory> {
        self.entries.get(id)
    }

    /// Keeps the receiver's entry and writes the file when it changed; when
    /// it can't be written the entry lasts until the store is dropped.
    pub fn keep(&mut self, id: &str, memory: TimingMemory) -> io::Result<()> {
        if self.entries.get(id) == Some(&memory) {
            return Ok(());
        }
        self.entries.insert(id.to_owned(), memory);
        self.save()
    }

    /// Drops the receiver's entry and writes the file when it held one.
    pub fn forget(&mut self, id: &str) -> io::Result<()> {
        if self.entries.remove(id).is_none() {
            return Ok(());
        }
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.writable {
            return Err(io::Error::other(
                "the AirPlay 2 timing file is left as it is",
            ));
        }
        write_atomically(path, &encode(&self.entries)?)
    }
}

enum Decoded {
    Entries(BTreeMap<String, TimingMemory>),
    Newer,
    Format,
}

fn encode(entries: &BTreeMap<String, TimingMemory>) -> io::Result<Vec<u8>> {
    let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "a receiver ID is too long");
    let mut bytes = MAGIC.to_vec();
    bytes.push(VERSION);
    for (id, memory) in entries {
        let entry = memory.to_bytes();
        let id_len = u16::try_from(id.len()).map_err(|_| too_long())?;
        let entry_len = u16::try_from(entry.len()).map_err(|_| too_long())?;
        bytes.extend_from_slice(&id_len.to_le_bytes());
        bytes.extend_from_slice(id.as_bytes());
        bytes.extend_from_slice(&entry_len.to_le_bytes());
        bytes.extend_from_slice(&entry);
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Decoded {
    let Some((version, mut rest)) = bytes.strip_prefix(MAGIC).and_then(<[u8]>::split_first) else {
        return Decoded::Format;
    };
    if *version > VERSION {
        return Decoded::Newer;
    }
    if *version != VERSION {
        return Decoded::Format;
    }
    let mut entries = BTreeMap::new();
    // A field cut short ends what can be read; one that doesn't read costs
    // only its own entry.
    while let (Some(id), Some(memory)) = (take_field(&mut rest), take_field(&mut rest)) {
        let id = std::str::from_utf8(id).ok().filter(|id| !id.is_empty());
        let (Some(id), Some(memory)) = (id, TimingMemory::from_bytes(memory)) else {
            continue;
        };
        entries.entry(id.to_owned()).or_insert(memory);
    }
    Decoded::Entries(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiosync_airplay::ap2::Timing;
    use std::fs;

    fn entry(timing: Timing, srcvers: &str) -> TimingMemory {
        TimingMemory {
            timing,
            features: 0x1_0240_0000_0000,
            srcvers: Some(srcvers.to_owned()),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let folder = crate::test_scratch().join("airplay2-timing-store");
        fs::create_dir_all(&folder).expect("a scratch folder");
        let path = folder.join(name);
        let _ = fs::remove_file(&path);
        path
    }

    // covers: P217
    #[test]
    fn entries_survive_a_relaunch_and_a_forgotten_one_stays_gone() {
        let path = scratch("relaunch.dat");
        let mut store = TimingStore::open(path.clone());
        assert!(!path.exists(), "nothing is written before an entry");
        store
            .keep("A1", entry(Timing::Ptp, "366.0"))
            .expect("written");
        store
            .keep("B2", entry(Timing::Ntp, "377.1"))
            .expect("written");
        let again = TimingStore::open(path.clone());
        assert_eq!(again.get("A1"), Some(&entry(Timing::Ptp, "366.0")));
        assert_eq!(again.get("B2"), Some(&entry(Timing::Ntp, "377.1")));

        let mut again = again;
        again.forget("A1").expect("written");
        let last = TimingStore::open(path.clone());
        assert_eq!(last.get("A1"), None);
        assert_eq!(last.get("B2"), Some(&entry(Timing::Ntp, "377.1")));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_file_it_cant_use_means_no_entries() {
        let path = scratch("broken.dat");
        fs::write(&path, b"not a timing file").expect("written");
        let mut store = TimingStore::open(path.clone());
        assert_eq!(store.get("A1"), None);
        store
            .keep("A1", entry(Timing::Ptp, "366.0"))
            .expect("replaced");
        assert!(TimingStore::open(path.clone()).get("A1").is_some());

        let mut cut = fs::read(&path).expect("the file");
        cut.pop();
        fs::write(&path, &cut).expect("written");
        assert_eq!(TimingStore::open(path.clone()).get("A1"), None);

        let mut newer = MAGIC.to_vec();
        newer.push(VERSION + 1);
        fs::write(&path, &newer).expect("written");
        let mut store = TimingStore::open(path.clone());
        assert_eq!(store.get("A1"), None);
        assert!(store.keep("A1", entry(Timing::Ptp, "366.0")).is_err());
        assert_eq!(store.get("A1"), Some(&entry(Timing::Ptp, "366.0")));
        assert_eq!(fs::read(&path).expect("the file"), newer, "left as it is");
        let _ = fs::remove_file(&path);
    }

    fn push_entry(bytes: &mut Vec<u8>, id: &[u8], entry: &[u8]) {
        for field in [id, entry] {
            let len = u16::try_from(field.len()).expect("a short field");
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(field);
        }
    }

    #[test]
    fn a_bad_or_repeated_entry_costs_only_itself() {
        let path = scratch("bad-entry.dat");
        let mut bytes = MAGIC.to_vec();
        bytes.push(VERSION);
        push_entry(&mut bytes, b"A1", &entry(Timing::Ptp, "366.0").to_bytes());
        push_entry(&mut bytes, b"B2", b"not an entry");
        push_entry(&mut bytes, b"", &entry(Timing::Ptp, "366.0").to_bytes());
        push_entry(
            &mut bytes,
            &[0xff, 0xfe],
            &entry(Timing::Ptp, "366.0").to_bytes(),
        );
        push_entry(&mut bytes, b"A1", &entry(Timing::Ntp, "1.0").to_bytes());
        push_entry(&mut bytes, b"C3", &entry(Timing::Ntp, "377.1").to_bytes());
        fs::write(&path, &bytes).expect("written");
        let store = TimingStore::open(path.clone());
        assert_eq!(store.get("A1"), Some(&entry(Timing::Ptp, "366.0")));
        assert_eq!(store.get("B2"), None);
        assert_eq!(store.get("C3"), Some(&entry(Timing::Ntp, "377.1")));

        bytes.extend_from_slice(&[9, 0, b'D']);
        fs::write(&path, &bytes).expect("written");
        let store = TimingStore::open(path.clone());
        assert_eq!(
            store.get("C3"),
            Some(&entry(Timing::Ntp, "377.1")),
            "a cut tail costs only the tail"
        );
        let _ = fs::remove_file(&path);
    }
}
