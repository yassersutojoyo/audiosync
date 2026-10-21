//! The stream server on 127.0.0.1 (C55 to C64): access, the HTTP
//! response, the WAV format and the real-time pacing.

mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioWriter, StreamFormat, audio_ring};
use audiosync_cast::stream::{
    BlockQueue, ResponseEnd, ServerEvent, StreamConfig, StreamServer, image_size,
};
use common::http;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn server(allowed: Vec<IpAddr>) -> (StreamServer, AudioWriter) {
    let (writer, reader) = audio_ring(48_000);
    let config = StreamConfig {
        local_ip: LOCAL,
        allowed,
        format: StreamFormat::wav(48_000).expect("rate"),
    };
    (StreamServer::start(config, reader).expect("starts"), writer)
}

/// Writes 10 ms of `value` every 10 ms until `stop`.
fn feed(writer: AudioWriter, value: i16, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let block = vec![value; 960];
        let start = Instant::now();
        let mut written = 0u32;
        while !stop.load(Ordering::SeqCst) {
            writer.write_i16(&block);
            written += 1;
            let next = start + Duration::from_millis(10) * written;
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    })
}

fn get(server: &StreamServer, method: &str, extra: &str) -> TcpStream {
    let (address, path) = http::split_url(&server.url()).expect("url");
    http::request(address, &format!("{method} {path} HTTP/1.1\r\nHost: x\r\n{extra}\r\n"))
        .expect("connects")
}

fn events(server: &StreamServer) -> Vec<ServerEvent> {
    std::iter::from_fn(|| server.poll_event()).collect()
}

// covers: C9, C55, C56, C57
#[test]
fn the_url_is_the_local_address_an_ephemeral_port_and_a_fresh_random_token() {
    let (first, _w1) = server(vec![LOCAL]);
    let (second, _w2) = server(vec![LOCAL]);
    let url = first.url();
    let (address, path) = http::split_url(&url).expect("an IP literal and a path");
    assert_eq!(address.ip(), LOCAL, "the channel's local address, no host name");
    assert_eq!(first.address().ip(), LOCAL, "listening on that one address");
    assert_ne!(address.port(), 0);
    assert_eq!(first.token().len(), 32, "128 bits as hex");
    assert!(first.token().chars().all(|c| c.is_ascii_hexdigit()));
    assert!(path.contains(first.token()));
    assert_ne!(first.token(), second.token(), "a new token per session");
    assert_ne!(first.address().port(), second.address().port());

    let (head, _) = http::read_response(
        &mut http::request(address, "GET /not-the-token.wav HTTP/1.1\r\n\r\n").expect("connects"),
        Duration::ZERO,
    );
    assert_eq!(http::status(&head), 404, "another path");
    let wrong = path.replace(first.token(), second.token());
    let (head, _) = http::read_response(
        &mut http::request(address, &format!("GET {wrong} HTTP/1.1\r\n\r\n")).expect("connects"),
        Duration::ZERO,
    );
    assert_eq!(http::status(&head), 404, "another session's token");

    first.stop();
    let after = http::request(address, &format!("GET {path} HTTP/1.1\r\n\r\n"));
    if let Ok(mut stream) = after {
        let (head, _) = http::read_response(&mut stream, Duration::ZERO);
        assert_ne!(http::status(&head), 200, "the token died with the session");
    }
    second.stop();
}

#[test]
fn an_ipv6_url_has_the_address_in_brackets() {
    let (_writer, reader) = audio_ring(4800);
    let config = StreamConfig {
        local_ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
        allowed: vec![IpAddr::V6(Ipv6Addr::LOCALHOST)],
        format: StreamFormat::wav(44_100).expect("rate"),
    };
    // A runner without IPv6 loopback can't run this check.
    let Ok(server) = StreamServer::start(config, reader) else {
        return;
    };
    let port = server.address().port();
    assert!(server.url().starts_with(&format!("http://[::1]:{port}/")));
    server.stop();
}

// covers: C58
#[test]
fn a_request_from_an_address_that_is_not_the_device_gets_403() {
    let (server, _writer) = server(vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))]);
    let mut stream = get(&server, "GET", "");
    let (head, _) = http::read_response(&mut stream, Duration::ZERO);
    assert_eq!(http::status(&head), 403);
    assert!(
        events(&server).contains(&ServerEvent::Refused {
            from: LOCAL,
            status: 403
        })
    );
    server.stop();
}

// covers: C59, C60
#[test]
fn a_get_is_a_chunked_200_with_the_headers_and_range_is_ignored() {
    let (server, writer) = server(vec![LOCAL]);
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(writer, 7, Arc::clone(&stop));
    let mut stream = get(&server, "GET", "Range: bytes=1000-\r\n");
    let (head, body) = http::read_response(&mut stream, Duration::from_millis(300));
    assert_eq!(http::status(&head), 200, "not 206: Range is ignored");
    assert!(head.starts_with("HTTP/1.1 200"));
    assert_eq!(http::header(&head, "Content-Type"), Some("audio/wav"));
    assert_eq!(http::header(&head, "Cache-Control"), Some("no-store"));
    assert_eq!(http::header(&head, "Access-Control-Allow-Origin"), Some("*"));
    assert_eq!(http::header(&head, "Transfer-Encoding"), Some("chunked"));
    assert_eq!(http::header(&head, "Content-Length"), None);
    let chunks = http::chunks(&body);
    assert!(chunks.len() >= 3, "the header chunk and audio");
    assert_eq!(&chunks[0][..4], b"RIFF", "a response starts with the header");

    let mut head_request = get(&server, "HEAD", "");
    let (head, body) = http::read_response(&mut head_request, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    assert_eq!(http::header(&head, "Content-Type"), Some("audio/wav"));
    assert!(body.is_empty(), "HEAD has no body");
    assert!(events(&server).contains(&ServerEvent::Fetch {
        from: LOCAL,
        head: false,
        range: true
    }));
    stop.store(true, Ordering::SeqCst);
    feeder.join().expect("feeder");
    server.stop();
}

// covers: C61
#[test]
fn a_second_get_ends_the_first_and_a_reset_by_the_device_is_no_error() {
    let (server, writer) = server(vec![LOCAL]);
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(writer, 3, Arc::clone(&stop));
    let mut first = get(&server, "GET", "");
    let (_, _) = http::read_response(&mut first, Duration::from_millis(200));
    let mut second = get(&server, "GET", "");
    let (head, body) = http::read_response(&mut second, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    assert_eq!(&http::chunks(&body)[0][..4], b"RIFF", "a fresh header");
    // The first response got its terminating chunk and ends.
    let (_, rest) = http::read_response(&mut first, Duration::from_millis(500));
    let tail = String::from_utf8_lossy(&rest);
    assert!(tail.ends_with("0\r\n\r\n") || rest.is_empty());
    let until = Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    while Instant::now() < until && !seen.contains(&ServerEvent::ResponseEnded(ResponseEnd::Replaced)) {
        seen.extend(events(&server));
        thread::sleep(Duration::from_millis(20));
    }
    assert!(seen.contains(&ServerEvent::ResponseEnded(ResponseEnd::Replaced)));

    drop(second);
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until
        && !seen.contains(&ServerEvent::ResponseEnded(ResponseEnd::ClosedByDevice))
    {
        seen.extend(events(&server));
        thread::sleep(Duration::from_millis(20));
    }
    assert!(seen.contains(&ServerEvent::ResponseEnded(ResponseEnd::ClosedByDevice)));
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, ServerEvent::AcceptFailed(_))),
        "a device closing the stream is not an error: {seen:?}"
    );
    stop.store(true, Ordering::SeqCst);
    feeder.join().expect("feeder");
    server.stop();
}

// covers: C62
#[test]
fn the_wav_header_is_16_bit_stereo_lpcm_with_largest_value_sizes() {
    for rate in [44_100u32, 48_000] {
        let format = StreamFormat::wav(rate).expect("rate");
        assert_eq!(format.content_type(), "audio/wav");
        let header = format.stream_header();
        assert_eq!(header.len(), 44);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[4..8], &u32::MAX.to_le_bytes());
        assert_eq!(&header[8..16], b"WAVEfmt ");
        assert_eq!(&header[16..20], &16u32.to_le_bytes());
        assert_eq!(&header[20..22], &1u16.to_le_bytes(), "LPCM");
        assert_eq!(&header[22..24], &2u16.to_le_bytes(), "two channels");
        assert_eq!(&header[24..28], &rate.to_le_bytes());
        assert_eq!(&header[28..32], &(rate * 4).to_le_bytes());
        assert_eq!(&header[32..34], &4u16.to_le_bytes());
        assert_eq!(&header[34..36], &16u16.to_le_bytes(), "16-bit");
        assert_eq!(&header[36..40], b"data");
        assert_eq!(&header[40..44], &u32::MAX.to_le_bytes());
    }
    assert_eq!(StreamFormat::wav(32_000), None, "only 44 100 and 48 000 Hz");
}

// covers: C64
#[test]
fn audio_leaves_in_20_ms_blocks_at_real_time_starting_at_the_newest() {
    let (server, writer) = server(vec![LOCAL]);
    // Audio written before the GET is old: the response must not carry it.
    writer.write_i16(&vec![111i16; 2 * 9600]);
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(writer, 5, Arc::clone(&stop));
    let start = Instant::now();
    let mut stream = get(&server, "GET", "");
    let (_, body) = http::read_response(&mut stream, Duration::from_millis(600));
    let elapsed = start.elapsed();
    let chunks = http::chunks(&body);
    let audio = &chunks[1..];
    assert!(!audio.is_empty());
    assert!(audio.iter().all(|chunk| chunk.len() == 960 * 4), "20 ms blocks of 48 kHz stereo");
    let frames: usize = audio.iter().map(|chunk| chunk.len() / 4).sum();
    let allowed = (elapsed.as_secs_f64() * 48_000.0) as usize + 960;
    assert!(frames <= allowed, "{frames} frames in {elapsed:?}: faster than real time");
    assert!(
        audio
            .iter()
            .flat_map(|chunk| chunk.chunks_exact(2))
            .all(|pair| i16::from_le_bytes([pair[0], pair[1]]) == 5),
        "the backlog written before the GET was sent"
    );
    stop.store(true, Ordering::SeqCst);
    feeder.join().expect("feeder");
    server.stop();

    // A reader that stalls: the queue holds 2 s and drops the oldest.
    let mut queue = BlockQueue::new(96_000);
    for index in 0..150u8 {
        queue.push(vec![index; 960 * 4]);
    }
    assert_eq!(queue.frames(), 96_000);
    assert_eq!(queue.dropped(), 50 * 960);
    assert_eq!(queue.pop().expect("a block")[0], 50, "the oldest went first");
}

#[test]
fn the_stream_server_writes_the_samples_as_given() {
    let (server, writer) = server(vec![LOCAL]);
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(writer, -12_345, Arc::clone(&stop));
    let mut stream = get(&server, "GET", "");
    let (_, body) = http::read_response(&mut stream, Duration::from_millis(200));
    let chunks = http::chunks(&body);
    assert!(chunks.len() > 1);
    assert!(
        chunks[1..]
            .iter()
            .flat_map(|chunk| chunk.chunks_exact(2))
            .all(|pair| i16::from_le_bytes([pair[0], pair[1]]) == -12_345)
    );
    stop.store(true, Ordering::SeqCst);
    feeder.join().expect("feeder");
    server.stop();
}

#[test]
fn artwork_is_served_under_its_own_random_path_up_to_720p() {
    let (server, _writer) = server(vec![LOCAL]);
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend_from_slice(&1280u32.to_be_bytes());
    png.extend_from_slice(&720u32.to_be_bytes());
    png.extend_from_slice(&[8, 2, 0, 0, 0]);
    assert_eq!(image_size(&png), Some(("image/png", 1280, 720)));
    let url = server.set_artwork(png.clone()).expect("accepted");
    assert!(!url.contains(server.token()), "its own token");
    let (address, path) = http::split_url(&url).expect("url");
    let mut stream =
        http::request(address, &format!("GET {path} HTTP/1.1\r\n\r\n")).expect("connects");
    let (head, body) = http::read_response(&mut stream, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    assert_eq!(http::header(&head, "Content-Type"), Some("image/png"));
    assert_eq!(body, png);

    let mut large = png.clone();
    large[16..20].copy_from_slice(&1920u32.to_be_bytes());
    assert!(server.set_artwork(large).is_err());
    let jpeg = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0,
        0x02, 0x80, 0x03,
    ];
    assert_eq!(image_size(&jpeg), Some(("image/jpeg", 640, 480)));
    assert!(server.set_artwork(b"GIF89a".to_vec()).is_err());
    server.stop();
}

#[test]
fn other_methods_get_405() {
    let (server, _writer) = server(vec![LOCAL]);
    let mut stream = get(&server, "POST", "Content-Length: 0\r\n");
    let (head, _) = http::read_response(&mut stream, Duration::ZERO);
    assert_eq!(http::status(&head), 405);
    server.stop();
}
