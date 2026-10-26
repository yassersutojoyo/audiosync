//! The identity `fake_tv --deviceid` gives a copy, so two copies are two
//! speakers to a sender.

mod common;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use audiosync_airplay::discovery::DeviceId;
use television::{DEVICE_ID, HOST_LABEL, Pin, parse_device_id};

const OTHER: [u8; 6] = [0x02, 0x00, 0x5E, 0x00, 0x53, 0x56];

#[test]
fn a_colon_form_id_reads_in_either_case() {
    assert_eq!(parse_device_id("02:00:5E:00:53:56"), Ok(OTHER));
    assert_eq!(parse_device_id("02:00:5e:00:53:56"), Ok(OTHER));
}

#[test]
fn a_malformed_id_names_the_expected_form() {
    for bad in [
        "",
        "02:00:5E:00:53",
        "02:00:5E:00:53:56:00",
        "0200:5E:00:53:56",
        "02005E005356",
        "02-00-5E-00-53-56",
        "02:00:5E:00:53:GG",
        " 02:00:5E:00:53:56",
    ] {
        let error = parse_device_id(bad).expect_err(bad);
        assert!(
            error.contains("six colon-separated hex bytes"),
            "{bad:?}: {error}"
        );
    }
}

#[test]
fn the_default_identity_keeps_its_host_and_id() {
    assert_eq!(television::device_id_text(DEVICE_ID), "02:00:5E:00:53:55");
    assert_eq!(television::host_label(DEVICE_ID), HOST_LABEL);
}

#[test]
fn another_id_reaches_the_txt_record_and_its_own_host() {
    let txt = television::txt(Pin::None, 0, OTHER);
    let advertised = txt
        .iter()
        .find(|(key, _)| *key == "deviceid")
        .map(|(_, value)| value.as_str())
        .expect("a deviceid entry");
    assert_eq!(DeviceId::from_colon_form(advertised), Some(DeviceId(OTHER)));
    assert_ne!(television::host_label(OTHER), HOST_LABEL);
}

#[test]
fn another_id_reaches_the_info_answer() {
    let fake = television::television("Second TV", Pin::None, "4821", 0, OTHER);
    let info: plist::Value = plist::Value::from_reader(std::io::Cursor::new(&fake.info_body))
        .expect("the info body is a plist");
    let dict = info.as_dictionary().expect("a dictionary");
    let id = dict.get("deviceID").and_then(plist::Value::as_string);
    assert_eq!(id, Some("02:00:5E:00:53:56"));
}
