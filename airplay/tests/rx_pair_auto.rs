//! `rx_pair --auto`: the sender's configuration from the two environment
//! variables as the apps read them (S312), and the per-receiver choice its
//! summary line states.

#[path = "../examples/rx_pair/auto_choice.rs"]
mod auto_choice;

use std::net::SocketAddr;

use audiosync_airplay::discovery::{Features, ProtocolPath};
use audiosync_airplay::sender::AIRPLAY2_AUTO;

use auto_choice::{AutoMode, Services, device};

fn choice(auto: Option<&str>, switch: Option<&str>, services: Services) -> String {
    let address: SocketAddr = "127.0.0.1:7000".parse().unwrap();
    AutoMode::from_env(auto, switch).choice(&device(address, services, 1))
}

#[test]
fn unset_variables_follow_the_builds_flag_with_the_switch_off() {
    let mode = AutoMode::from_env(None, None);
    assert_eq!(mode.auto, AIRPLAY2_AUTO);
    assert!(!mode.switch);
    let config = mode.config();
    assert_eq!(config.airplay2, AIRPLAY2_AUTO);
    assert!(!config.airplay2_preferred);
    let airplay2_only = choice(None, None, Services::AirPlay2);
    if AIRPLAY2_AUTO {
        assert_eq!(
            airplay2_only,
            "no AirPlay 1, AirPlay 2 auto on -> AirPlay 2"
        );
    } else {
        assert_eq!(
            airplay2_only,
            "no AirPlay 1, AirPlay 2 auto off -> not played (AirPlay 2 only)"
        );
    }
}

#[test]
fn an_airplay2_only_receiver_plays_only_with_the_flag_or_the_switch() {
    assert_eq!(
        choice(Some("0"), None, Services::AirPlay2),
        "no AirPlay 1, AirPlay 2 auto off -> not played (AirPlay 2 only)"
    );
    assert_eq!(
        choice(Some("1"), None, Services::AirPlay2),
        "no AirPlay 1, AirPlay 2 auto on -> AirPlay 2"
    );
    assert_eq!(
        choice(Some("0"), Some("1"), Services::AirPlay2),
        "no AirPlay 1, AirPlay 2 auto off, AirPlay 2 switch on -> AirPlay 2"
    );
    assert_eq!(
        choice(Some("1"), Some("1"), Services::AirPlay2),
        "no AirPlay 1, AirPlay 2 auto on, Prefer AirPlay 2 on -> AirPlay 2"
    );
}

#[test]
fn an_airplay1_only_receiver_stays_on_airplay1_with_the_flag_off() {
    assert_eq!(
        choice(Some("0"), None, Services::AirPlay1),
        "AirPlay 1 only, AirPlay 2 auto off -> AirPlay 1"
    );
    assert_eq!(
        choice(Some("0"), Some("1"), Services::AirPlay1),
        "AirPlay 1 only, AirPlay 2 auto off, AirPlay 2 switch on -> AirPlay 1"
    );
}

#[test]
fn zero_and_other_values_read_as_the_apps_read_them() {
    let zero = AutoMode::from_env(Some("0"), Some("0"));
    assert!(!zero.auto);
    assert!(!zero.switch);
    for garbage in ["yes", "true", "", " 1"] {
        let mode = AutoMode::from_env(Some(garbage), Some(garbage));
        assert_eq!(mode.auto, AIRPLAY2_AUTO, "{garbage:?}");
        assert!(!mode.switch, "{garbage:?}");
    }
    assert_eq!(
        choice(Some("yes"), Some("yes"), Services::AirPlay2),
        choice(None, None, Services::AirPlay2)
    );
}

#[test]
fn a_dual_receiver_stays_on_airplay1_until_the_switch_prefers_airplay2() {
    assert_eq!(
        choice(Some("0"), None, Services::Both),
        "AirPlay 1 and 2, AirPlay 2 auto off -> AirPlay 1"
    );
    assert_eq!(
        choice(Some("1"), None, Services::Both),
        "AirPlay 1 and 2, AirPlay 2 auto on -> AirPlay 1"
    );
    assert_eq!(
        choice(Some("0"), Some("1"), Services::Both),
        "AirPlay 1 and 2, AirPlay 2 auto off, AirPlay 2 switch on -> AirPlay 1"
    );
    assert_eq!(
        choice(Some("1"), Some("1"), Services::Both),
        "AirPlay 1 and 2, AirPlay 2 auto on, Prefer AirPlay 2 on -> AirPlay 2"
    );
    assert_eq!(
        choice(Some("1"), Some("1"), Services::AirPlay1),
        "AirPlay 1 only, AirPlay 2 auto on, Prefer AirPlay 2 on -> AirPlay 1"
    );
}

#[test]
fn an_airplay2_record_offers_ptp_so_a_501_retries_with_it() {
    let address: SocketAddr = "127.0.0.1:7000".parse().unwrap();
    for services in [Services::AirPlay2, Services::Both] {
        assert!(device(address, services, 1).features().has(Features::PTP));
    }
}

#[test]
fn the_rx_options_pick_the_services() {
    assert_eq!(
        Services::new(ProtocolPath::AirPlay1, false),
        Services::AirPlay1
    );
    assert_eq!(
        Services::new(ProtocolPath::AirPlay2, false),
        Services::AirPlay2
    );
    assert_eq!(Services::new(ProtocolPath::AirPlay2, true), Services::Both);
}
