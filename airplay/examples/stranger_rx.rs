//! A stand-in AirPlay 1 receiver for the stranger workflow
//! (.github/workflows/stranger.yml): the tests' fake receiver
//! ([`NetReceiver`]) on one address, advertised over mDNS as a PCM-only
//! `_raop._tcp` speaker, writing the frames it would play to stdout
//! (16-bit little-endian stereo, 44.1 kHz) when the sender's sync packets
//! say they are heard, mapped onto this machine's clock through the timing
//! exchanges. A frame missing from the RTP sequence is written as silence.
//! Development only; examples never enter a release build.
//!
//! ```text
//! cargo run -p audiosync-airplay --example stranger_rx -- <address> [<name>]
//! ```

#[path = "../tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::io::Write;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use audiosync_airplay::audio::{FRAMES_PER_PACKET, read_pcm_payload};
use audiosync_airplay::clock::{NtpTime, SenderClock};
use common::net::{Behavior, NetReceiver};
use mdns_sd::{ServiceDaemon, ServiceInfo};

const RATE: f64 = 44_100.0;
const HOST_LABEL: &str = "stranger-rx";
const PCM_TXT: &[(&str, &str)] = &[
    ("txtvers", "1"),
    ("ch", "2"),
    ("cn", "0"),
    ("et", "0"),
    ("sr", "44100"),
    ("ss", "16"),
    ("tp", "UDP"),
    ("md", "0,1,2"),
];
/// A gap in the RTP sequence longer than this starts a new timeline instead
/// of being filled with silence.
const LONGEST_FILL: u32 = 44_100;

/// Where the sender's clock stands against this process's.
struct Mapping {
    /// The sender-clock seconds of a timing reply and the local instant it
    /// was taken at (the exchange's midpoint).
    sender_s: f64,
    at: Instant,
    /// The RTP timestamp heard at `heard_s` sender-clock seconds.
    rtp: u32,
    heard_s: f64,
}

impl Mapping {
    fn play_at(&self, rtp: u32) -> Instant {
        let sender = self.heard_s + f64::from(rtp.wrapping_sub(self.rtp) as i32) / RATE;
        let after = sender - self.sender_s;
        let span = Duration::from_secs_f64(after.abs());
        if after >= 0.0 {
            self.at + span
        } else {
            self.at.checked_sub(span).unwrap_or(self.at)
        }
    }
}

fn advertise(ip: IpAddr, port: u16, name: &str) -> Result<ServiceDaemon, mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;
    let info = ServiceInfo::new(
        "_raop._tcp.local.",
        &format!("02005E005399@{name}"),
        &format!("{HOST_LABEL}.local."),
        ip.to_string().as_str(),
        port,
        PCM_TXT,
    )?;
    daemon.register(info)?;
    Ok(daemon)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(ip) = args.first().and_then(|text| text.parse::<IpAddr>().ok()) else {
        eprintln!("usage: stranger_rx <address> [<name>]");
        std::process::exit(2);
    };
    let name = args.get(1).map_or("Stranger Receiver", String::as_str);
    let receiver = NetReceiver::start_at(ip, Behavior::default()).unwrap_or_else(|error| {
        eprintln!("stranger_rx: can't listen on {ip}: {error}");
        std::process::exit(1);
    });
    let _daemon = advertise(ip, receiver.address.port(), name).unwrap_or_else(|error| {
        eprintln!("stranger_rx: mDNS: {error}");
        std::process::exit(1);
    });
    eprintln!("stranger_rx: {name} on {}", receiver.address);

    let clock = SenderClock::new(Instant::now());
    let seconds = |time: NtpTime| clock.seconds_of(time);
    let mut stdout = std::io::stdout().lock();
    let mut read = 0;
    let mut pending: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut next_rtp: Option<u32> = None;
    let mut frames = vec![[0i16; 2]; FRAMES_PER_PACKET];
    loop {
        let mapping = {
            let log = receiver.log();
            for packet in &log.audio[read..] {
                pending.insert(packet.header.timestamp, packet.payload.clone());
            }
            read = log.audio.len();
            match (log.timing.last(), log.syncs.last()) {
                (Some(timing), Some((_, _, sync))) => Some(Mapping {
                    sender_s: seconds(timing.reply.transmit),
                    at: timing.sent_at + (timing.replied_at - timing.sent_at) / 2,
                    rtp: sync.rtp_heard,
                    heard_s: seconds(sync.time),
                }),
                _ => None,
            }
        };
        if let Some(mapping) = mapping {
            let now = Instant::now();
            while let Some((&rtp, _)) = pending.first_key_value() {
                if mapping.play_at(rtp) > now {
                    break;
                }
                let payload = pending
                    .pop_first()
                    .map(|(_, payload)| payload)
                    .unwrap_or_default();
                let mut out = Vec::new();
                if let Some(expected) = next_rtp {
                    let gap = rtp.wrapping_sub(expected);
                    if (1..=LONGEST_FILL).contains(&gap) {
                        out.resize(gap as usize * 4, 0);
                    } else if (LONGEST_FILL + 1..u32::MAX / 2).contains(&gap) {
                        eprintln!("stranger_rx: new timeline at RTP {rtp}");
                    } else if gap != 0 {
                        continue;
                    }
                }
                let count = payload.len() / 4;
                read_pcm_payload(&payload, &mut frames[..count.min(FRAMES_PER_PACKET)]);
                for frame in &frames[..count.min(FRAMES_PER_PACKET)] {
                    out.extend_from_slice(&frame[0].to_le_bytes());
                    out.extend_from_slice(&frame[1].to_le_bytes());
                }
                next_rtp = Some(rtp.wrapping_add(count as u32));
                if stdout
                    .write_all(&out)
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
