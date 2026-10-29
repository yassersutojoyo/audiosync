//! The `fake_tv` command line, in its own file so the flag tests include it.

#![allow(dead_code)]

use crate::television::{self, DEVICE_ID, Pin};

pub struct Options {
    pub name: String,
    pub device_id: [u8; 6],
    pub port: u16,
    pub pin: Pin,
    pub code: String,
    pub on_network: bool,
    pub ptp_only: bool,
    pub summary: bool,
}

pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut name = None;
    let (mut pin_given, mut password_given) = (false, false);
    let mut options = Options {
        name: String::new(),
        device_id: DEVICE_ID,
        port: 0,
        pin: Pin::None,
        code: "4821".to_owned(),
        on_network: false,
        ptp_only: false,
        summary: false,
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--lan" => options.on_network = true,
            "--ptp-only" => options.ptp_only = true,
            "--summary" => options.summary = true,
            "--name" => name = Some(value()?),
            "--deviceid" => options.device_id = television::parse_device_id(&value()?)?,
            "--port" => options.port = value()?.parse().map_err(|_| "bad --port")?,
            "--pin" => {
                pin_given = true;
                options.pin = match value()?.as_str() {
                    "every" => Pin::EveryTime,
                    "once" => Pin::Once,
                    other => return Err(format!("--pin takes every or once, not {other}")),
                }
            }
            "--code" => {
                let code = value()?;
                if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("--code takes digits".to_owned());
                }
                pin_given = true;
                options.code = code;
            }
            "--password" => {
                let password = value()?;
                if password.is_empty() {
                    return Err("--password takes a non-empty text".to_owned());
                }
                password_given = true;
                options.pin = Pin::Password;
                options.code = password;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if pin_given && password_given {
        return Err("--password excludes --pin and --code".to_owned());
    }
    options.name = name.unwrap_or_else(|| television::default_name(options.device_id));
    Ok(options)
}
