//! The lines `fake_tv --summary` prints for the events of the dry-run cells.

mod common;
#[path = "../examples/fake_tv/summary.rs"]
mod summary;

use audiosync_airplay::rtsp::{Method, Request};
use common::ap2::{dict, plist};
use common::hap::tlv;
use plist::Value;
use summary::{ECHO_WINDOW_SECS, Summary};

fn request(method: Method, uri: &str, body: &[u8]) -> Request {
    let mut request = Request::new(method, uri);
    request.body = body.to_vec();
    request
}

fn pair_setup(state: u8, transient: bool) -> Request {
    let body = if transient {
        tlv(&[(6, &[state]), (0, &[0]), (19, &[16])])
    } else {
        tlv(&[(6, &[state]), (0, &[0])])
    };
    request(Method::Post, "/pair-setup", &body)
}

fn volume(db: f64) -> Request {
    let body = format!("volume: {db:.6}\r\n");
    request(Method::SetParameter, "rtsp://tv/1", body.as_bytes())
}

#[test]
fn pairing_counts_starts_and_completions() {
    let mut summary = Summary::default();
    let lines: Vec<Option<String>> = vec![
        summary.request(1.0, &pair_setup(1, false), 200),
        summary.request(2.0, &pair_setup(3, false), 200),
        summary.request(3.0, &pair_setup(5, false), 200),
        summary.request(9.0, &pair_setup(1, false), 200),
        summary.request(10.0, &pair_setup(1, true), 200),
    ];
    assert_eq!(
        lines,
        vec![
            Some("[    1.000] PIN pairing started #1".to_owned()),
            None,
            Some("[    3.000] PIN pairing done #1".to_owned()),
            Some("[    9.000] PIN pairing started #2".to_owned()),
            Some("[   10.000] pairing without PIN #1".to_owned()),
        ]
    );
}

#[test]
fn stream_requests_get_one_line_each_with_a_running_count() {
    let mut summary = Summary::default();
    let stream = plist(dict(&[("streams", Value::Array(Vec::new()))]));
    assert_eq!(summary.connection(0.5), "[    0.500] connection #1 opened");
    let lines: Vec<String> = [
        request(Method::Setup, "rtsp://tv/1", &[]),
        request(Method::Setup, "rtsp://tv/1", &stream),
        request(Method::Record, "rtsp://tv/1", &[]),
        request(Method::Flush, "rtsp://tv/1", &[]),
        request(Method::Flush, "rtsp://tv/1", &[]),
        request(Method::Teardown, "rtsp://tv/1", &[]),
    ]
    .iter()
    .map(|request| summary.request(4.0, request, 200).expect("a line"))
    .collect();
    assert_eq!(
        lines,
        [
            "[    4.000] SETUP session #1",
            "[    4.000] SETUP stream #1",
            "[    4.000] RECORD #1 (play starts)",
            "[    4.000] FLUSH #1 (pause)",
            "[    4.000] FLUSH #2 (pause)",
            "[    4.000] TEARDOWN #1 (stream ends)",
        ]
    );
    assert_eq!(
        summary.request(5.0, &request(Method::Flush, "rtsp://tv/1", &[]), 455),
        Some("[    5.000] FLUSH #3 (pause) - refused with status 455".to_owned())
    );
}

#[test]
fn requests_no_cell_judges_print_nothing() {
    let mut summary = Summary::default();
    let feedback = request(Method::Post, "/feedback", &[]);
    let info = request(Method::Get, "/info", &[]);
    let progress = request(Method::SetParameter, "rtsp://tv/1", b"progress: 1/2/3\r\n");
    let read = request(Method::GetParameter, "rtsp://tv/1", b"volume\r\n");
    for request in [feedback, info, progress, read] {
        assert_eq!(summary.request(1.0, &request, 200), None);
    }
}

#[test]
fn volume_from_the_app_shows_its_level_whatever_the_status() {
    let mut summary = Summary::default();
    assert_eq!(
        summary.request(2.0, &volume(-20.0), 500),
        Some("[    2.000] volume from the app #1: -20.0 dB".to_owned())
    );
}

#[test]
fn vol_then_one_read_and_no_echo() {
    let mut summary = Summary::default();
    let read = request(Method::GetParameter, "rtsp://tv/1", b"volume\r\n");
    assert_eq!(
        summary.pushed(10.0, -30.0),
        "[   10.000] volume set with vol #1: -30.0 dB"
    );
    assert_eq!(
        summary.request(11.0, &read, 200),
        Some("[   11.000] app read the new level #1: -30.0 dB".to_owned())
    );
    assert_eq!(summary.request(12.0, &read, 200), None);
    let later = 10.0 + ECHO_WINDOW_SECS + 0.5;
    assert_eq!(
        summary.request(later, &volume(-30.0), 500),
        Some(format!("[{later:9.3}] volume from the app #1: -30.0 dB"))
    );
}

#[test]
fn the_app_sending_the_vol_level_back_is_an_echo() {
    let mut summary = Summary::default();
    summary.pushed(10.0, -30.0);
    assert_eq!(
        summary.request(11.3, &volume(-29.7), 500),
        Some("[   11.300] ECHO #1: the app sent -29.7 dB back 1.3 s after vol".to_owned())
    );
    assert_eq!(
        summary.request(12.0, &volume(-12.0), 500),
        Some("[   12.000] volume from the app #1: -12.0 dB".to_owned())
    );
}
