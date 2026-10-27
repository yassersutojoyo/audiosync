//! `rx_pair`'s receiver devices and its `--auto` mode: the sender built as
//! the apps build it from `AUDIOSYNC_AIRPLAY2_AUTO` and `AUDIOSYNC_AIRPLAY2`
//! (S312), and the per-receiver protocol choice that follows, in words.

use std::net::SocketAddr;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ProtocolPath, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::sender::{SenderConfig, airplay2_auto, airplay2_switch, planned_path};

/// The services a receiver offers the sender: the `_raop._tcp` record, the
/// `_airplay._tcp` record, or both on the same address and port.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Services {
    AirPlay1,
    AirPlay2,
    Both,
}

impl Services {
    pub fn new(path: ProtocolPath, both: bool) -> Services {
        match (both, path) {
            (true, _) => Services::Both,
            (false, ProtocolPath::AirPlay1) => Services::AirPlay1,
            (false, ProtocolPath::AirPlay2) => Services::AirPlay2,
        }
    }
}

/// The flag and the switch as the apps read them, with no saved setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoMode {
    pub auto: bool,
    pub switch: bool,
}

impl AutoMode {
    pub fn from_env(auto: Option<&str>, switch: Option<&str>) -> AutoMode {
        AutoMode {
            auto: airplay2_auto(auto),
            switch: airplay2_switch(switch),
        }
    }

    pub fn config(self) -> SenderConfig {
        SenderConfig::default().with_airplay2_switch(self.auto, self.switch)
    }

    fn state(self) -> &'static str {
        match (self.auto, self.switch) {
            (true, true) => "AirPlay 2 auto on, Prefer AirPlay 2 on",
            (true, false) => "AirPlay 2 auto on",
            (false, true) => "AirPlay 2 auto off, AirPlay 2 switch on",
            (false, false) => "AirPlay 2 auto off",
        }
    }

    /// The protocol the driver plans for `device` before any connection,
    /// e.g. "no AirPlay 1, AirPlay 2 auto off -> not played (AirPlay 2
    /// only)". A refused AirPlay 1 session may still move it to AirPlay 2.
    pub fn choice(self, device: &Device) -> String {
        let config = self.config();
        let services = match (device.raop.is_some(), device.airplay.is_some()) {
            (true, true) => "AirPlay 1 and 2",
            (true, false) => "AirPlay 1 only",
            _ => "no AirPlay 1",
        };
        let outcome = match planned_path(device, config.airplay2, config.airplay2_preferred, None) {
            Ok(ProtocolPath::AirPlay1) => "AirPlay 1".to_owned(),
            Ok(ProtocolPath::AirPlay2) => "AirPlay 2".to_owned(),
            Err(reason) => format!("not played ({reason})"),
        };
        format!("{services}, {} -> {outcome}", self.state())
    }
}

/// The device discovery would have built for a receiver at `address` with
/// `services`.
pub fn device(address: SocketAddr, services: Services, n: u8) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, n]);
    let addresses = vec![ScopedAddr::new(address.ip())];
    let raop = ServiceRecord {
        kind: ServiceKind::Raop,
        instance: format!("{device_id}@Receiver {n}"),
        host: format!("receiver-{n}.local"),
        port: address.port(),
        txt: text(&[
            "txtvers=1",
            "ch=2",
            "cn=0,1",
            "et=0",
            "sr=44100",
            "ss=16",
            "tp=UDP",
            "md=0,1,2",
        ]),
        addresses: addresses.clone(),
    };
    // Bit 41 (0x200 in the high word) lets the driver bind PTP and retry
    // SETUP with it after a 501, as for a receiver that advertises PTP.
    let mut airplay_txt = text(&["features=0x0,0x10240", "flags=0x4", "model=Receiver1,1"]);
    airplay_txt.push(format!("deviceid={device_id}").into_bytes());
    let airplay = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Receiver {n}"),
        host: format!("receiver-{n}.local"),
        port: address.port(),
        txt: airplay_txt,
        addresses,
    };
    let (raop, airplay) = match services {
        Services::AirPlay1 => (Some(&raop), None),
        Services::AirPlay2 => (None, Some(&airplay)),
        Services::Both => (Some(&raop), Some(&airplay)),
    };
    Device::from_records(DeviceKey::Id(device_id), raop, airplay).expect("a device from a record")
}

fn text(entries: &[&str]) -> Vec<Vec<u8>> {
    entries.iter().map(|s| s.as_bytes().to_vec()).collect()
}
