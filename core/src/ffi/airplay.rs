//! AudioSync's own AirPlay sender ([`audiosync_airplay`]) through the C
//! interface, for the macOS app, which links one Rust static library and so
//! reaches the sender through the core's.
//!
//! This first stage carries discovery only: the host creates a sender,
//! starts browsing the local network and takes the receivers it finds, one
//! change at a time, with their ID, name, model, whether they can play from
//! this sender and discovery's hints. No audio crosses this interface yet,
//! and nothing secret ever does: no password, PIN, key or credential.
//!
//! Threads: every `asc_airplay_*` call on a handle runs on one control
//! thread at a time (never an audio thread: they allocate, and
//! `asc_airplay_destroy` waits for the sender's threads), and
//! `asc_airplay_destroy` runs once no other call on that handle can run. The
//! sender itself runs its own threads and sockets (P6), which this module
//! never touches.

use std::ffi::{CString, c_char};
use std::ptr;

use audiosync_airplay::discovery::{DeviceKey, ServiceKind};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::sender::{ReceiverEvent, ReceiverInfo, Sender, SenderConfig, SenderEvent};

/// What a receiver event reports ([`AscAirPlayReceiver::event`]): discovery
/// found a receiver.
pub const AIRPLAY_APPEARED: i32 = 1;
/// Its records or its verdict changed.
pub const AIRPLAY_CHANGED: i32 = 2;
/// It left the network, or discovery stopped.
pub const AIRPLAY_GONE: i32 = 3;

/// Whether a receiver can play from this sender
/// ([`AscAirPlayReceiver::availability`]): it can.
pub const AIRPLAY_AVAILABLE: i32 = 0;
/// An Apple TV that asks for a PIN once (the legacy pairing, P46).
pub const AIRPLAY_NEEDS_PIN: i32 = 1;
/// It asks for a password (P39).
pub const AIRPLAY_NEEDS_PASSWORD: i32 = 2;
/// It likely needs another kind of pairing (P16).
pub const AIRPLAY_NEEDS_PAIRING: i32 = 3;
/// Only AirPlay 2 could serve it, and that path is off (P2).
pub const AIRPLAY_AIRPLAY2_ONLY: i32 = 4;
/// The sender can't drive it (P139); [`AscAirPlayReceiver::unsupported`]
/// says why.
pub const AIRPLAY_UNSUPPORTED: i32 = 5;

/// Why a receiver can't play ([`AscAirPlayReceiver::unsupported`]): no
/// reason, it can (or the reason is not one of these).
pub const AIRPLAY_REASON_NONE: i32 = 0;
/// No encryption mode the sender supports.
pub const AIRPLAY_REASON_ENCRYPTION: i32 = 1;
/// No codec the sender may use.
pub const AIRPLAY_REASON_CODEC: i32 = 2;
/// No UDP transport.
pub const AIRPLAY_REASON_TRANSPORT: i32 = 3;
/// Its methods lack ANNOUNCE, SETUP or RECORD.
pub const AIRPLAY_REASON_METHODS: i32 = 4;
/// A format other than 44 100 Hz, 16-bit stereo.
pub const AIRPLAY_REASON_FORMAT: i32 = 5;
/// Only AirPlay 2 could serve it.
pub const AIRPLAY_REASON_AIRPLAY2_ONLY: i32 = 6;

/// What discovery saw ([`AscAirPlayReceiver::flags`]), bit by bit: it
/// announces `_raop._tcp` (AirPlay 1 audio).
pub const AIRPLAY_FLAG_RAOP: u32 = 1;
/// It announces `_airplay._tcp`.
pub const AIRPLAY_FLAG_AIRPLAY: u32 = 2;
/// Its model is an Apple TV.
pub const AIRPLAY_FLAG_APPLE_TV: u32 = 4;
/// Its records say it needs a password.
pub const AIRPLAY_FLAG_PASSWORD: u32 = 8;
/// Its flags hint that it needs pairing (only a hint, P46).
pub const AIRPLAY_FLAG_PAIRING: u32 = 16;
/// An Apple TV that needs the legacy PIN pairing.
pub const AIRPLAY_FLAG_LEGACY_PAIRING: u32 = 32;
/// Its flags hint that another sender may be playing (only a hint, P137).
pub const AIRPLAY_FLAG_BUSY: u32 = 64;
/// It lists AirPlay 2 features.
pub const AIRPLAY_FLAG_AIRPLAY2: u32 = 128;

/// One change discovery saw. The strings are NUL-terminated UTF-8 owned by
/// the sender handle, valid until the next `asc_airplay_next_receiver` call
/// on it or its destroy; a gone receiver has only its ID, its name and
/// model are empty and its availability is `AIRPLAY_AVAILABLE`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscAirPlayReceiver {
    pub event: i32,
    pub id: *const c_char,
    pub name: *const c_char,
    pub model: *const c_char,
    pub availability: i32,
    pub unsupported: i32,
    pub flags: u32,
}

/// An AirPlay sender behind an opaque C pointer.
pub struct AscAirPlay {
    sender: Sender,
    /// The stream's audio input. Nothing writes to it yet; a later stage
    /// hands it to the capture thread.
    _input: InputWriter,
    /// The strings of the last receiver handed out: ID, name, model.
    strings: [CString; 3],
}

/// The ID a receiver goes by, the one the Windows app uses too: its device
/// ID (12 uppercase hex digits), or, for a service without one, its kind
/// and instance name (`raop:<instance>`, `airplay:<instance>`).
fn receiver_id(key: &DeviceKey) -> String {
    match key {
        DeviceKey::Id(id) => id.to_string(),
        DeviceKey::Instance(ServiceKind::Raop, instance) => format!("raop:{instance}"),
        DeviceKey::Instance(ServiceKind::AirPlay, instance) => format!("airplay:{instance}"),
    }
}

/// The reason code of an unsupported receiver.
fn reason_code(reason: UnsupportedReason) -> i32 {
    match reason {
        UnsupportedReason::Encryption => AIRPLAY_REASON_ENCRYPTION,
        UnsupportedReason::Codec => AIRPLAY_REASON_CODEC,
        UnsupportedReason::Transport => AIRPLAY_REASON_TRANSPORT,
        UnsupportedReason::Methods => AIRPLAY_REASON_METHODS,
        UnsupportedReason::Format => AIRPLAY_REASON_FORMAT,
        UnsupportedReason::AirPlay2Only => AIRPLAY_REASON_AIRPLAY2_ONLY,
    }
}

/// Whether a receiver can play, and if not, why: its plan first (P139,
/// P2), then discovery's hints, the legacy pairing before a password, as
/// the Windows app decides. There is no pairing store behind this
/// interface yet, so an Apple TV that asks for a PIN always needs one.
fn availability(info: &ReceiverInfo) -> (i32, i32) {
    let verdict = &info.device.verdict;
    match info.plan {
        Err(ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only)) => {
            (AIRPLAY_AIRPLAY2_ONLY, AIRPLAY_REASON_AIRPLAY2_ONLY)
        }
        Err(ErrorKind::Unsupported(reason)) => (AIRPLAY_UNSUPPORTED, reason_code(reason)),
        Err(_) => (AIRPLAY_UNSUPPORTED, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.legacy_pairing => (AIRPLAY_NEEDS_PIN, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.needs_password => (AIRPLAY_NEEDS_PASSWORD, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.needs_pairing => (AIRPLAY_NEEDS_PAIRING, AIRPLAY_REASON_NONE),
        Ok(()) => (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE),
    }
}

/// Discovery's view of a receiver as flag bits.
fn flags(info: &ReceiverInfo) -> u32 {
    let device = &info.device;
    let verdict = &device.verdict;
    [
        (device.raop.is_some(), AIRPLAY_FLAG_RAOP),
        (device.airplay.is_some(), AIRPLAY_FLAG_AIRPLAY),
        (device.is_apple_tv(), AIRPLAY_FLAG_APPLE_TV),
        (verdict.needs_password, AIRPLAY_FLAG_PASSWORD),
        (verdict.needs_pairing, AIRPLAY_FLAG_PAIRING),
        (verdict.legacy_pairing, AIRPLAY_FLAG_LEGACY_PAIRING),
        (verdict.busy_hint, AIRPLAY_FLAG_BUSY),
        (verdict.airplay2, AIRPLAY_FLAG_AIRPLAY2),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .fold(0, |bits, (_, bit)| bits | bit)
}

/// The receiver's model: AirPlay `model` when present, otherwise RAOP `am`.
fn model(info: &ReceiverInfo) -> String {
    let device = &info.device;
    device
        .airplay_txt
        .as_ref()
        .and_then(|txt| txt.model.clone())
        .or_else(|| device.raop_txt.as_ref().and_then(|txt| txt.model.clone()))
        .unwrap_or_default()
}

/// `text` as a C string; a NUL inside (which no DNS-SD name should carry)
/// becomes U+FFFD.
fn c_string(text: &str) -> CString {
    CString::new(text.replace('\0', "\u{FFFD}")).unwrap_or_default()
}

impl AscAirPlay {
    /// Keeps the strings of `event` and describes it with pointers to them.
    fn report(&mut self, event: ReceiverEvent) -> AscAirPlayReceiver {
        let (code, id, name, model, (availability, unsupported), flags) = match &event {
            ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info) => (
                if matches!(event, ReceiverEvent::Appeared(_)) {
                    AIRPLAY_APPEARED
                } else {
                    AIRPLAY_CHANGED
                },
                receiver_id(&info.device.key),
                info.device.name.clone(),
                model(info),
                availability(info),
                flags(info),
            ),
            ReceiverEvent::Gone(key) => (
                AIRPLAY_GONE,
                receiver_id(key),
                String::new(),
                String::new(),
                (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE),
                0,
            ),
        };
        self.strings = [c_string(&id), c_string(&name), c_string(&model)];
        AscAirPlayReceiver {
            event: code,
            id: self.strings[0].as_ptr(),
            name: self.strings[1].as_ptr(),
            model: self.strings[2].as_ptr(),
            availability,
            unsupported,
            flags,
        }
    }
}

/// Starts an AirPlay sender: its stream, silent until a later stage gives
/// it audio, and its driver thread. Discovery waits for
/// [`asc_airplay_start_discovery`]. Null when the thread can't start.
#[unsafe(no_mangle)]
pub extern "C" fn asc_airplay_create() -> *mut AscAirPlay {
    match Sender::start(SenderConfig::default()) {
        Ok((sender, input)) => Box::into_raw(Box::new(AscAirPlay {
            sender,
            _input: input,
            strings: Default::default(),
        })),
        Err(_) => ptr::null_mut(),
    }
}

/// Stops the sender and frees it: discovery stops and its threads end
/// within about 2 s, which this call waits for (P129), so a host calls it
/// off its main thread. Sets `*airplay` to null, so destroying through the
/// same pointer again does nothing.
///
/// # Safety
///
/// `airplay` is null or points to a pointer that is null or was returned by
/// [`asc_airplay_create`] and not destroyed yet; no other call on that
/// handle runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_destroy(airplay: *mut *mut AscAirPlay) {
    if airplay.is_null() {
        return;
    }
    // SAFETY: by the caller's contract `airplay` points to a readable and
    // writable handle pointer.
    let handle = unsafe { airplay.replace(ptr::null_mut()) };
    if handle.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_airplay_create, and
    // by the caller's contract it is destroyed once (the host's pointer is
    // now null), with no call in progress.
    drop(unsafe { Box::from_raw(handle) });
}

/// Starts browsing the local network for AirPlay receivers over multicast
/// DNS (P7); they arrive through [`asc_airplay_next_receiver`]. False when
/// the handle is null or multicast DNS can't start.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_start_discovery(airplay: *mut AscAirPlay) -> bool {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return false;
    };
    airplay.sender.start_discovery().is_ok()
}

/// Stops browsing; every receiver discovery listed is then reported gone.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_stop_discovery(airplay: *mut AscAirPlay) {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    if let Some(airplay) = unsafe { airplay.as_mut() } {
        airplay.sender.stop_discovery();
    }
}

/// Takes the next change discovery saw, if one is waiting, into
/// `*receiver` and returns true. Returns false, leaving `*receiver` as it
/// was, when none is waiting or either pointer is null. Never waits. The
/// sender's other events (there are none before audio comes in a later
/// stage) are passed over.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `receiver` is null or points to a writable
/// `AscAirPlayReceiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_next_receiver(
    airplay: *mut AscAirPlay,
    receiver: *mut AscAirPlayReceiver,
) -> bool {
    if receiver.is_null() {
        return false;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return false;
    };
    while let Some(event) = airplay.sender.poll_event() {
        if let SenderEvent::Receiver(event) = event {
            let report = airplay.report(event);
            // SAFETY: non-null, and by the caller's contract writable.
            unsafe { receiver.write(report) };
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};

    use audiosync_airplay::discovery::{BrowseEvent, Browser, ScopedAddr, ServiceRecord};

    /// A browser that hands over a fixed list of events, one per poll.
    struct Script(Vec<BrowseEvent>);

    impl Browser for Script {
        fn poll_event(&mut self) -> Option<BrowseEvent> {
            (!self.0.is_empty()).then(|| self.0.remove(0))
        }
    }

    /// A RAOP record at a documentation address.
    fn raop(id: &str, name: &str, txt: &[&str]) -> ServiceRecord {
        ServiceRecord {
            kind: ServiceKind::Raop,
            instance: format!("{id}@{name}"),
            host: format!("{}.local", name.replace(' ', "-")),
            port: 7000,
            txt: txt.iter().map(|s| s.as_bytes().to_vec()).collect(),
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20)))],
        }
    }

    const OPEN: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "am=Speaker1,1",
    ];

    const APPLE_TV: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0,3,5",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "am=AppleTV3,2",
        "sf=0x8",
    ];

    const PASSWORD: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "pw=true",
    ];

    const RSA_ONLY: &[&str] = &["txtvers=1", "cn=0,1", "et=1", "tp=UDP"];

    /// An `_airplay._tcp`-only device with an AirPlay 2 features bit.
    fn airplay2_only() -> ServiceRecord {
        ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: "Living Room".to_owned(),
            host: "living.local".to_owned(),
            port: 7000,
            txt: vec![
                b"deviceid=02:00:5E:00:53:05".to_vec(),
                b"features=0x0,0x40".to_vec(),
                b"model=AudioAccessory5,1".to_vec(),
            ],
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 21)))],
        }
    }

    /// A receiver event with its strings copied out.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Seen {
        event: i32,
        id: String,
        name: String,
        model: String,
        availability: i32,
        unsupported: i32,
        flags: u32,
    }

    fn text(pointer: *const c_char) -> String {
        assert!(!pointer.is_null());
        // SAFETY: a string the handle owns until the next call on it.
        unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .expect("UTF-8")
            .to_owned()
    }

    fn empty() -> AscAirPlayReceiver {
        AscAirPlayReceiver {
            event: 0,
            id: ptr::null(),
            name: ptr::null(),
            model: ptr::null(),
            availability: -1,
            unsupported: -1,
            flags: 0,
        }
    }

    /// The next `count` receiver events, waiting at most 10 s for them.
    fn next(handle: *mut AscAirPlay, count: usize) -> Vec<Seen> {
        let mut seen = Vec::new();
        let until = Instant::now() + Duration::from_secs(10);
        while seen.len() < count && Instant::now() < until {
            let mut receiver = empty();
            // SAFETY: a live handle, used by this thread only; a local out struct.
            if unsafe { asc_airplay_next_receiver(handle, &mut receiver) } {
                seen.push(Seen {
                    event: receiver.event,
                    id: text(receiver.id),
                    name: text(receiver.name),
                    model: text(receiver.model),
                    availability: receiver.availability,
                    unsupported: receiver.unsupported,
                    flags: receiver.flags,
                });
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        seen
    }

    fn start_with(handle: *mut AscAirPlay, events: Vec<BrowseEvent>) {
        // SAFETY: a live handle, used by this thread only.
        let airplay = unsafe { &*handle };
        airplay
            .sender
            .start_discovery_with(Box::new(Script(events)));
    }

    #[test]
    fn null_handles_and_null_outputs_are_harmless() {
        let mut receiver = empty();
        // SAFETY: null handles and pointers are part of the contract.
        unsafe {
            asc_airplay_destroy(ptr::null_mut());
            let mut none: *mut AscAirPlay = ptr::null_mut();
            asc_airplay_destroy(&mut none);
            assert!(none.is_null());
            assert!(!asc_airplay_start_discovery(ptr::null_mut()));
            asc_airplay_stop_discovery(ptr::null_mut());
            assert!(!asc_airplay_next_receiver(ptr::null_mut(), &mut receiver));
        }
        assert_eq!(receiver.event, 0, "untouched");

        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        // SAFETY: a live handle and a null out pointer.
        assert!(!unsafe { asc_airplay_next_receiver(handle, ptr::null_mut()) });
        // Nothing found without discovery.
        // SAFETY: a live handle and a local out struct.
        assert!(!unsafe { asc_airplay_next_receiver(handle, &mut receiver) });
        assert_eq!(receiver.event, 0, "untouched");
        // SAFETY: destroyed once; the second call sees the nulled pointer.
        unsafe {
            asc_airplay_destroy(&mut handle);
            assert!(handle.is_null());
            asc_airplay_destroy(&mut handle);
        }
    }

    #[test]
    fn discovered_receivers_cross_the_boundary_with_their_verdicts() {
        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        start_with(
            handle,
            vec![
                BrowseEvent::Resolved(raop("02005E005301", "Kitchen", OPEN)),
                BrowseEvent::Resolved(raop("02005E005302", "Den TV", APPLE_TV)),
                BrowseEvent::Resolved(raop("02005E005303", "Office", PASSWORD)),
                BrowseEvent::Resolved(raop("02005E005304", "Old", RSA_ONLY)),
                BrowseEvent::Resolved(airplay2_only()),
            ],
        );
        let mut seen = next(handle, 5);
        seen.sort_by(|a, b| a.id.cmp(&b.id));
        let raop_only = AIRPLAY_FLAG_RAOP;
        assert_eq!(
            seen,
            [
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005301".to_owned(),
                    name: "Kitchen".to_owned(),
                    model: "Speaker1,1".to_owned(),
                    availability: AIRPLAY_AVAILABLE,
                    unsupported: AIRPLAY_REASON_NONE,
                    flags: raop_only,
                },
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005302".to_owned(),
                    name: "Den TV".to_owned(),
                    model: "AppleTV3,2".to_owned(),
                    availability: AIRPLAY_NEEDS_PIN,
                    unsupported: AIRPLAY_REASON_NONE,
                    flags: raop_only
                        | AIRPLAY_FLAG_APPLE_TV
                        | AIRPLAY_FLAG_PAIRING
                        | AIRPLAY_FLAG_LEGACY_PAIRING,
                },
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005303".to_owned(),
                    name: "Office".to_owned(),
                    model: String::new(),
                    availability: AIRPLAY_NEEDS_PASSWORD,
                    unsupported: AIRPLAY_REASON_NONE,
                    flags: raop_only | AIRPLAY_FLAG_PASSWORD,
                },
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005304".to_owned(),
                    name: "Old".to_owned(),
                    model: String::new(),
                    availability: AIRPLAY_UNSUPPORTED,
                    unsupported: AIRPLAY_REASON_ENCRYPTION,
                    flags: raop_only,
                },
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005305".to_owned(),
                    name: "Living Room".to_owned(),
                    model: "AudioAccessory5,1".to_owned(),
                    availability: AIRPLAY_AIRPLAY2_ONLY,
                    unsupported: AIRPLAY_REASON_AIRPLAY2_ONLY,
                    flags: AIRPLAY_FLAG_AIRPLAY | AIRPLAY_FLAG_AIRPLAY2,
                },
            ]
        );
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_receiver_that_leaves_and_every_one_after_a_stop_is_reported_gone() {
        let mut handle = asc_airplay_create();
        start_with(
            handle,
            vec![
                BrowseEvent::Resolved(raop("02005E005311", "Kitchen", OPEN)),
                BrowseEvent::Resolved(raop("02005E005312", "Bedroom", OPEN)),
                BrowseEvent::Removed {
                    kind: ServiceKind::Raop,
                    instance: "02005E005311@Kitchen".to_owned(),
                },
            ],
        );
        let seen = next(handle, 3);
        let events: Vec<(i32, &str, &str)> = seen
            .iter()
            .map(|s| (s.event, s.id.as_str(), s.name.as_str()))
            .collect();
        assert!(
            events.contains(&(AIRPLAY_GONE, "02005E005311", "")),
            "{events:?}"
        );
        let gone = seen.iter().find(|s| s.event == AIRPLAY_GONE).unwrap();
        assert_eq!(
            (
                gone.model.as_str(),
                gone.availability,
                gone.unsupported,
                gone.flags
            ),
            ("", AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE, 0)
        );

        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_stop_discovery(handle) };
        let after = next(handle, 1);
        assert_eq!(
            after
                .iter()
                .map(|s| (s.event, s.id.as_str()))
                .collect::<Vec<_>>(),
            [(AIRPLAY_GONE, "02005E005312")]
        );
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn ids_names_and_strings_follow_the_hosts_conventions() {
        assert_eq!(
            receiver_id(&DeviceKey::Instance(ServiceKind::Raop, "odd".to_owned())),
            "raop:odd"
        );
        assert_eq!(
            receiver_id(&DeviceKey::Instance(
                ServiceKind::AirPlay,
                "Living Room".to_owned()
            )),
            "airplay:Living Room"
        );
        assert_eq!(c_string("a\0b").to_str(), Ok("a\u{FFFD}b"));
    }

    #[test]
    fn live_discovery_starts_or_fails_cleanly_and_destroy_is_prompt() {
        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        // A runner without multicast may refuse; either way the handle stays
        // usable and its destroy returns within the sender's 2 s bound.
        // SAFETY: a live handle, used by this thread only.
        let _ = unsafe { asc_airplay_start_discovery(handle) };
        let _ = next(handle, 0);
        let started = Instant::now();
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(handle.is_null());
    }
}
