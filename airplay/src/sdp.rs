//! The SDP body of ANNOUNCE (P28, P29, P30), per RFC 4566 §5.

use std::net::IpAddr;

/// The audio codec of a session (P74).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// Apple Lossless, the default (P29).
    Alac,
    /// 16-bit big-endian PCM, opt-in (P30).
    Pcm,
}

/// The `a=rtpmap:96` encoding name of a codec.
pub fn encoding(codec: Codec) -> &'static str {
    match codec {
        Codec::Alac => "AppleLossless",
        Codec::Pcm => "L16/44100/2",
    }
}

/// The ALAC configuration of `a=fmtp:96` (P29): 352 frames per packet,
/// compatible version 0, 16 bits, tuning values 40, 10 and 14, 2 channels,
/// maximum run 255, unknown maximum frame size and average bit rate, 44 100
/// Hz.
pub const ALAC_FMTP: &str = "352 0 16 40 10 14 2 255 0 0 44100";

/// The ANNOUNCE body: `v=`, `o=`, `s=`, `c=`, `t=`, `m=`, `a=rtpmap` and, for
/// ALAC, `a=fmtp`, in that order, each line ending in CRLF (P28). The stream
/// isn't encrypted, so there are no key attributes. Addresses are written
/// without an interface scope; `IP6` marks an IPv6 address.
pub fn announce_body(session_id: u32, sender: IpAddr, receiver: IpAddr, codec: Codec) -> String {
    let mut lines = vec![
        "v=0".to_owned(),
        format!("o=AudioSync {session_id} 0 IN {} {sender}", family(sender)),
        "s=AudioSync".to_owned(),
        format!("c=IN {} {receiver}", family(receiver)),
        "t=0 0".to_owned(),
        "m=audio 0 RTP/AVP 96".to_owned(),
        format!("a=rtpmap:96 {}", encoding(codec)),
    ];
    if codec == Codec::Alac {
        lines.push(format!("a=fmtp:96 {ALAC_FMTP}"));
    }
    let mut body = lines.join("\r\n");
    body.push_str("\r\n");
    body
}

fn family(address: IpAddr) -> &'static str {
    match address {
        IpAddr::V4(_) => "IP4",
        IpAddr::V6(_) => "IP6",
    }
}
