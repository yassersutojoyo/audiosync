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
    BlockQueue, FetchHeaders, MAX_LOGGED_NAMES, MAX_LOGGED_TEXT, ResponseEnd, ServerEvent,
    StreamConfig, StreamServer, image_size,
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
    http::request(
        address,
        &format!("{method} {path} HTTP/1.1\r\nHost: x\r\n{extra}\r\n"),
    )
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
    assert_eq!(
        address.ip(),
        LOCAL,
        "the channel's local address, no host name"
    );
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
    assert_eq!(http::status(&head), 403, "{head}");
    assert!(events(&server).contains(&ServerEvent::Refused {
        from: LOCAL,
        status: 403
    }));
    server.stop();
}

// covers: C59, C60, C62
#[test]
fn a_get_is_a_chunked_200_with_the_headers_and_range_is_ignored() {
    let (server, writer) = server(vec![LOCAL]);
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(writer, 7, Arc::clone(&stop));
    let mut stream = get(&server, "GET", "Range: bytes=1000-\r\n");
    // 300 ms was too short for three chunks on a busy macOS runner (CI 39722270459).
    let (head, body) = http::read_response(&mut stream, Duration::from_millis(1000));
    assert_eq!(http::status(&head), 200, "not 206: Range is ignored");
    assert!(head.starts_with("HTTP/1.1 200"));
    assert_eq!(http::header(&head, "Content-Type"), Some("audio/wav"));
    assert_eq!(http::header(&head, "Cache-Control"), Some("no-store"));
    assert_eq!(
        http::header(&head, "Access-Control-Allow-Origin"),
        Some("*")
    );
    assert_eq!(http::header(&head, "Transfer-Encoding"), Some("chunked"));
    assert_eq!(http::header(&head, "Content-Length"), None);
    let chunks = http::chunks(&body);
    assert!(chunks.len() >= 3, "the header chunk and audio");
    assert_eq!(
        &chunks[0][..4],
        b"RIFF",
        "a response starts with the header"
    );

    let mut head_request = get(&server, "HEAD", "");
    let (head, body) = http::read_response(&mut head_request, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    assert_eq!(http::header(&head, "Content-Type"), Some("audio/wav"));
    assert!(body.is_empty(), "HEAD has no body");
    let fetched = events(&server).into_iter().find_map(|event| match event {
        ServerEvent::Fetch {
            from,
            head,
            headers,
        } => Some((from, head, headers.range)),
        _ => None,
    });
    assert_eq!(
        fetched,
        Some((LOCAL, false, Some("bytes=1000-".to_owned())))
    );
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
    while Instant::now() < until
        && !seen.contains(&ServerEvent::ResponseEnded(ResponseEnd::Replaced))
    {
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

// The WAV half of C63; FLAC is pending.
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
    assert!(
        audio.iter().all(|chunk| chunk.len() == 960 * 4),
        "20 ms blocks of 48 kHz stereo"
    );
    let frames: usize = audio.iter().map(|chunk| chunk.len() / 4).sum();
    let allowed = (elapsed.as_secs_f64() * 48_000.0) as usize + 960;
    assert!(
        frames <= allowed,
        "{frames} frames in {elapsed:?}: faster than real time"
    );
    let samples: Vec<i16> = audio
        .iter()
        .flat_map(|chunk| chunk.as_chunks::<2>().0.iter())
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    assert!(
        samples.iter().all(|sample| matches!(sample, 5 | 0)),
        "the backlog written before the GET was sent"
    );
    assert!(samples.contains(&5), "only silence was sent");
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
    assert_eq!(
        queue.pop().expect("a block")[0],
        50,
        "the oldest went first"
    );
}

// covers: C64
#[test]
fn a_device_that_stops_reading_fills_a_2_s_queue_and_drops_the_oldest() {
    let (server, writer) = server(vec![LOCAL]);
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(writer, 1, Arc::clone(&stop));
    let mut stream = get(&server, "GET", "");
    let (head, _) = http::read_response(&mut stream, Duration::ZERO);
    assert_eq!(http::status(&head), 200);
    // The socket buffers fill first; then the queue, then drops.
    let until = Instant::now() + Duration::from_secs(30);
    let mut most_queued = 0;
    let mut stats = server.stats();
    while Instant::now() < until && stats.frames_dropped == 0 {
        thread::sleep(Duration::from_millis(50));
        stats = server.stats();
        assert!(stats.frames_queued <= 96_000, "{stats:?}");
        most_queued = most_queued.max(stats.frames_queued);
    }
    assert!(stats.frames_dropped > 0, "{stats:?}");
    assert!(stats.streaming, "a slow reader is not a closed one");
    assert!(most_queued >= 96_000 - 2 * 960, "{most_queued}");
    stop.store(true, Ordering::SeqCst);
    feeder.join().expect("feeder");
    let started = Instant::now();
    server.stop();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stop waits for no stalled reader"
    );
    drop(stream);
}

/// Whether the server closed `stream` within `limit`.
fn closed_within(stream: &mut TcpStream, limit: Duration) -> bool {
    use std::io::Read;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let until = Instant::now() + limit;
    let mut buffer = [0u8; 64];
    while Instant::now() < until {
        match stream.read(&mut buffer) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return true,
        }
    }
    false
}

#[test]
fn idle_and_trickling_connections_cannot_pin_the_server() {
    use std::io::Write;
    let (server, _writer) = server(vec![LOCAL]);
    let mut idle: Vec<TcpStream> = (0..4)
        .map(|_| TcpStream::connect(server.address()).expect("connects"))
        .collect();
    thread::sleep(Duration::from_millis(200));
    let (address, path) = http::split_url(&server.url()).expect("url");
    let mut extra =
        http::request(address, &format!("GET {path} HTTP/1.1\r\n\r\n")).expect("connects");
    let (head, _) = http::read_response(&mut extra, Duration::ZERO);
    assert_eq!(http::status(&head), 503, "a fifth connection is refused");
    assert!(
        closed_within(&mut extra, Duration::from_secs(2)),
        "and closed"
    );
    assert!(events(&server).contains(&ServerEvent::Refused {
        from: LOCAL,
        status: 503
    }));
    // One byte a second never completes a request: one deadline for all.
    let started = Instant::now();
    let mut trickle = idle.remove(0);
    let mut closed = false;
    while started.elapsed() < Duration::from_secs(9) && !closed {
        let _ = trickle.write_all(b"G");
        closed = closed_within(&mut trickle, Duration::from_secs(1));
    }
    assert!(closed, "the trickling request was never cut off");
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "{:?}",
        started.elapsed()
    );
    server.stop();
}

#[test]
fn the_stream_server_writes_the_samples_as_given() {
    let (server, writer) = server(vec![LOCAL]);
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(writer, -12_345, Arc::clone(&stop));
    let mut stream = get(&server, "GET", "");
    // A loaded CI runner can take longer than 200 ms to send the first block.
    let (_, body) = http::read_response(&mut stream, Duration::from_millis(1000));
    let chunks = http::chunks(&body);
    assert!(chunks.len() > 1, "{} chunks", chunks.len());
    let samples: Vec<i16> = chunks[1..]
        .iter()
        .flat_map(|chunk| chunk.as_chunks::<2>().0.iter())
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    assert!(samples.iter().all(|sample| matches!(sample, -12_345 | 0)));
    assert!(samples.contains(&-12_345), "only silence was sent");
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

/// Opens a stream response and waits for its first block: the pump has
/// then taken the response in, so what the ring holds next is kept.
fn streaming(server: &StreamServer) -> TcpStream {
    let mut stream = get(server, "GET", "");
    let (head, _) = http::read_response(&mut stream, Duration::ZERO);
    assert_eq!(http::status(&head), 200);
    let until = Instant::now() + Duration::from_secs(5);
    while server.stats().frames_sent == 0 {
        assert!(Instant::now() < until, "never streamed");
        thread::sleep(Duration::from_millis(5));
    }
    stream
}

/// Polls the stats until `frames_dropped` reaches `at_least`, for 1.5 s:
/// less than the 2 s queue, so a response nobody reads drops nothing yet.
fn dropped_reaches(server: &StreamServer, at_least: u64) -> u64 {
    let until = Instant::now() + Duration::from_millis(1500);
    loop {
        let dropped = server.stats().frames_dropped;
        if dropped >= at_least || Instant::now() >= until {
            return dropped;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

// covers: C52
#[test]
fn a_silent_input_keeps_the_response_open_with_silence() {
    let (server, writer) = server(vec![LOCAL]);
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = feed(writer, 7, Arc::clone(&stop));
    let mut stream = get(&server, "GET", "");
    let stopper = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            stop.store(true, Ordering::SeqCst);
        })
    };
    let (_, body) = http::read_response(&mut stream, Duration::from_millis(900));
    stopper.join().expect("stopper");
    feeder.join().expect("feeder");
    let chunks = http::chunks(&body);
    let audio = &chunks[1..];
    assert!(audio.len() > 10, "{} chunks", audio.len());
    assert!(
        audio.iter().all(|chunk| !chunk.is_empty()),
        "the response ended"
    );
    assert!(audio.iter().flatten().any(|byte| *byte != 0), "the input");
    assert!(
        audio[audio.len() - 5..]
            .iter()
            .all(|chunk| chunk.len() == 960 * 4 && chunk.iter().all(|byte| *byte == 0)),
        "silence after the input stopped"
    );
    assert!(server.stats().streaming);
    server.stop();
}

// covers: C64
#[test]
fn a_backlog_in_the_ring_past_100_ms_is_dropped_and_counted() {
    let (server, writer) = server(vec![LOCAL]);
    let _stream = streaming(&server);
    writer.write_i16(&vec![3i16; 2 * 24_000]);
    assert!(dropped_reaches(&server, 1) > 0, "{:?}", server.stats());
    server.stop();
}

// covers: C79
#[test]
fn frames_the_full_ring_refused_count_as_dropped() {
    let (server, writer) = server(vec![LOCAL]);
    let _stream = streaming(&server);
    // 2 s into a 1 s ring: 48 000 frames never get in.
    writer.write_i16(&vec![3i16; 2 * 96_000]);
    let dropped = dropped_reaches(&server, 60_000);
    assert!(
        dropped >= 60_000,
        "{dropped}: the ring's overflow is missing"
    );
    server.stop();
}

// The G6 hardware check reads these log lines.
#[test]
fn a_fetch_logs_its_header_names_and_three_values_capped_and_never_the_token() {
    let (server, _writer) = server(vec![LOCAL]);
    let token = server.token().to_owned();
    let user_agent = format!("CrKey/1.56 {token} {}", "x".repeat(300));
    let mut extra = format!(
        "User-Agent: {user_agent}\r\nIcy-MetaData: 1\r\nReferer: {}\r\nX-{token}: secret-value\r\n",
        server.url()
    );
    for index in 0..40 {
        extra.push_str(&format!("X-Filler-{index}: secret-value\r\n"));
    }
    let mut stream = get(&server, "HEAD", &extra);
    let (head, _) = http::read_response(&mut stream, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);

    let seen = events(&server);
    let headers: &FetchHeaders = seen
        .iter()
        .find_map(|event| match event {
            ServerEvent::Fetch { headers, .. } => Some(headers),
            _ => None,
        })
        .expect("a fetch event");
    assert_eq!(headers.names.len(), MAX_LOGGED_NAMES);
    assert_eq!(
        headers.more,
        45 - MAX_LOGGED_NAMES,
        "Host, 4 headers and 40 fillers"
    );
    assert_eq!(
        headers.names[..4],
        ["Host", "User-Agent", "Icy-MetaData", "Referer"]
    );
    assert_eq!(headers.names[4], "X-<token>");
    assert_eq!(headers.icy_metadata.as_deref(), Some("1"));
    assert_eq!(headers.range, None);
    let logged_agent = headers.user_agent.as_deref().expect("the user agent");
    assert!(
        logged_agent.starts_with("CrKey/1.56 <token> x"),
        "{logged_agent}"
    );
    assert_eq!(logged_agent.chars().count(), MAX_LOGGED_TEXT);

    let log = format!("{seen:?}");
    assert!(!log.contains(&token), "the token reached the log: {log}");
    assert!(!log.contains("secret-value"), "only three values are kept");
    assert!(!log.contains("http://"), "the Referer value is not kept");
    server.stop();
}

/// The logged headers of one `HEAD` fetch carrying `extra`.
fn logged_headers(server: &StreamServer, extra: &str) -> FetchHeaders {
    let mut stream = get(server, "HEAD", extra);
    let (head, _) = http::read_response(&mut stream, Duration::from_millis(200));
    assert_eq!(http::status(&head), 200);
    events(server)
        .into_iter()
        .find_map(|event| match event {
            ServerEvent::Fetch { headers, .. } => Some(headers),
            _ => None,
        })
        .expect("a fetch event")
}

// The G6 hardware check reads these log lines.
#[test]
fn a_token_split_across_header_names_is_redacted() {
    let (server, _writer) = server(vec![LOCAL]);
    let token = server.token().to_owned();
    let (first, second) = token.split_at(token.len() / 2);
    let headers = logged_headers(
        &server,
        &format!("X-{first}: 1\r\nX-{second}: 1\r\nX-Request-1234567: 1\r\n"),
    );
    assert_eq!(
        headers.names,
        ["Host", "<redacted>", "<redacted>", "X-Request-1234567"],
        "seven hex digits in a row stay"
    );
    server.stop();
}

// The G6 hardware check reads these log lines.
#[test]
fn an_uppercase_token_is_scrubbed() {
    let (server, _writer) = server(vec![LOCAL]);
    let token = server.token().to_owned();
    let upper = token.to_ascii_uppercase();
    let headers = logged_headers(
        &server,
        &format!("X-{upper}: 1\r\nUser-Agent: CrKey {upper}\r\n"),
    );
    assert_eq!(headers.names[1], "X-<token>");
    assert_eq!(headers.user_agent.as_deref(), Some("CrKey <token>"));
    assert!(!format!("{headers:?}").to_ascii_lowercase().contains(&token));
    server.stop();
}

// The G6 hardware check reads these log lines.
#[test]
fn a_percent_encoded_token_is_scrubbed() {
    let (server, _writer) = server(vec![LOCAL]);
    let token = server.token().to_owned();
    let once: String = token.bytes().map(|byte| format!("%{byte:02X}")).collect();
    let twice = once.replace('%', "%25");
    let headers = logged_headers(
        &server,
        &format!("X-{once}: 1\r\nUser-Agent: CrKey {twice}\r\nRange: bytes=0-\r\n"),
    );
    assert_eq!(headers.names[1], "X-<token>");
    assert_eq!(headers.user_agent.as_deref(), Some("CrKey <token>"));
    assert_eq!(headers.range.as_deref(), Some("bytes=0-"));
    server.stop();
}

// The G6 hardware check reads these log lines.
#[test]
fn a_decimal_byte_range_is_logged_verbatim_and_a_range_holding_the_token_is_not() {
    let (server, _writer) = server(vec![LOCAL]);
    let token = server.token().to_owned();
    let (first, _) = token.split_at(token.len() / 2);
    let cases = [
        ("bytes=123456789-".to_owned(), "bytes=123456789-".to_owned()),
        (
            "bytes=123456789-123456799, 5-".to_owned(),
            "bytes=123456789-123456799, 5-".to_owned(),
        ),
        (format!("bytes={token}-"), "bytes=<token>-".to_owned()),
        // The trailing letters keep a chunk that happens to be all decimal
        // from passing as an offset.
        (format!("bytes=0-1,{first}ab-"), "<redacted>".to_owned()),
        (format!("{first}=0-"), "<redacted>".to_owned()),
    ];
    for (sent, logged) in cases {
        let headers = logged_headers(&server, &format!("Range: {sent}\r\n"));
        assert_eq!(headers.range.as_deref(), Some(logged.as_str()), "{sent}");
    }
    server.stop();
}
