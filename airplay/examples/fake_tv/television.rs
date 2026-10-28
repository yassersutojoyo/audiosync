//! The fake television `fake_tv` serves and `fake_tv_cells` drives: the
//! test fake's Samsung profile with the identity and lock mode it
//! advertises. Each example uses part of it.

#![allow(dead_code)]

use audiosync_airplay::discovery::DeviceId;
use plist::Value;

use crate::common::ap2::{FakeAp2, dict, int, plist, read_dict};

/// Bits 38 (CoreUtils pairing) and 48 (transient pairing): AirPlay 2 capable.
pub const FEATURES_HIGH: u64 = 0x1_0040;
/// Bits 15 to 17 (artwork, progress, text): track info reaches it. The
/// traced television sets none, so only the cells profile has them.
pub const FEATURES_METADATA: u64 = 0x3_8000;
/// The label of its own mDNS host name, in the `.local.` domain.
pub const HOST_LABEL: &str = "audiosync-test-tv";
pub const NAME: &str = "AudioSync Test TV";
pub const MODEL: &str = "Speaker1,1";
/// A locally administered address in the documentation block of RFC 7042.
pub const DEVICE_ID: [u8; 6] = [0x02, 0x00, 0x5E, 0x00, 0x53, 0x55];
/// The environment variable that sets the output latency it reports, in
/// microseconds.
pub const LATENCY_VAR: &str = "FAKE_TV_LATENCY_US";
/// Status flags as the television sends them without a PIN (bit 2).
const FLAGS_PLAIN: u64 = 0x4;
/// Bit 3: a PIN on every connection, and every pair-verify turned down.
const FLAGS_PIN_EVERY_TIME: u64 = 0x4c;
/// Bit 9: a PIN the first time only.
const FLAGS_PIN_ONCE: u64 = 0x244;
/// Bit 7: a password, kept as a pairing after the first connection. A
/// password receiver adds `pw` to its `_raop._tcp` TXT only, which this
/// AirPlay 2 only television does not advertise.
const FLAGS_PASSWORD: u64 = 0x84;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    None,
    EveryTime,
    Once,
    /// The code is a password: the SRP code of a pairing that lasts.
    Password,
}

pub fn status_flags(pin: Pin) -> u64 {
    match pin {
        Pin::None => FLAGS_PLAIN,
        Pin::EveryTime => FLAGS_PIN_EVERY_TIME,
        Pin::Once => FLAGS_PIN_ONCE,
        Pin::Password => FLAGS_PASSWORD,
    }
}

pub fn device_id_text(id: [u8; 6]) -> String {
    id.iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// `base` for the default identity, so the runbooks still match; another
/// one gets its id as a suffix, so two copies never share a record.
fn with_id(base: &str, id: [u8; 6]) -> String {
    if id == DEVICE_ID {
        return base.to_owned();
    }
    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{base}-{hex}")
}

/// Its mDNS host label.
pub fn host_label(id: [u8; 6]) -> String {
    with_id(HOST_LABEL, id)
}

/// The name it serves and advertises when `--name` gives none.
pub fn default_name(id: [u8; 6]) -> String {
    with_id(NAME, id)
}

/// Reads `--deviceid`: six colon-separated hex bytes, the form the
/// `deviceid` TXT entry carries.
pub fn parse_device_id(text: &str) -> Result<[u8; 6], String> {
    DeviceId::from_colon_form(text)
        .filter(|_| text.trim() == text)
        .map(|id| id.0)
        .ok_or_else(|| {
            format!("--deviceid takes six colon-separated hex bytes such as 02:00:5E:00:53:56, not {text:?}")
        })
}

/// The `_airplay._tcp` TXT entries it advertises, with `features_low` as
/// the low word of its features.
pub fn txt(pin: Pin, features_low: u64, id: [u8; 6]) -> Vec<(&'static str, String)> {
    vec![
        ("features", format!("{features_low:#x},{FEATURES_HIGH:#x}")),
        ("flags", format!("{:#x}", status_flags(pin))),
        ("model", MODEL.to_owned()),
        ("deviceid", device_id_text(id)),
    ]
}

pub fn television(name: &str, pin: Pin, code: &str, features_low: u64, id: [u8; 6]) -> FakeAp2 {
    let mut fake = FakeAp2::samsung();
    if pin != Pin::None {
        fake.hap.code = code.to_owned();
        fake.hap.pairing_per_connection = pin == Pin::EveryTime;
    }
    fake.info_body = plist(dict(&[
        (
            "features",
            int(((FEATURES_HIGH << 32) | features_low) as i64),
        ),
        ("statusFlags", int(status_flags(pin) as i64)),
        ("name", Value::String(name.to_owned())),
        ("model", Value::String(MODEL.to_owned())),
        ("deviceID", Value::String(device_id_text(id))),
    ]));
    fake
}

/// Reads [`LATENCY_VAR`]: unset or empty reports nothing, as before it
/// existed; otherwise whole microseconds.
pub fn latency_from(value: Option<&str>) -> Result<Option<u64>, String> {
    match value {
        None | Some("") => Ok(None),
        Some(text) => text
            .parse()
            .ok()
            .filter(|_| text.bytes().all(|b| b.is_ascii_digit()))
            .map(Some)
            .ok_or_else(|| {
                format!("{LATENCY_VAR} takes whole microseconds such as 250000, not {text:?}")
            }),
    }
}

/// Adds `micros` to its `/info` answer as the `outputLatencyMicros` of
/// `audioLatencies`, which the sender reads as R_s (P197).
pub fn report_latency(fake: &mut FakeAp2, micros: u64) {
    let mut info = read_dict(&fake.info_body).expect("the television's /info is a dictionary");
    let entry = dict(&[
        ("type", int(100)),
        ("audioType", Value::String("default".to_owned())),
        ("inputLatencyMicros", int(0)),
        ("outputLatencyMicros", Value::Integer(micros.into())),
    ]);
    info.insert("audioLatencies".to_owned(), Value::Array(vec![entry]));
    fake.info_body = plist(Value::Dictionary(info));
}
