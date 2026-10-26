//! The fake television `fake_tv` serves and `fake_tv_cells` drives: the
//! test fake's Samsung profile with the identity and PIN mode it
//! advertises. Each example uses part of it.

#![allow(dead_code)]

use plist::Value;

use crate::common::ap2::{FakeAp2, dict, int, plist};

/// Bits 38 (CoreUtils pairing) and 48 (transient pairing): AirPlay 2 capable.
pub const FEATURES_HIGH: u64 = 0x1_0040;
/// The label of its own mDNS host name, in the `.local.` domain.
pub const HOST_LABEL: &str = "audiosync-test-tv";
pub const MODEL: &str = "Speaker1,1";
/// A locally administered address in the documentation block of RFC 7042.
pub const DEVICE_ID: [u8; 6] = [0x02, 0x00, 0x5E, 0x00, 0x53, 0x55];
/// Status flags as the television sends them without a PIN (bit 2).
const FLAGS_PLAIN: u64 = 0x4;
/// Bit 3: a PIN on every connection, and every pair-verify turned down.
const FLAGS_PIN_EVERY_TIME: u64 = 0x4c;
/// Bit 9: a PIN the first time only.
const FLAGS_PIN_ONCE: u64 = 0x244;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    None,
    EveryTime,
    Once,
}

pub fn status_flags(pin: Pin) -> u64 {
    match pin {
        Pin::None => FLAGS_PLAIN,
        Pin::EveryTime => FLAGS_PIN_EVERY_TIME,
        Pin::Once => FLAGS_PIN_ONCE,
    }
}

pub fn device_id_text() -> String {
    DEVICE_ID
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The `_airplay._tcp` TXT entries it advertises.
pub fn txt(pin: Pin) -> Vec<(&'static str, String)> {
    vec![
        ("features", format!("0x0,{FEATURES_HIGH:#x}")),
        ("flags", format!("{:#x}", status_flags(pin))),
        ("model", MODEL.to_owned()),
        ("deviceid", device_id_text()),
    ]
}

pub fn television(name: &str, pin: Pin, code: &str) -> FakeAp2 {
    let mut fake = FakeAp2::samsung();
    if pin != Pin::None {
        fake.hap.code = code.to_owned();
        fake.hap.pairing_per_connection = pin == Pin::EveryTime;
    }
    fake.info_body = plist(dict(&[
        ("features", int((FEATURES_HIGH << 32) as i64)),
        ("statusFlags", int(status_flags(pin) as i64)),
        ("name", Value::String(name.to_owned())),
        ("model", Value::String(MODEL.to_owned())),
        ("deviceID", Value::String(device_id_text())),
    ]));
    fake
}
