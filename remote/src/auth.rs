//! The pairing code, the limits on guessing it, and the paired devices.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::net;

/// Digits in a pairing code.
pub const CODE_DIGITS: usize = 6;
/// Wrong codes one peer may send within [`Limits::window`] before it gets 429.
/// A peer is one address ([`net::peer_group`]).
pub const WRONG_PER_WINDOW: usize = 5;
/// Wrong codes one peer may send until the code is rotated; after that only
/// that peer is refused.
pub const WRONG_PER_PEER: u32 = 10;
/// Wrong codes from all peers together after which pairing stays locked for
/// everyone until the code is rotated. Above [`WRONG_PER_PEER`], so one peer
/// alone never locks out the others. Only tries while a code works count,
/// since a guess can't succeed while none does.
pub const WRONG_BEFORE_LOCK: u32 = 40;
/// Devices remembered at once; pairing another drops the oldest.
pub const MAX_DEVICES: usize = 8;
/// A paired device that sends no request for this long is forgotten. The
/// cookie outlives it, so a lost phone stops working on its own.
pub const DEVICE_IDLE_LIMIT: Duration = Duration::from_secs(90 * 24 * 60 * 60);
/// How far a device's last-seen time moves before the file is written again,
/// so a busy page doesn't write it on every request.
const SEEN_SAVE_STEP: u64 = 24 * 60 * 60;
/// The paired-device file's format.
const FILE_VERSION: u64 = 1;

/// No randomness for a code or a token; a guessable one is never used instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RandomError;

fn random_bytes<const N: usize>() -> Result<[u8; N], RandomError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| RandomError)?;
    Ok(bytes)
}

/// A uniform 6-digit code, leading zeros kept.
pub fn random_code() -> Result<String, RandomError> {
    // The largest multiple of 10^6 below 2^32, so every code is equally likely.
    const LIMIT: u32 = 4_294_000_000;
    loop {
        let value = u32::from_le_bytes(random_bytes()?);
        if value < LIMIT {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

fn equal_in_constant_time(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The time limits of pairing; the defaults are the product's, tests shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How long a code works after it is made.
    pub code_lifetime: Duration,
    /// The window [`WRONG_PER_WINDOW`] counts in.
    pub window: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            code_lifetime: Duration::from_secs(10 * 60),
            window: Duration::from_secs(60),
        }
    }
}

/// What a pairing try came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairOutcome {
    /// The code was right; it is used up and a new one made.
    Paired,
    Wrong,
    /// No code works now: it expired or none could be made.
    Expired,
    /// This peer sent [`WRONG_PER_WINDOW`] wrong codes within the window.
    Limited,
    /// This peer sent [`WRONG_PER_PEER`] wrong codes; rotating the code lets it try again.
    Blocked,
    /// [`WRONG_BEFORE_LOCK`] wrong codes in total; rotating the code unlocks.
    Locked,
}

/// The current pairing code and the count of wrong tries.
#[derive(Debug)]
pub struct Pairing {
    limits: Limits,
    code: Option<(String, Instant)>,
    wrong_total: u32,
    /// Holds a peer only after a wrong try, so it never holds more than [`WRONG_BEFORE_LOCK`] peers.
    wrong_by_peer: HashMap<IpAddr, PeerTries>,
}

#[derive(Debug, Default)]
struct PeerTries {
    total: u32,
    recent: VecDeque<Instant>,
}

impl Pairing {
    pub fn new(limits: Limits, now: Instant) -> Result<Pairing, RandomError> {
        let mut pairing = Pairing {
            limits,
            code: None,
            wrong_total: 0,
            wrong_by_peer: HashMap::new(),
        };
        pairing.rotate(now)?;
        Ok(pairing)
    }

    /// A new code, as when the user reopens the setting: the old one stops
    /// working, the lockout and the counts are cleared.
    pub fn rotate(&mut self, now: Instant) -> Result<String, RandomError> {
        self.wrong_total = 0;
        self.wrong_by_peer.clear();
        self.code = None;
        let code = random_code()?;
        self.code = Some((code.clone(), now));
        Ok(code)
    }

    /// The code that works now, if any.
    pub fn current(&self, now: Instant) -> Option<&str> {
        self.code
            .as_ref()
            .filter(|(_, made)| now.saturating_duration_since(*made) < self.limits.code_lifetime)
            .map(|(code, _)| code.as_str())
    }

    pub fn locked(&self) -> bool {
        self.wrong_total >= WRONG_BEFORE_LOCK
    }

    /// One try from `from`. A limited, blocked or locked try is refused
    /// before the code is looked at, so the right code doesn't get through either.
    pub fn attempt(&mut self, from: IpAddr, code: &str, now: Instant) -> PairOutcome {
        if self.locked() {
            return PairOutcome::Locked;
        }
        let peer = net::peer_group(from);
        let window = self.limits.window;
        if let Some(tries) = self.wrong_by_peer.get_mut(&peer) {
            while tries
                .recent
                .front()
                .is_some_and(|time| now.saturating_duration_since(*time) >= window)
            {
                tries.recent.pop_front();
            }
            if tries.total >= WRONG_PER_PEER {
                return PairOutcome::Blocked;
            }
            if tries.recent.len() >= WRONG_PER_WINDOW {
                return PairOutcome::Limited;
            }
        }
        let outcome = match self.current(now) {
            Some(current) if equal_in_constant_time(current.as_bytes(), code.trim().as_bytes()) => {
                // Single use: the next device needs the next code.
                self.code = random_code().ok().map(|code| (code, now));
                return PairOutcome::Paired;
            }
            Some(_) => PairOutcome::Wrong,
            None => PairOutcome::Expired,
        };
        if outcome == PairOutcome::Wrong {
            self.wrong_total = self.wrong_total.saturating_add(1);
        } else if self.wrong_by_peer.len() >= WRONG_BEFORE_LOCK as usize
            && !self.wrong_by_peer.contains_key(&peer)
        {
            // Keeps the map bounded now that these tries don't move the total.
            return outcome;
        }
        let tries = self.wrong_by_peer.entry(peer).or_default();
        tries.total = tries.total.saturating_add(1);
        tries.recent.push_back(now);
        outcome
    }
}

/// A device token: 128 random bits as 32 hex digits.
pub fn random_token() -> Result<String, RandomError> {
    let bytes: [u8; 16] = random_bytes()?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

type TokenHash = [u8; 32];

fn token_hash(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(text: &str) -> Option<TokenHash> {
    // from_str_radix alone would read a leading "+", which the Mac's count rejects.
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut hash = [0u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

/// Seconds since 1970; a clock set before that reads as 0.
fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// One paired device: its token's hash and when it last sent a request.
#[derive(Clone, Copy, Debug)]
struct Device {
    hash: TokenHash,
    seen: u64,
    /// `seen` as the last snapshot had it.
    seen_saved: u64,
}

impl Device {
    /// A clock that jumped ahead may drop the device: from here that reads
    /// the same as a device left unused.
    fn idle_too_long(&self, now: u64) -> bool {
        now.saturating_sub(self.seen) >= DEVICE_IDLE_LIMIT.as_secs()
    }

    /// Whether the file's time for this device is a day or more off.
    fn needs_saving(&self) -> bool {
        self.seen.abs_diff(self.seen_saved) >= SEEN_SAVE_STEP
    }
}

/// The paired devices: only each token's SHA-256 and its last-seen time,
/// oldest first. A change returns a [`Snapshot`] that the caller saves after
/// letting go of its lock, so a slow disk never holds up a reader.
#[derive(Debug)]
pub struct TokenStore {
    path: Option<PathBuf>,
    devices: Vec<Device>,
    generation: u64,
    /// The file as read differs from the store: no last-seen times, idle
    /// devices or times ahead of the clock.
    rewrite: bool,
    /// The file as read, when a device in it didn't: copied aside by the
    /// first save that writes over it, never by a read alone.
    damaged: Option<Arc<[u8]>>,
}

/// Why the paired-device file was not read at start, or not all of it.
#[derive(Debug)]
pub enum LoadProblem {
    /// The store starts empty.
    Unreadable(io::Error),
    /// The store starts empty.
    NotInFormat,
    /// `skipped` devices didn't read and pair again; the others are kept.
    /// The first save copies the file aside before it writes over it.
    Skipped { skipped: usize },
}

/// The devices as one change left them, for [`DeviceFile::save`].
#[derive(Debug)]
#[must_use = "a change reaches the file only once its snapshot is saved"]
pub struct Snapshot {
    path: Option<PathBuf>,
    devices: Vec<(TokenHash, u64)>,
    generation: u64,
    /// The damaged file as read, to keep aside before writing over it.
    keep_first: Option<Arc<[u8]>>,
}

/// Writes snapshots to the paired-device file one at a time; one older
/// than the last written is skipped, so the file never goes back.
#[derive(Debug, Default)]
pub struct DeviceFile {
    written: Mutex<Written>,
}

#[derive(Debug, Default)]
struct Written {
    generation: u64,
    /// The damaged file was kept aside, so later writes needn't again.
    kept_aside: bool,
}

impl DeviceFile {
    pub fn save(&self, snapshot: Snapshot) -> io::Result<()> {
        let Some(path) = snapshot.path.as_deref() else {
            return Ok(());
        };
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        if snapshot.generation <= written.generation {
            return Ok(());
        }
        if let Some(bytes) = snapshot.keep_first.as_deref()
            && !written.kept_aside
        {
            // While the copy fails, the damaged file is not written over.
            keep_aside(path, bytes, unix_seconds(SystemTime::now()))?;
            written.kept_aside = true;
        }
        let hashes: Vec<String> = snapshot.devices.iter().map(|(hash, _)| hex(hash)).collect();
        let seen: Vec<u64> = snapshot.devices.iter().map(|(_, seen)| *seen).collect();
        // "seen" sits beside "devices" so a reader of the older layout still counts them.
        let text = json!({ "version": FILE_VERSION, "devices": hashes, "seen": seen }).to_string();
        write_replacing(path, text.as_bytes())?;
        written.generation = snapshot.generation;
        Ok(())
    }
}

impl TokenStore {
    /// The store at `path`; a missing file is an empty store, an unreadable
    /// or damaged one too, reported so the host can log it.
    pub fn open(path: Option<PathBuf>) -> (TokenStore, Option<LoadProblem>) {
        TokenStore::open_at(path, SystemTime::now())
    }

    /// [`TokenStore::open`] at `now`: devices idle too long are left out, a
    /// time ahead of the clock counts as now, and a file from before last-seen
    /// times were kept counts every device as seen now.
    pub fn open_at(path: Option<PathBuf>, now: SystemTime) -> (TokenStore, Option<LoadProblem>) {
        let mut store = TokenStore {
            path,
            devices: Vec::new(),
            generation: 0,
            rewrite: false,
            damaged: None,
        };
        let Some(path) = store.path.clone() else {
            return (store, None);
        };
        let now = unix_seconds(now);
        let problem = match fs::read(&path) {
            Ok(bytes) => match parse_file(&bytes, now) {
                Some((mut devices, upgraded, skipped)) => {
                    let read = devices.len();
                    devices.retain(|device| !device.idle_too_long(now));
                    for device in &mut devices {
                        device.seen = device.seen.min(now);
                    }
                    store.rewrite = upgraded
                        || devices.len() != read
                        || devices
                            .iter()
                            .any(|device| device.seen != device.seen_saved);
                    store.devices = devices;
                    (skipped > 0).then(|| {
                        store.damaged = Some(Arc::from(bytes));
                        store.rewrite = true;
                        LoadProblem::Skipped { skipped }
                    })
                }
                None => Some(LoadProblem::NotInFormat),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => Some(LoadProblem::Unreadable(error)),
        };
        (store, problem)
    }

    /// The snapshot that brings the file in line with what was loaded, once.
    /// Without it, a file no device uses would never lose its idle devices.
    pub fn take_rewrite(&mut self) -> Option<Snapshot> {
        std::mem::take(&mut self.rewrite).then(|| self.snapshot())
    }

    /// The devices held, idle or not; [`TokenStore::count_at`] leaves the idle out.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// The devices that still work at `now`.
    pub fn count_at(&self, now: SystemTime) -> usize {
        let now = unix_seconds(now);
        self.devices
            .iter()
            .filter(|device| !device.idle_too_long(now))
            .count()
    }

    fn position(&self, token: &str) -> Option<usize> {
        if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let hash = token_hash(token);
        // Every hash is compared, so the time taken doesn't tell which one matched.
        self.devices
            .iter()
            .enumerate()
            .fold(None, |found, (index, device)| {
                if equal_in_constant_time(&device.hash, &hash) {
                    Some(index)
                } else {
                    found
                }
            })
    }

    /// Whether `token` belongs to a paired device that hasn't been idle too long.
    pub fn contains(&self, token: &str, now: SystemTime) -> bool {
        let now = unix_seconds(now);
        self.position(token)
            .is_some_and(|index| !self.devices[index].idle_too_long(now))
    }

    /// A request carrying `token` at `now`: forgets the devices idle too
    /// long and marks this one seen. A time ahead of the clock counts as now,
    /// so a clock that jumped ahead and back keeps no device longer. The
    /// snapshot comes when the file needs writing: a device was forgotten, or
    /// a time in the file is a day or more off.
    pub fn check(&mut self, token: &str, now: SystemTime) -> (bool, Option<Snapshot>) {
        let now = unix_seconds(now);
        let before = self.devices.len();
        self.devices.retain(|device| !device.idle_too_long(now));
        for device in &mut self.devices {
            device.seen = device.seen.min(now);
        }
        let found = self.position(token);
        if let Some(index) = found {
            self.devices[index].seen = now;
        }
        let changed = self.devices.len() != before || self.devices.iter().any(Device::needs_saving);
        (found.is_some(), changed.then(|| self.snapshot()))
    }

    /// Remembers a new device's token, seen at `now`, dropping the oldest
    /// beyond [`MAX_DEVICES`]. The token works at once, saved or not, until
    /// the app quits.
    pub fn add(&mut self, token: &str, now: SystemTime) -> Snapshot {
        let now = unix_seconds(now);
        self.devices.push(Device {
            hash: token_hash(token),
            seen: now,
            seen_saved: now,
        });
        if self.devices.len() > MAX_DEVICES {
            let excess = self.devices.len() - MAX_DEVICES;
            self.devices.drain(..excess);
        }
        self.snapshot()
    }

    /// Forgets one device; none when it wasn't paired.
    pub fn remove(&mut self, token: &str) -> Option<Snapshot> {
        let hash = token_hash(token);
        let before = self.devices.len();
        self.devices.retain(|device| device.hash != hash);
        (self.devices.len() != before).then(|| self.snapshot())
    }

    /// Forgets every device; a damaged file is not kept aside, since the
    /// user asked to forget what it holds.
    pub fn clear(&mut self) -> Snapshot {
        self.devices.clear();
        self.damaged = None;
        self.snapshot()
    }

    fn snapshot(&mut self) -> Snapshot {
        self.generation += 1;
        for device in &mut self.devices {
            device.seen_saved = device.seen;
        }
        Snapshot {
            path: self.path.clone(),
            devices: self
                .devices
                .iter()
                .map(|device| (device.hash, device.seen))
                .collect(),
            generation: self.generation,
            keep_first: self.damaged.clone(),
        }
    }
}

/// The devices in `bytes`, whether the file needs writing again, and how
/// many devices didn't read. A device whose hash isn't hex is skipped; one
/// whose time isn't a whole number counts as seen at `now`, and so does
/// every device when the "seen" list is missing or of another length.
fn parse_file(bytes: &[u8], now: u64) -> Option<(Vec<Device>, bool, usize)> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if value.get("version")?.as_u64()? != FILE_VERSION {
        return None;
    }
    let entries = value.get("devices")?.as_array()?;
    let seen = value
        .get("seen")
        .and_then(Value::as_array)
        .filter(|seen| seen.len() == entries.len());
    let mut rewrite = seen.is_none();
    let mut skipped = 0;
    let mut devices = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Some(hash) = entry.as_str().and_then(from_hex) else {
            skipped += 1;
            continue;
        };
        let seen = match seen.map(|seen| seen[index].as_u64()) {
            Some(Some(seen)) => seen,
            Some(None) => {
                rewrite = true;
                now
            }
            None => now,
        };
        devices.push(Device {
            hash,
            seen,
            seen_saved: seen,
        });
    }
    if devices.len() > MAX_DEVICES {
        devices.drain(..devices.len() - MAX_DEVICES);
    }
    Some((devices, rewrite, skipped))
}

/// Keeps `bytes`, the file at `path` as read, in a new file beside it,
/// `remote-devices.ignored-<seconds>.json` (`-2` and on when taken), which
/// nothing later replaces.
fn keep_aside(path: &Path, bytes: &[u8], now: u64) -> io::Result<PathBuf> {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    for attempt in 1..=100u32 {
        let suffix = if attempt == 1 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let copy = path.with_file_name(format!("{stem}.ignored-{now}{suffix}.json"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&copy)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                file.sync_all()?;
                return Ok(copy);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}

/// Writes next to `path` and renames over it, so a crash leaves the old
/// file or the new one, never half of one.
fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn peer(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
    }

    fn short() -> Limits {
        Limits {
            code_lifetime: Duration::from_secs(600),
            window: Duration::from_secs(60),
        }
    }

    fn wrong_for(code: &str) -> String {
        let digit = (code.as_bytes()[0] - b'0' + 1) % 10;
        format!("{digit}{}", &code[1..])
    }

    // covers: S368
    #[test]
    fn codes_have_six_digits() {
        for _ in 0..200 {
            let code = random_code().unwrap();
            assert_eq!(code.len(), CODE_DIGITS);
            assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        }
    }

    // covers: S368
    #[test]
    fn the_right_code_pairs_once() {
        let now = Instant::now();
        let mut pairing = Pairing::new(short(), now).unwrap();
        let code = pairing.current(now).unwrap().to_owned();
        assert_eq!(pairing.attempt(peer(1), &code, now), PairOutcome::Paired);
        assert_eq!(pairing.attempt(peer(1), &code, now), PairOutcome::Wrong);
        let next = pairing.current(now).unwrap().to_owned();
        assert_ne!(next, code);
        assert_eq!(pairing.attempt(peer(1), &next, now), PairOutcome::Paired);
    }

    // covers: S368
    #[test]
    fn a_code_expires() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let later = start + Duration::from_secs(600);
        assert_eq!(pairing.current(later), None);
        assert_eq!(pairing.attempt(peer(1), &code, later), PairOutcome::Expired);
        let fresh = pairing.rotate(later).unwrap();
        assert_eq!(pairing.attempt(peer(1), &fresh, later), PairOutcome::Paired);
    }

    // covers: S369
    #[test]
    fn five_wrong_codes_a_minute_per_address_then_429() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for _ in 0..WRONG_PER_WINDOW {
            assert_eq!(pairing.attempt(peer(1), &wrong, start), PairOutcome::Wrong);
        }
        assert_eq!(pairing.attempt(peer(1), &code, start), PairOutcome::Limited);
        // Another address is counted on its own.
        assert_eq!(pairing.attempt(peer(2), &wrong, start), PairOutcome::Wrong);
        let next_minute = start + Duration::from_secs(60);
        assert_eq!(
            pairing.attempt(peer(1), &code, next_minute),
            PairOutcome::Paired
        );
    }

    /// Sends wrong codes from `from` a minute apart in fives, as fast as the window allows.
    fn send_wrong(pairing: &mut Pairing, from: IpAddr, wrong: &str, start: Instant, count: u32) {
        for index in 0..count {
            let at = start + Duration::from_secs(60 * u64::from(index) / WRONG_PER_WINDOW as u64);
            assert_eq!(
                pairing.attempt(from, wrong, at),
                PairOutcome::Wrong,
                "try {index}"
            );
        }
    }

    // covers: S369
    #[test]
    fn one_peer_out_of_tries_is_refused_while_others_still_pair() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        send_wrong(&mut pairing, peer(1), &wrong, start, WRONG_PER_PEER);
        let later = start + Duration::from_secs(300);
        assert_eq!(pairing.attempt(peer(1), &code, later), PairOutcome::Blocked);
        assert!(!pairing.locked());
        assert_eq!(pairing.attempt(peer(2), &code, later), PairOutcome::Paired);
        let fresh = pairing.rotate(later).unwrap();
        assert_eq!(pairing.attempt(peer(1), &fresh, later), PairOutcome::Paired);
    }

    // covers: S369
    #[test]
    fn two_ipv6_hosts_in_one_64_are_separate_peers() {
        // The server admits IPv6 peers only from its own /64, so all of them share it.
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        let host = |last: u16| IpAddr::V6(Ipv6Addr::new(0xfdab, 0, 0, 1, 0, 0, 0, last));
        send_wrong(&mut pairing, host(1), &wrong, start, WRONG_PER_PEER);
        let later = start + Duration::from_secs(300);
        assert_eq!(pairing.attempt(host(1), &code, later), PairOutcome::Blocked);
        assert_eq!(pairing.attempt(host(2), &code, later), PairOutcome::Paired);
    }

    // covers: S369
    #[test]
    fn one_peer_alone_never_locks_pairing() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        send_wrong(&mut pairing, peer(1), &wrong, start, WRONG_PER_PEER);
        for minute in 5..5 + u64::from(WRONG_BEFORE_LOCK) {
            let at = start + Duration::from_secs(60 * minute);
            assert_eq!(pairing.attempt(peer(1), &wrong, at), PairOutcome::Blocked);
        }
        assert!(!pairing.locked());
    }

    // covers: S369
    #[test]
    fn forty_wrong_codes_from_several_peers_lock_pairing_until_rotation() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for index in 0..WRONG_BEFORE_LOCK {
            let from = peer(1 + (index as usize / WRONG_PER_WINDOW) as u8);
            assert_eq!(pairing.attempt(from, &wrong, start), PairOutcome::Wrong);
        }
        assert!(pairing.locked());
        assert_eq!(pairing.attempt(peer(50), &code, start), PairOutcome::Locked);
        let later = start + Duration::from_secs(300);
        assert_eq!(pairing.attempt(peer(50), &code, later), PairOutcome::Locked);
        let fresh = pairing.rotate(later).unwrap();
        assert!(!pairing.locked());
        assert_eq!(pairing.attempt(peer(1), &fresh, later), PairOutcome::Paired);
    }

    // covers: S369
    #[test]
    fn wrong_tries_while_no_code_works_never_lock_pairing() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        let expired = start + Duration::from_secs(600);
        for _ in 0..WRONG_PER_WINDOW {
            assert_eq!(
                pairing.attempt(peer(1), &wrong, expired),
                PairOutcome::Expired
            );
        }
        for last in 2..=200 {
            assert_eq!(
                pairing.attempt(peer(last), &wrong, expired),
                PairOutcome::Expired
            );
        }
        assert!(!pairing.locked());
        assert!(pairing.wrong_by_peer.len() <= WRONG_BEFORE_LOCK as usize);
        // The per-peer limits still hold while no code works.
        assert_eq!(
            pairing.attempt(peer(1), &wrong, expired),
            PairOutcome::Limited
        );
        let fresh = pairing.rotate(expired).unwrap();
        assert_eq!(
            pairing.attempt(peer(1), &fresh, expired),
            PairOutcome::Paired
        );
    }

    // covers: S369
    #[test]
    fn wrong_tries_while_a_code_works_still_lock_after_expired_ones() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for index in 0..WRONG_BEFORE_LOCK - 1 {
            let from = peer(1 + (index as usize / WRONG_PER_WINDOW) as u8);
            assert_eq!(pairing.attempt(from, &wrong, start), PairOutcome::Wrong);
        }
        let expired = start + Duration::from_secs(600);
        assert_eq!(
            pairing.attempt(peer(60), &wrong, expired),
            PairOutcome::Expired
        );
        assert!(!pairing.locked(), "an expired try is not the fortieth");
        let fresh = pairing.rotate(expired).unwrap();
        let wrong = wrong_for(&fresh);
        for index in 0..WRONG_BEFORE_LOCK {
            let from = peer(1 + (index as usize / WRONG_PER_WINDOW) as u8);
            assert_eq!(pairing.attempt(from, &wrong, expired), PairOutcome::Wrong);
        }
        assert!(pairing.locked());
        assert_eq!(
            pairing.attempt(peer(60), &fresh, expired),
            PairOutcome::Locked
        );
    }

    // covers: S369
    #[test]
    fn tries_refused_as_limited_never_count_toward_the_lock() {
        // Otherwise one address could lock pairing for every phone within a minute.
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for _ in 0..WRONG_PER_WINDOW {
            assert_eq!(pairing.attempt(peer(1), &wrong, start), PairOutcome::Wrong);
        }
        for _ in 0..WRONG_BEFORE_LOCK * 2 {
            assert_eq!(
                pairing.attempt(peer(1), &wrong, start),
                PairOutcome::Limited
            );
        }
        assert!(!pairing.locked());
        assert_eq!(pairing.attempt(peer(2), &code, start), PairOutcome::Paired);
    }

    // covers: S368, S369
    #[test]
    fn rotating_retires_the_old_code_and_clears_the_limit() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let old = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&old);
        for _ in 0..WRONG_PER_WINDOW {
            pairing.attempt(peer(1), &wrong, start);
        }
        assert_eq!(pairing.attempt(peer(1), &old, start), PairOutcome::Limited);
        let fresh = pairing.rotate(start).unwrap();
        assert_eq!(pairing.current(start), Some(fresh.as_str()));
        if fresh != old {
            assert_eq!(pairing.attempt(peer(1), &old, start), PairOutcome::Wrong);
        }
        assert_eq!(pairing.attempt(peer(1), &fresh, start), PairOutcome::Paired);
    }

    // covers: S368
    #[test]
    fn a_code_is_matched_whole_after_trimming() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let near_misses = [
            String::new(),
            code[..5].to_owned(),
            format!("{code}0"),
            format!("0{code}"),
            format!("{} {}", &code[..3], &code[3..]),
        ];
        for (index, near) in near_misses.iter().enumerate() {
            assert_eq!(
                pairing.attempt(peer(10 + index as u8), near, start),
                PairOutcome::Wrong,
                "{near:?}"
            );
        }
        assert_eq!(
            pairing.attempt(peer(1), &format!(" \t{code}\n"), start),
            PairOutcome::Paired
        );
    }

    // covers: S369
    #[test]
    fn the_wrong_code_window_slides_with_each_try() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for second in 0..WRONG_PER_WINDOW as u64 {
            let at = start + Duration::from_secs(second * 10);
            assert_eq!(pairing.attempt(peer(1), &wrong, at), PairOutcome::Wrong);
        }
        let almost = start + Duration::from_secs(60) - Duration::from_millis(1);
        assert_eq!(
            pairing.attempt(peer(1), &code, almost),
            PairOutcome::Limited
        );
        // The first wrong try leaves the window, so exactly one more is let through.
        let first_gone = start + Duration::from_secs(60);
        assert_eq!(
            pairing.attempt(peer(1), &wrong, first_gone),
            PairOutcome::Wrong
        );
        assert_eq!(
            pairing.attempt(peer(1), &code, first_gone),
            PairOutcome::Limited
        );
        let second_gone = start + Duration::from_secs(70);
        assert_eq!(
            pairing.attempt(peer(1), &code, second_gone),
            PairOutcome::Paired
        );
    }

    /// This process's folder under `parent`, after removing the folders there
    /// that no run has touched for an hour.
    fn process_folder(parent: PathBuf) -> PathBuf {
        let pid = std::process::id().to_string();
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for entry in fs::read_dir(&parent).into_iter().flatten().flatten() {
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|time| time < stale);
            if old && entry.file_name() != pid.as_str() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        parent.join(pid)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = process_folder(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("test-scratch"),
        )
        .join(name);
        let _ = fs::remove_dir_all(&dir);
        dir.join("remote-devices.json")
    }

    // covers: S371
    #[test]
    fn tokens_are_kept_as_hashes_eight_at_most() {
        let path = scratch("auth-tokens");
        let (mut store, problem) = TokenStore::open(Some(path.clone()));
        assert!(problem.is_none());
        let device_file = DeviceFile::default();
        let tokens: Vec<String> = (0..MAX_DEVICES + 1)
            .map(|_| random_token().unwrap())
            .collect();
        for token in &tokens {
            device_file
                .save(store.add(token, SystemTime::now()))
                .unwrap();
        }
        assert_eq!(store.len(), MAX_DEVICES);
        assert!(
            !store.contains(&tokens[0], SystemTime::now()),
            "the oldest is dropped"
        );
        assert!(
            tokens[1..]
                .iter()
                .all(|token| store.contains(token, SystemTime::now()))
        );

        let file = fs::read_to_string(&path).unwrap();
        assert!(tokens.iter().all(|token| !file.contains(token.as_str())));
        assert!(file.contains(&hex(&token_hash(&tokens[1]))));

        let (reopened, problem) = TokenStore::open(Some(path.clone()));
        assert!(problem.is_none());
        assert_eq!(reopened.len(), MAX_DEVICES);
        assert!(reopened.contains(&tokens[MAX_DEVICES], SystemTime::now()));

        device_file.save(store.remove(&tokens[3]).unwrap()).unwrap();
        assert!(store.remove(&tokens[3]).is_none());
        assert!(
            !TokenStore::open(Some(path.clone()))
                .0
                .contains(&tokens[3], SystemTime::now())
        );
        device_file.save(store.clear()).unwrap();
        assert!(TokenStore::open(Some(path)).0.is_empty());
    }

    // covers: S372
    #[test]
    fn a_snapshot_saved_late_never_takes_the_file_back() {
        let path = scratch("auth-order");
        let (mut store, _) = TokenStore::open(Some(path.clone()));
        let device_file = DeviceFile::default();
        let first = random_token().unwrap();
        let second = random_token().unwrap();
        let older = store.add(&first, SystemTime::now());
        let newer = store.add(&second, SystemTime::now());
        device_file.save(newer).unwrap();
        device_file.save(older).unwrap();
        assert!(
            TokenStore::open(Some(path))
                .0
                .contains(&second, SystemTime::now())
        );
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn day_zero() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    // covers: S371
    #[test]
    fn a_device_idle_for_ninety_days_is_forgotten() {
        let path = scratch("auth-idle");
        let start = day_zero();
        let (mut store, _) = TokenStore::open_at(Some(path.clone()), start);
        let device_file = DeviceFile::default();
        let token = random_token().unwrap();
        let other = random_token().unwrap();
        device_file.save(store.add(&token, start)).unwrap();
        device_file.save(store.add(&other, start)).unwrap();

        // Every request starts the ninety days over.
        let used = start + DAY * 89;
        assert!(store.check(&token, used).0);
        let idle = used + DAY * 89;
        assert!(store.check(&token, idle).0);
        assert!(!store.contains(&other, idle), "the other went unused");
        assert_eq!(store.count_at(idle), 1);

        let (paired, snapshot) = store.check(&token, idle + DAY);
        assert!(paired);
        device_file
            .save(snapshot.expect("a forgotten device is written out"))
            .unwrap();
        assert_eq!(store.len(), 1);
        let (reopened, _) = TokenStore::open_at(Some(path), idle + DAY);
        assert_eq!(reopened.len(), 1);
        assert!(reopened.contains(&token, idle + DAY * 90 - Duration::from_secs(1)));
        assert!(!reopened.contains(&token, idle + DAY * 91));
    }

    // covers: S371
    #[test]
    fn last_seen_is_written_once_a_day_and_survives_a_restart() {
        let path = scratch("auth-seen");
        let start = day_zero();
        let (mut store, _) = TokenStore::open_at(Some(path.clone()), start);
        let device_file = DeviceFile::default();
        let token = random_token().unwrap();
        device_file.save(store.add(&token, start)).unwrap();
        let (paired, snapshot) = store.check(&token, start + DAY / 2);
        assert!(paired && snapshot.is_none());
        let (_, snapshot) = store.check(&token, start + DAY * 2);
        device_file.save(snapshot.expect("a day later")).unwrap();
        let (_, snapshot) = store.check(&token, start + DAY * 2);
        assert!(snapshot.is_none());

        let (reopened, _) = TokenStore::open_at(Some(path), start + DAY * 2);
        assert!(reopened.contains(&token, start + DAY * 91));
        assert!(!reopened.contains(&token, start + DAY * 92));
    }

    // covers: S372
    #[test]
    fn a_file_without_last_seen_counts_every_device_as_seen_now() {
        let path = scratch("auth-upgrade");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let token = random_token().unwrap();
        let text = json!({ "version": 1, "devices": [hex(&token_hash(&token))] }).to_string();
        fs::write(&path, text).unwrap();
        let opened = day_zero() + DAY * 400;
        let (mut store, problem) = TokenStore::open_at(Some(path.clone()), opened);
        assert!(problem.is_none());
        assert!(store.contains(&token, opened + DAY * 89));
        assert!(!store.contains(&token, opened + DAY * 90));

        // Written once at load, so restarts without a request still let it expire.
        DeviceFile::default()
            .save(store.take_rewrite().expect("an upgraded file is written"))
            .unwrap();
        assert!(store.take_rewrite().is_none());
        let (mut reopened, _) = TokenStore::open_at(Some(path.clone()), opened + DAY * 60);
        assert!(reopened.take_rewrite().is_none());
        let (reopened, _) = TokenStore::open_at(Some(path), opened + DAY * 90);
        assert!(reopened.is_empty());
    }

    // covers: S371
    #[test]
    fn a_clock_that_jumped_ahead_and_back_keeps_no_device_longer() {
        let path = scratch("auth-clock-back");
        let start = day_zero();
        let ahead = start + DAY * 80;
        let (mut store, _) = TokenStore::open_at(Some(path.clone()), start);
        let device_file = DeviceFile::default();
        let token = random_token().unwrap();
        device_file.save(store.add(&token, start)).unwrap();
        device_file
            .save(store.check(&token, ahead).1.expect("a year later"))
            .unwrap();

        let (paired, snapshot) = store.check(&token, start + DAY);
        assert!(paired);
        device_file
            .save(snapshot.expect("a time ahead of the clock is written back"))
            .unwrap();
        assert!(!store.contains(&token, start + DAY * 91));

        device_file
            .save(store.check(&token, ahead).1.unwrap())
            .unwrap();
        let (mut reopened, _) = TokenStore::open_at(Some(path.clone()), start + DAY);
        DeviceFile::default()
            .save(
                reopened
                    .take_rewrite()
                    .expect("a loaded time ahead of the clock"),
            )
            .unwrap();
        assert!(reopened.contains(&token, start + DAY * 90));
        let (reopened, _) = TokenStore::open_at(Some(path), start + DAY * 91);
        assert!(reopened.is_empty());
    }

    // covers: S371
    #[test]
    fn a_clock_that_jumps_ninety_days_ahead_forgets_every_device() {
        let start = day_zero();
        let (mut store, _) = TokenStore::open_at(None, start);
        let token = random_token().unwrap();
        let other = random_token().unwrap();
        let _ = store.add(&token, start);
        let _ = store.add(&other, start);
        let (paired, snapshot) = store.check(&token, start + DAY * 90);
        assert!(!paired, "the device sending the request goes too");
        assert!(snapshot.is_some());
        assert!(store.is_empty());
    }

    // covers: S372
    #[test]
    fn a_last_seen_value_that_isnt_a_list_counts_every_device_as_seen_now() {
        let path = scratch("auth-seen-damaged");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let token = random_token().unwrap();
        let text =
            json!({ "version": 1, "devices": [hex(&token_hash(&token))], "seen": "x" }).to_string();
        fs::write(&path, text).unwrap();
        let (mut store, problem) = TokenStore::open_at(Some(path), day_zero());
        assert!(problem.is_none());
        assert!(store.contains(&token, day_zero() + DAY * 89));
        assert!(store.take_rewrite().is_some());
    }

    // covers: S372
    #[test]
    fn a_hash_reads_only_as_two_hex_digits_per_byte() {
        let hash = token_hash("phone");
        let text = hex(&hash);
        assert_eq!(from_hex(&text), Some(hash));
        assert_eq!(from_hex(&text.to_uppercase()), Some(hash));
        let signed = format!("+{}", &text[1..]);
        assert_eq!(from_hex(&signed), None, "{signed}");
        let file = json!({ "version": 1, "devices": [text, signed] }).to_string();
        let (devices, _, skipped) = parse_file(file.as_bytes(), 0).unwrap();
        assert_eq!((devices.len(), skipped), (1, 1));
    }

    // covers: S372
    #[test]
    fn a_whole_number_written_as_a_float_doesnt_read() {
        let hash = hex(&token_hash("phone"));
        for version in ["1.0", "1e0"] {
            let file = format!(r#"{{"version":{version},"devices":["{hash}"]}}"#);
            assert!(parse_file(file.as_bytes(), 5).is_none(), "{version}");
        }
        for seen in ["1.8e9", "1800000000.0", "-1"] {
            let file = format!(r#"{{"version":1,"devices":["{hash}"],"seen":[{seen}]}}"#);
            let (devices, rewrite, _) = parse_file(file.as_bytes(), 5).unwrap();
            assert_eq!(devices[0].seen, 5, "{seen} counts as seen now");
            assert!(rewrite, "{seen}");
        }
    }

    /// What the copies kept aside next to `path` hold.
    fn copies_kept_aside(path: &Path) -> Vec<Vec<u8>> {
        let mut copies: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|copy| {
                copy.file_name().is_some_and(|name| {
                    name.to_string_lossy().starts_with("remote-devices.ignored")
                })
            })
            .collect();
        copies.sort();
        copies.iter().map(|copy| fs::read(copy).unwrap()).collect()
    }

    // covers: S372
    #[test]
    fn opening_a_file_with_an_unreadable_device_without_saving_leaves_no_copy() {
        let path = scratch("auth-bad-device-only-counted");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let token = random_token().unwrap();
        let text = json!({ "version": 1, "devices": [hex(&token_hash(&token)), "zz"] }).to_string();
        fs::write(&path, &text).unwrap();
        // The Web Remote window opens the store only to count, while the
        // remote is off, every time it shows.
        for _ in 0..2 {
            let (store, _) = TokenStore::open_at(Some(path.clone()), day_zero());
            assert_eq!(store.len(), 1);
        }
        assert!(copies_kept_aside(&path).is_empty());
        assert_eq!(fs::read(&path).unwrap(), text.into_bytes());

        // Forget every device, from the window while the remote is off:
        // what the user asked to forget isn't kept in a copy.
        let (mut store, _) = TokenStore::open_at(Some(path.clone()), day_zero());
        DeviceFile::default().save(store.clear()).unwrap();
        assert!(copies_kept_aside(&path).is_empty());
        assert!(TokenStore::open_at(Some(path), day_zero()).0.is_empty());
    }

    // covers: S372
    #[test]
    fn an_unreadable_device_costs_only_that_device_and_the_file_is_kept_aside_first() {
        let path = scratch("auth-one-bad-device");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let now = day_zero();
        let seen = unix_seconds(now);
        let first = random_token().unwrap();
        let second = random_token().unwrap();
        let third = random_token().unwrap();
        let text = json!({
            "version": 1,
            "devices": [hex(&token_hash(&first)), "zz", 5, hex(&token_hash(&second)), hex(&token_hash(&third))],
            "seen": [seen, seen, seen, "yesterday", seen]
        })
        .to_string();
        fs::write(&path, &text).unwrap();
        let (mut store, _) = TokenStore::open_at(Some(path.clone()), now);
        assert_eq!(store.len(), 3);
        for token in [&first, &second, &third] {
            assert!(store.contains(token, now + DAY * 89));
        }
        DeviceFile::default()
            .save(
                store
                    .take_rewrite()
                    .expect("the bad entries are written out"),
            )
            .unwrap();
        assert_eq!(copies_kept_aside(&path), [text.into_bytes()]);
        let (reopened, problem) = TokenStore::open_at(Some(path.clone()), now);
        assert!(problem.is_none());
        assert_eq!(reopened.len(), 3);

        // A last-seen list of another length: every device counts as seen now.
        let text =
            json!({ "version": 1, "devices": [hex(&token_hash(&first))], "seen": [] }).to_string();
        fs::write(&path, text).unwrap();
        let (store, _) = TokenStore::open_at(Some(path), now);
        assert!(store.contains(&first, now + DAY * 89));
    }

    // covers: S372
    #[test]
    fn a_damaged_file_starts_empty() {
        let path = scratch("auth-damaged");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for damaged in [
            &b"not json"[..],
            b"{\"version\": \"1\", \"devices\": []}",
            b"{\"version\": 1, \"devices\": \"zz\"}",
        ] {
            fs::write(&path, damaged).unwrap();
            let (store, problem) = TokenStore::open(Some(path.clone()));
            assert!(store.is_empty());
            assert!(matches!(problem, Some(LoadProblem::NotInFormat)));
        }
        fs::write(&path, b"{\"version\": 1, \"devices\": [\"zz\"]}").unwrap();
        let (store, problem) = TokenStore::open(Some(path));
        assert!(store.is_empty());
        assert!(matches!(problem, Some(LoadProblem::Skipped { skipped: 1 })));
    }

    // covers: S370
    #[test]
    fn only_well_formed_tokens_are_looked_up() {
        let (mut store, _) = TokenStore::open(None);
        let token = random_token().unwrap();
        DeviceFile::default()
            .save(store.add(&token, SystemTime::now()))
            .unwrap();
        assert!(store.contains(&token, SystemTime::now()));
        assert!(!store.contains("", SystemTime::now()));
        assert!(!store.contains(&token[..31], SystemTime::now()));
        assert!(!store.contains(&format!("{}g", &token[..31]), SystemTime::now()));
    }
}
