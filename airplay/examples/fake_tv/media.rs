//! The audio ports a measurement reads: the first packet on each, with the
//! receiver's clock when it arrived, so the RTP time can be set against an
//! anchor (a sync packet or SETRATEANCHORTIME) and the arrival.

use std::io::{ErrorKind, Read};
use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Instant, SystemTime};

use crate::dump::unix;
use crate::realtime::{Declared, Loss};
use crate::summary;

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn arrival(start: Instant) -> String {
    format!(
        "{} first packet at unix {:.6}",
        summary::stamp(start.elapsed().as_secs_f64()),
        unix(SystemTime::now())
    )
}

/// An RTP header's sequence and timestamp (RFC 3550).
fn rtp(packet: &[u8]) -> String {
    match (u16_at(packet, 2), u32_at(packet, 4)) {
        (Some(seq), Some(timestamp)) => format!("rtp seq {seq} timestamp {timestamp}"),
        _ => "too short for an RTP header".to_owned(),
    }
}

/// A sync packet's fields (P93): bytes 16 to 19 minus 4 to 7 is the
/// latency the sender asks for, in frames.
fn sync(packet: &[u8]) -> String {
    let (Some(now), Some(next)) = (u32_at(packet, 4), u32_at(packet, 16)) else {
        return "too short for a sync packet".to_owned();
    };
    let hex: String = packet.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "type {:#04x}, rtp {now}, next rtp {next}, next minus rtp {} frames, bytes {hex}",
        packet[1],
        next.wrapping_sub(now)
    )
}

/// Reads `socket` for good: the audio on the data port, which `loss` may
/// drop, or the control port, whose sync packets show the latency the
/// sender declares. With `first`, it logs the first datagram.
pub fn watch_udp(socket: UdpSocket, control: bool, start: Instant, first: bool, loss: Arc<Loss>) {
    thread::spawn(move || {
        let mut buffer = [0u8; 2048];
        let mut first = first;
        let mut declared = Declared::default();
        loop {
            let (count, from) = match socket.recv_from(&mut buffer) {
                Ok(datagram) => datagram,
                // On Windows a resend request to a closed port makes the next
                // receive fail with ConnectionReset; the socket still works.
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionReset | ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(_) => break,
            };
            let packet = &buffer[..count];
            if control {
                let seconds = start.elapsed().as_secs_f64();
                let lines = [
                    loss.on_control(packet, from),
                    declared.sync(packet, seconds),
                ];
                for line in lines.into_iter().flatten() {
                    println!(
                        "{} {line}, unix {:.6}",
                        summary::stamp(seconds),
                        unix(SystemTime::now())
                    );
                }
            } else {
                loss.on_data(packet);
            }
            if first {
                first = false;
                let (port, fields) = if control {
                    ("control", sync(packet))
                } else {
                    ("data", rtp(packet))
                };
                println!(
                    "{} on the UDP {port} port, {count} bytes: {fields}",
                    arrival(start)
                );
            }
        }
    });
}

/// Takes buffered-audio connections (P168) and logs each one's first
/// packet. It assumes a 2-byte length before each packet's RTP header, which
/// the spec does not state yet, so the log is a hint, not a check.
pub fn watch_tcp(listener: TcpListener, start: Instant) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut buffer = [0u8; 16 * 1024];
                let mut first = true;
                while let Ok(count) = stream.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    if first {
                        first = false;
                        let length = u16_at(&buffer[..count], 0).unwrap_or_default();
                        println!(
                            "{} on the TCP buffered port, length {length}: {}",
                            arrival(start),
                            rtp(buffer.get(2..count).unwrap_or_default())
                        );
                    }
                }
            });
        }
    });
}
