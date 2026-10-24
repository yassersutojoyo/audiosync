//! The app's mDNS next to avahi-daemon, ignored by default: the Linux CI
//! job runs avahi-daemon, publishes the services below with
//! `avahi-publish`, and runs this with `cargo test -- --ignored`. Both
//! stacks bind UDP port 5353 (mdns-sd with SO_REUSEADDR and SO_REUSEPORT),
//! and each still has to hear the other.

use std::process::Command;
use std::time::{Duration, Instant};

use audiosync_airplay::dacp::{DacpAdvertiser, DacpId, DacpRecord};
use audiosync_airplay::discovery::mdns::{MdnsAdvertiser, MdnsBrowser};
use audiosync_airplay::discovery::{BrowseEvent, Browser};
use audiosync_cast::discovery::BrowseEvent as CastEvent;
use audiosync_cast::discovery::mdns::MdnsBrowser as CastBrowser;

/// The instances the CI job publishes through avahi.
const RAOP_INSTANCE: &str = "0123456789AB@AudioSyncCI";
const CAST_INSTANCE: &str = "AudioSyncCICast";

const LONG: Duration = Duration::from_secs(30);

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + LONG;
    while Instant::now() < until {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    done()
}

fn run(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The UDP sockets bound to port 5353, from every process.
fn sockets_on_5353() -> Vec<String> {
    run("ss", &["-H", "-u", "-a", "-n", "sport = :5353"])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
#[ignore = "needs the CI job's avahi-daemon and the services it publishes"]
fn mdns_shares_port_5353_with_avahi() {
    let before = sockets_on_5353();
    assert!(!before.is_empty(), "avahi-daemon has no socket on 5353");
    let mut airplay = MdnsBrowser::new().expect("the AirPlay browser binds next to avahi");
    let mut cast = CastBrowser::new().expect("the Cast browser binds next to avahi");
    let after = sockets_on_5353();
    assert!(
        after.len() > before.len(),
        "no socket of ours on 5353: before {before:#?}, after {after:#?}"
    );

    let mut raop = Vec::new();
    let mut casts = Vec::new();
    let heard = wait_until(|| {
        while let Some(event) = airplay.poll_event() {
            if let BrowseEvent::Resolved(record) = event {
                raop.push(record.instance);
            }
        }
        while let Some(event) = cast.poll_event() {
            if let CastEvent::Resolved(record) = event {
                casts.push(record.fullname);
            }
        }
        raop.iter().any(|name| name == RAOP_INSTANCE)
            && casts.iter().any(|name| name.starts_with(CAST_INSTANCE))
    });
    assert!(
        heard,
        "avahi's services not resolved: AirPlay {raop:?}, Cast {casts:?}"
    );

    let mut advertiser = MdnsAdvertiser::new();
    let record = DacpRecord::new(DacpId::new(0xA0B1_C2D3_E4F5_0617).unwrap(), 3689);
    advertiser
        .register(&record)
        .expect("the remote-control service registers next to avahi");
    let mut listed = String::new();
    let seen = wait_until(|| {
        listed = run("avahi-browse", &["-p", "-t", "-k", "_dacp._tcp"]);
        listed.contains(&record.instance)
    });
    advertiser.withdraw();
    assert!(seen, "avahi never saw {}: {listed}", record.instance);
}
