//! The tests' fake Cast device as a program, for checking a real AudioSync
//! against it by hand: `cargo run --example fake_cast -- <listen address>`,
//! then one command per line on stdin: `takeover` (another sender takes
//! the device), `drop` (the connection breaks; the device accepts the
//! reconnect), `log` (message types received so far) or `quit`.
//! Runs inside WSL only: a listener on Windows raises a Firewall prompt.

#[path = "../tests/common/mod.rs"]
mod common;

use std::io::BufRead;
use std::net::IpAddr;

use common::{Behavior, Command, FakeDevice};

fn main() {
    if cfg!(windows) {
        eprintln!("fake_cast runs inside WSL only");
        std::process::exit(2);
    }
    let Some(host) = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<IpAddr>().ok())
    else {
        eprintln!("usage: fake_cast <listen address>");
        std::process::exit(2);
    };
    let device = FakeDevice::start_on(Behavior::default(), host);
    println!("listening on {}", device.address);
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        match line.trim() {
            "takeover" => device.send(Command::TakeOver),
            "drop" => device.send(Command::DropConnection),
            "log" => println!("{:?} fetches {}", device.types(), device.fetches().len()),
            "quit" => break,
            other => println!("unknown command: {other}"),
        }
    }
    device.join();
}
