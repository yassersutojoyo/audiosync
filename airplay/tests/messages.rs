//! Message-level checks: RTSP parsing and encoding, the ANNOUNCE body, the
//! digest arithmetic, the volume text and the DMAP layout.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use audiosync_airplay::digest::{self, Challenge, QopAuth};
use audiosync_airplay::discovery::Features;
use audiosync_airplay::metadata::{MetadataSupport, dmap_chunk};
use audiosync_airplay::rtsp::{
    JackKind, JackStatus, Message, Method, ParseError, Parser, Request, TransportPorts,
    audio_latency, public_methods, session_id,
};
use audiosync_airplay::sdp::{ALAC_FMTP, Codec, announce_body};
use audiosync_airplay::volume::{self, Volume};

fn response(bytes: &[u8]) -> audiosync_airplay::rtsp::Response {
    let mut parser = Parser::new();
    parser.push(bytes);
    match parser.next_message() {
        Ok(Some(Message::Response(response))) => response,
        other => panic!("{other:?}"),
    }
}

// covers: P24
#[test]
fn rtsp_response_parsing() {
    // No Content-Length means no body: the next message starts right after.
    let mut parser = Parser::new();
    parser.push(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\nRTSP/1.0 401 Unauthorized\r\ncseq: 2\r\n");
    let Ok(Some(Message::Response(first))) = parser.next_message() else {
        panic!("first response");
    };
    assert_eq!((first.code, first.reason.as_str()), (200, "OK"));
    assert!(first.body.is_empty());
    assert_eq!(parser.next_message(), Ok(None), "the second is incomplete");
    parser.push(b"CONTENT-LENGTH: 4\r\n\r\nab");
    assert_eq!(parser.next_message(), Ok(None), "its body is incomplete");
    parser.push(b"cd");
    let Ok(Some(Message::Response(second))) = parser.next_message() else {
        panic!("second response");
    };
    // Header names compare case-insensitively.
    assert_eq!(second.cseq(), Some(2));
    assert_eq!(second.headers.get("Content-Length"), Some("4"));
    assert_eq!(second.body, b"abcd");
    assert_eq!(parser.buffered(), 0);

    // Bare LF line ends, continuation lines and an empty reason.
    let parsed = response(b"RTSP/1.0 200\nCSeq: 3\nServer: Fake\n  Receiver\n\n");
    assert_eq!(parsed.cseq(), Some(3));
    assert_eq!(parsed.headers.get("server"), Some("Fake Receiver"));
    assert_eq!(parsed.reason, "");

    // Malformed messages.
    for bytes in [
        &b"RTSP/1.0 20 OK\r\n\r\n"[..],
        b"HTTP/1.1 200 OK\r\n\r\n",
        b"RTSP/1.0 200 OK\r\nno colon here\r\n\r\n",
        b"RTSP/1.0 200 OK\r\nContent-Length: four\r\n\r\n",
    ] {
        let mut parser = Parser::new();
        parser.push(bytes);
        assert!(
            parser.next_message().is_err(),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
    let mut parser = Parser::new();
    parser.push(b"PLAY rtsp://192.0.2.10/1 RTSP/1.0\r\n\r\n");
    assert_eq!(parser.next_message(), Err(ParseError::UnknownMethod));

    // Audio-Jack-Status values.
    assert_eq!(
        JackStatus::parse("connected; type=analog"),
        Some(JackStatus {
            connected: true,
            kind: Some(JackKind::Analog)
        })
    );
    assert_eq!(
        JackStatus::parse("connected;type=digital"),
        Some(JackStatus {
            connected: true,
            kind: Some(JackKind::Digital)
        })
    );
    assert_eq!(
        JackStatus::parse("disconnected"),
        Some(JackStatus {
            connected: false,
            kind: None
        })
    );
    assert_eq!(JackStatus::parse("unplugged"), None);
}

#[test]
fn requests_encode_and_parse_back() {
    let mut request = Request::new(Method::SetParameter, "rtsp://192.0.2.10/7");
    request.headers.push("CSeq", "4");
    request.headers.push("Content-Type", "text/parameters");
    request.headers.push("X-Injected", "a\r\nEvil: header");
    request.body = b"volume: -3.000000\r\n".to_vec();
    let bytes = request.encode();
    let text = String::from_utf8(bytes.clone()).expect("UTF-8");
    assert!(text.starts_with("SET_PARAMETER rtsp://192.0.2.10/7 RTSP/1.0\r\nCSeq: 4\r\n"));
    assert!(text.contains("\r\nContent-Length: 19\r\n\r\nvolume"));
    assert!(
        !text.contains("\r\nEvil:"),
        "CR and LF can't end a header value"
    );
    let mut parser = Parser::new();
    parser.push(&bytes);
    let Ok(Some(Message::Request(parsed))) = parser.next_message() else {
        panic!("request");
    };
    assert_eq!(parsed.method, Method::SetParameter);
    assert_eq!(parsed.body, request.body);
    assert_eq!(parsed.headers.get("x-injected"), Some("a  Evil: header"));
}

#[test]
fn header_helpers_read_the_values_the_session_needs() {
    let ports = TransportPorts::parse(
        "RTP/AVP/UDP;unicast;mode=record;server_port=53561;control_port=63379;timing_port=0",
    );
    assert_eq!(ports.server_port, Some(53_561));
    assert_eq!(ports.control_port, Some(63_379));
    assert_eq!(ports.timing_port, Some(0));
    assert_eq!(
        TransportPorts::parse("RTP/AVP/UDP;server_port=x").server_port,
        None
    );
    assert_eq!(
        public_methods("ANNOUNCE, SETUP,RECORD"),
        ["ANNOUNCE", "SETUP", "RECORD"]
    );
    assert_eq!(session_id("1;timeout=60"), Some("1"));
    assert_eq!(session_id(" "), None);
    assert_eq!(audio_latency("3035"), Some(3035));
    assert_eq!(audio_latency("soon"), None);
}

// covers: P28, P29, P30
#[test]
fn rtsp_sdp_pcm_and_alac() {
    let sender = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
    let receiver = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20));
    assert_eq!(
        announce_body(3_141_592_653, sender, receiver, Codec::Alac),
        "v=0\r\n\
         o=AudioSync 3141592653 0 IN IP4 192.0.2.10\r\n\
         s=AudioSync\r\n\
         c=IN IP4 192.0.2.20\r\n\
         t=0 0\r\n\
         m=audio 0 RTP/AVP 96\r\n\
         a=rtpmap:96 AppleLossless\r\n\
         a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n"
    );
    assert_eq!(ALAC_FMTP, "352 0 16 40 10 14 2 255 0 0 44100");
    assert_eq!(
        announce_body(1, sender, receiver, Codec::Pcm),
        "v=0\r\n\
         o=AudioSync 1 0 IN IP4 192.0.2.10\r\n\
         s=AudioSync\r\n\
         c=IN IP4 192.0.2.20\r\n\
         t=0 0\r\n\
         m=audio 0 RTP/AVP 96\r\n\
         a=rtpmap:96 L16/44100/2\r\n"
    );
    let body = announce_body(
        1,
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2)),
        Codec::Alac,
    );
    assert!(body.contains("\r\no=AudioSync 1 0 IN IP6 2001:db8::1\r\n"));
    assert!(body.contains("\r\nc=IN IP6 2001:db8::2\r\n"));
    for key in ["rsaaeskey", "fpaeskey", "aesiv"] {
        assert!(!body.contains(key));
    }
}

// covers: P37
#[test]
fn digest_known_answer() {
    // RFC 2617 §3.5, with qop=auth.
    assert_eq!(
        digest::response(
            "Mufasa",
            "testrealm@host.com",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            Some(QopAuth {
                nc: 1,
                cnonce: "0a4f113b"
            }),
        ),
        "6629fae49393a05397450978507c4ef1"
    );
    assert_eq!(
        digest::md5_hex("Mufasa:testrealm@host.com:Circle Of Life"),
        "939e7578ed9e3c518a452acee763bce9"
    );
    // Without qop: MD5(HA1:nonce:HA2), worked out by hand from RFC 2617.
    let nonce = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
    assert_eq!(
        digest::response(
            "iTunes",
            "raop",
            "hunter2",
            "ANNOUNCE",
            "rtsp://192.0.2.10/3141592653",
            nonce,
            None
        ),
        "449aca5632d2fb732a012afd0a340263"
    );
    // The URI is exactly the request line's: `*` for OPTIONS.
    let challenge =
        Challenge::parse(&format!("Digest realm=\"raop\", nonce=\"{nonce}\"")).expect("challenge");
    assert_eq!(
        digest::authorization(&challenge, "hunter2", Method::Options, "*", 1, "c"),
        format!(
            "Digest username=\"iTunes\", realm=\"raop\", nonce=\"{nonce}\", uri=\"*\", \
             response=\"be4e701be6ac917b5e687870f7e8a556\""
        )
    );
    // With qop and opaque.
    let challenge = Challenge::parse(
        "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", \
         nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"",
    )
    .expect("challenge");
    assert!(challenge.qop_auth);
    assert!(!challenge.stale);
    assert_eq!(
        digest::authorization(
            &challenge,
            "Circle Of Life",
            Method::Get,
            "/dir/index.html",
            1,
            "0a4f113b"
        ),
        concat!(
            "Digest username=\"iTunes\", realm=\"testrealm@host.com\", ",
            "nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", uri=\"/dir/index.html\", ",
            "response=\"82b588a9ff3a7c321f766878d0326a71\", qop=auth, nc=00000001, ",
            "cnonce=\"0a4f113b\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""
        )
    );
}

#[test]
fn digest_challenges_parse_with_quotes_stale_and_algorithm() {
    let stale =
        Challenge::parse("digest REALM=raop, nonce=\"a,b\\\"c\", stale=TRUE").expect("parsed");
    assert_eq!(stale.realm, "raop");
    assert_eq!(stale.nonce, "a,b\"c");
    assert!(stale.stale);
    assert!(Challenge::parse("Digest realm=\"raop\", nonce=\"n\", algorithm=MD5").is_some());
    assert!(Challenge::parse("Digest realm=\"raop\", nonce=\"n\", algorithm=SHA-256").is_none());
    assert!(Challenge::parse("Basic realm=\"raop\"").is_none());
    assert!(
        Challenge::parse("Digest realm=\"raop\"").is_none(),
        "no nonce"
    );
    assert!(
        Challenge::parse("Digest realm=\"raop, nonce=\"n\"").is_none(),
        "open quote"
    );
}

// covers: P117
#[test]
fn volume_text_is_fixed_point_with_six_decimals() {
    assert_eq!(Volume::Db(-11.123877).wire_text(), "-11.123877");
    assert_eq!(Volume::Db(-20.0).wire_text(), "-20.000000");
    assert_eq!(Volume::Db(0.0).wire_text(), "0.000000");
    assert_eq!(Volume::Db(-0.0).wire_text(), "0.000000");
    assert_eq!(Volume::Db(-0.000_000_1).wire_text(), "0.000000");
    assert_eq!(Volume::Db(-1e-5).wire_text(), "-0.000010");
    assert_eq!(Volume::Db(-29.999_999_6).wire_text(), "-30.000000");
    assert_eq!(Volume::Muted.wire_text(), "-144.000000");
    assert_eq!(Volume::Db(-7.25).parameter_body(), "volume: -7.250000\r\n");
    for db in [-30.0, -29.5, -15.123_456, -0.5] {
        let text = Volume::Db(db).wire_text();
        assert!(!text.contains('e') && !text.contains(','), "{text}");
        assert_eq!(text.split('.').nth(1).map(str::len), Some(6), "{text}");
    }
}

// covers: P118
#[test]
fn host_volumes_are_clamped_and_nan_is_ignored() {
    assert_eq!(Volume::from_db(-45.0), Some(Volume::Db(-30.0)));
    assert_eq!(Volume::from_db(6.0), Some(Volume::Db(0.0)));
    assert_eq!(Volume::from_db(f64::NEG_INFINITY), Some(Volume::Db(-30.0)));
    assert_eq!(Volume::from_db(f64::NAN), None);
    assert_eq!(
        Volume::from_db(-144.0),
        Some(Volume::Db(-30.0)),
        "mute is its own value"
    );
    assert_eq!(Volume::Muted.wire_db(), -144.0);
    assert_eq!(volume::parse_reported(b"volume: -11.5\r\n"), Some(-11.5));
    assert_eq!(volume::parse_reported(b"Volume:-144.000000"), Some(-144.0));
    assert_eq!(volume::parse_reported(b"progress: 1/2/3\r\n"), None);
    assert_eq!(volume::parse_reported(b"volume: loud"), None);
}

// covers: P128
#[test]
fn binary_plist_metadata_is_never_offered() {
    let only_bit_50 = Features(1 << Features::BINARY_PLIST_METADATA);
    assert_eq!(
        MetadataSupport::from_features(only_bit_50),
        MetadataSupport::default()
    );
    let text_art_progress = Features((1 << 17) | (1 << 15) | (1 << 16));
    assert_eq!(
        MetadataSupport::from_features(text_art_progress),
        MetadataSupport {
            text: true,
            artwork: true,
            progress: true
        }
    );
    assert_eq!(
        MetadataSupport::from_raop_md(Some(&[0, 2])),
        MetadataSupport {
            text: true,
            artwork: false,
            progress: true
        }
    );
    assert_eq!(
        MetadataSupport::from_raop_md(None),
        MetadataSupport::default()
    );
}

#[test]
fn dmap_chunks_are_code_length_data() {
    assert_eq!(dmap_chunk(b"minm", b"Hi"), b"minm\x00\x00\x00\x02Hi");
    assert_eq!(dmap_chunk(b"asal", b""), b"asal\x00\x00\x00\x00");
    let utf8 = "Café".as_bytes();
    assert_eq!(dmap_chunk(b"asar", utf8)[4..8], [0, 0, 0, 5]);
}
