//! The Mac's store of AirPlay pairing credentials (`airplay_pairings`):
//! a round trip through the file, a bad entry skipped alone, a corrupt or
//! newer file ignored, a file with something unusable kept aside, the
//! file for its owner only, and no secret in anything printable.

use std::fs;
use std::path::{Path, PathBuf};

use audiosync_airplay::pairing::{Credentials, SECRET_BYTES};
use audiosync_core::airplay_pairings::{Loaded, MAGIC, PairingStore, VERSION};

/// Credentials with a recognizable client ID and secret.
fn credentials(client: &str, fill: u8) -> Credentials {
    Credentials::new(client, [fill; SECRET_BYTES]).expect("valid credentials")
}

/// A fresh folder for one test, in the target folder's scratch space.
fn folder(name: &str) -> PathBuf {
    let folder = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("airplay-pairings")
        .join(name);
    let _ = fs::remove_dir_all(&folder);
    folder
}

#[test]
fn credentials_survive_a_restart() {
    let path = folder("restart")
        .join("AudioSync")
        .join("airplay-pairings.dat");
    let (mut store, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::NoFile);
    assert!(store.is_empty());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("written");
    store
        .insert("raop:Den TV", credentials("FEDCBA9876543210", 9))
        .expect("written");
    assert!(path.exists(), "the folder is created and the file written");
    assert!(
        !path.with_extension("dat.tmp").exists(),
        "no temporary file stays behind"
    );

    let (again, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::Receivers(2));
    assert_eq!(again.len(), 2);
    assert_eq!(
        again.get("02005E005301"),
        Some(&credentials("0123456789ABCDEF", 7))
    );
    assert_eq!(
        again.get("raop:Den TV"),
        Some(&credentials("FEDCBA9876543210", 9))
    );
    assert!(!again.contains("02005E005399"));

    let bytes = fs::read(&path).expect("the file");
    assert!(bytes.starts_with(MAGIC) && bytes[MAGIC.len()] == VERSION);
}

#[test]
fn forgetting_the_last_pairing_removes_the_file() {
    let path = folder("forget").join("airplay-pairings.dat");
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("written");
    assert!(!store.remove("02005E005399").expect("nothing to write"));
    assert!(path.exists());
    assert!(store.remove("02005E005301").expect("written"));
    assert!(!path.exists(), "nothing paired, no file");
    let (again, loaded) = PairingStore::open(path);
    assert_eq!(loaded, Loaded::NoFile);
    assert!(again.is_empty());
}

#[test]
fn a_corrupt_file_is_ignored_and_replaced_by_the_next_pairing() {
    let base = folder("corrupt");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    let mut good = MAGIC.to_vec();
    good.push(VERSION);
    let cases: Vec<Vec<u8>> = vec![
        b"not a pairings file".to_vec(),
        MAGIC.to_vec(),
        [good.clone(), vec![5, 0, b'a']].concat(),
        [MAGIC.to_vec(), vec![0]].concat(),
    ];
    for bytes in cases {
        fs::write(&path, &bytes).expect("written");
        let (store, loaded) = PairingStore::open(path.clone());
        assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
        assert!(store.is_empty());
    }
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("the corrupt file is replaced");
    assert_eq!(PairingStore::open(path).1, Loaded::Receivers(1));
    let kept = kept_aside(&base);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(
        fs::read(&kept[0]).expect("the copy"),
        [MAGIC.to_vec(), vec![0]].concat(),
        "the corrupt file is kept aside before it is replaced"
    );
}

/// One entry as the file frames it: the ID, then the credential blob, each
/// after its 16-bit little-endian length.
fn entry(id: &[u8], blob: &[u8]) -> Vec<u8> {
    let length = |field: &[u8]| u16::try_from(field.len()).expect("short").to_le_bytes();
    [&length(id)[..], id, &length(blob)[..], blob].concat()
}

/// The copies kept aside next to `airplay-pairings.dat` in `folder`.
fn kept_aside(folder: &Path) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = fs::read_dir(folder)
        .expect("the folder")
        .map(|found| found.expect("an entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("airplay-pairings.ignored-"))
        })
        .collect();
    kept.sort();
    kept
}

#[test]
fn a_bad_entry_loses_only_itself_and_the_file_is_kept_aside_before_the_next_write() {
    let base = folder("bad-entry");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    let first = credentials("0123456789ABCDEF", 7);
    let second = credentials("FEDCBA9876543210", 9);
    let bytes = [
        MAGIC.to_vec(),
        vec![VERSION],
        entry(b"02005E005301", &first.to_bytes()),
        entry(b"02005E005302", &[1, 2, 3]),
        entry(b"", &second.to_bytes()),
        entry(b"02005E005301", &second.to_bytes()),
        entry(&[0xff, 0xfe], &second.to_bytes()),
        entry(b"raop:Den TV", &second.to_bytes()),
    ]
    .concat();
    fs::write(&path, &bytes).expect("written");

    let (mut store, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::Receivers(2));
    assert_eq!(
        store.get("02005E005301"),
        Some(&first),
        "a repeated ID keeps the first"
    );
    assert_eq!(store.get("raop:Den TV"), Some(&second));
    assert!(
        kept_aside(&base).is_empty(),
        "nothing is copied before a write"
    );
    fs::write(&path, b"changed after the read").expect("written");

    store
        .insert("02005E005303", credentials("00112233445566FF", 3))
        .expect("written");
    store
        .insert("02005E005304", credentials("00112233445566EE", 4))
        .expect("written");
    let kept = kept_aside(&base);
    assert_eq!(kept.len(), 1, "kept aside once: {kept:?}");
    assert_eq!(
        fs::read(&kept[0]).expect("the copy"),
        bytes,
        "the copy holds what was read, not what the file holds at the write"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&kept[0])
            .expect("the copy")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the copy is its owner's only");
    }
    assert_eq!(PairingStore::open(path.clone()).1, Loaded::Receivers(4));

    let unrelated = base.join("airplay-pairings.ignored-1.dat");
    fs::write(&unrelated, b"another receiver's copy").expect("written");
    assert!(store.remove("02005E005303").expect("written"));
    assert_eq!(kept_aside(&base).len(), 2, "no copy holds that ID");
    assert!(store.remove("02005E005301").expect("written"));
    assert_eq!(
        kept_aside(&base),
        vec![unrelated],
        "forgetting a receiver deletes the copies that hold its ID, and only those"
    );
}

#[test]
fn forgetting_a_receiver_only_a_copy_holds_still_deletes_that_copy() {
    let base = folder("forget-copy-only");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    let good = credentials("0123456789ABCDEF", 7);
    let secret = credentials("FEDCBA9876543210", 9).to_bytes();
    let skipped = [
        MAGIC.to_vec(),
        vec![VERSION],
        entry(b"02005E005301", &good.to_bytes()),
        entry(b"02005E005302", &secret[..secret.len() - 1]),
    ]
    .concat();
    let broken = [
        MAGIC.to_vec(),
        vec![VERSION],
        entry(b"02005E005303", &secret),
        vec![9, 0, b'x'],
    ]
    .concat();
    for (bytes, forgotten) in [(skipped, "02005E005302"), (broken, "02005E005303")] {
        for copy in kept_aside(&base) {
            fs::remove_file(copy).expect("removed");
        }
        fs::write(&path, &bytes).expect("written");
        let (mut store, _) = PairingStore::open(path.clone());
        assert!(!store.contains(forgotten));
        store
            .insert("02005E005304", credentials("00112233445566FF", 3))
            .expect("written");
        assert_eq!(kept_aside(&base).len(), 1, "the file is kept aside");
        assert!(
            !store.remove(forgotten).expect("the copy is deleted"),
            "it wasn't paired"
        );
        assert!(
            kept_aside(&base).is_empty(),
            "a copy holding the forgotten ID is deleted even when only the copy held it"
        );
        assert!(store.contains("02005E005304"));
    }
}

#[test]
fn forgetting_a_skipped_receiver_before_any_write_leaves_no_copy_of_it() {
    let base = folder("forget-before-write");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    let secret = credentials("FEDCBA9876543210", 9).to_bytes();
    fs::write(
        &path,
        [
            MAGIC.to_vec(),
            vec![VERSION],
            entry(
                b"02005E005301",
                &credentials("0123456789ABCDEF", 7).to_bytes(),
            ),
            entry(b"02005E005302", &secret[..secret.len() - 1]),
        ]
        .concat(),
    )
    .expect("written");
    let framed = entry(b"02005E005302", b"");
    let framed = &framed[..framed.len() - 2];
    let holds_it = |file: &Path| {
        fs::read(file)
            .expect("the file")
            .windows(framed.len())
            .any(|window| window == framed)
    };

    let (mut store, _) = PairingStore::open(path.clone());
    assert!(
        !store.remove("02005E005302").expect("written"),
        "it wasn't paired"
    );
    store
        .insert("02005E005303", credentials("00112233445566FF", 3))
        .expect("written");
    assert!(!holds_it(&path), "the file no longer holds it");
    for copy in kept_aside(&base) {
        assert!(!holds_it(&copy), "no copy holds the forgotten ID: {copy:?}");
    }
    assert!(store.contains("02005E005301") && store.contains("02005E005303"));
}

#[test]
fn a_newer_file_is_ignored_and_left_as_it_is() {
    let base = folder("newer");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    let newer = [MAGIC, &[VERSION + 1], b"whatever comes next"].concat();
    fs::write(&path, &newer).expect("written");
    let (mut store, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::Newer(VERSION + 1));
    assert!(store.is_empty());
    assert!(
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 7))
            .is_err(),
        "the newer file isn't overwritten"
    );
    assert!(store.contains("02005E005301"), "kept for this run");
    assert_eq!(fs::read(&path).expect("still there"), newer);
}

#[test]
fn a_leftover_temporary_file_does_not_block_a_write() {
    let base = folder("leftover");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    fs::write(base.join("airplay-pairings.dat.tmp"), b"from a crash").expect("written");
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("written");
    assert!(!base.join("airplay-pairings.dat.tmp").exists());
    assert_eq!(PairingStore::open(path).1, Loaded::Receivers(1));
}

#[cfg(unix)]
#[test]
fn the_file_is_for_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let path = folder("mode")
        .join("AudioSync")
        .join("airplay-pairings.dat");
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("written");
    let mode = fs::metadata(&path).expect("the file").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let folder_mode = fs::metadata(path.parent().expect("its folder"))
        .expect("the folder")
        .permissions()
        .mode();
    assert_eq!(folder_mode & 0o077, 0, "the new folder is its owner's only");
}

#[test]
fn nothing_printable_holds_a_secret() {
    let path = folder("debug").join("airplay-pairings.dat");
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 0xAB))
        .expect("written");
    let shown = format!("{store:?} {}", PairingStore::open(path).1);
    assert!(shown.contains("02005E005301"));
    assert!(!shown.contains("0123456789ABCDEF") && !shown.to_lowercase().contains("ab, ab"));
}

#[test]
fn an_in_memory_store_writes_nothing() {
    let mut store = PairingStore::in_memory();
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("nothing to write");
    assert!(store.contains("02005E005301"));
    assert!(store.remove("02005E005301").expect("nothing to write"));
}

// covers: S224
#[cfg(unix)]
#[test]
fn a_file_others_may_read_is_made_owner_only_and_still_used() {
    use audiosync_core::airplay_pairings::Access;
    use std::os::unix::fs::PermissionsExt;
    let path = folder("open-to-others").join("airplay-pairings.dat");
    let (mut store, _) = PairingStore::open(path.clone());
    assert_eq!(store.access(), Access::Private);
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 3))
        .expect("written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode set");

    let (store, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::Receivers(1));
    assert_eq!(store.access(), Access::Tightened);
    let mode = fs::metadata(&path).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let (store, loaded) = PairingStore::open(path);
    assert_eq!(loaded, Loaded::Receivers(1));
    assert_eq!(
        store.access(),
        Access::Private,
        "an owner-only file is left as it is"
    );
}

// covers: S224
#[cfg(unix)]
#[test]
fn an_ignored_file_others_may_read_still_reports_it_was_tightened() {
    use audiosync_core::airplay_pairings::Access;
    use std::os::unix::fs::PermissionsExt;
    let folder = folder("ignored-open-to-others");
    fs::create_dir_all(&folder).expect("folder made");
    let mut newer = MAGIC.to_vec();
    newer.push(VERSION + 1);
    for (name, bytes) in [
        ("corrupt.dat", b"not a pairings file".to_vec()),
        ("newer.dat", newer),
    ] {
        let path = folder.join(name);
        fs::write(&path, bytes).expect("written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode set");

        let (store, loaded) = PairingStore::open(path.clone());
        assert!(
            matches!(loaded, Loaded::Ignored(_) | Loaded::Newer(_)),
            "{name}: {loaded:?}"
        );
        assert_eq!(store.access(), Access::Tightened, "{name}");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}

// covers: S224
#[cfg(unix)]
#[test]
fn a_symbolic_link_in_the_files_place_is_not_followed() {
    use std::os::unix::fs::PermissionsExt;
    let folder = folder("symlink");
    let target = folder.join("elsewhere.dat");
    let (mut store, _) = PairingStore::open(target.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 5))
        .expect("written");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("mode set");
    let path = folder.join("airplay-pairings.dat");
    std::os::unix::fs::symlink(&target, &path).expect("a symbolic link");

    let (store, loaded) = PairingStore::open(path.clone());
    assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
    assert!(store.is_empty());
    assert_eq!(
        store.access(),
        audiosync_core::airplay_pairings::Access::Unread
    );
    let mode = fs::metadata(&target)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644, "the link's target is left alone");
}

// covers: S224
#[cfg(unix)]
#[test]
fn a_file_another_account_owns_is_reported_unread() {
    use audiosync_core::airplay_pairings::Access;
    use std::os::unix::fs::MetadataExt;
    let mine = folder("foreign").join("mine.dat");
    fs::create_dir_all(mine.parent().expect("a folder")).expect("folder made");
    fs::write(&mine, b"x").expect("written");
    let me = fs::metadata(&mine).expect("metadata").uid();
    // A system file root owns; running as root there is no other account.
    let foreign = PathBuf::from("/etc/hosts");
    let Ok(metadata) = fs::metadata(&foreign) else {
        return;
    };
    if metadata.uid() == me {
        return;
    }
    let (store, loaded) = PairingStore::open(foreign);
    assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
    assert!(store.is_empty());
    assert_eq!(store.access(), Access::Unread);
}
