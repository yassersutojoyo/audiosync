//! The lines `fake_tv --summary` prints for the events of the dry-run cells.

mod common;
#[path = "../examples/fake_tv/summary.rs"]
mod summary;

use audiosync_airplay::rtsp::{Method, Request};
use common::ap2::{dict, plist};
use common::hap::tlv;
use plist::Value;
use summary::Summary;

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
        summary.request(1.0, &pair_setup(1, false), 200, None),
        summary.request(2.0, &pair_setup(3, false), 200, None),
        summary.request(3.0, &pair_setup(5, false), 200, None),
        summary.request(9.0, &pair_setup(1, false), 200, None),
        summary.request(10.0, &pair_setup(1, true), 200, None),
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
fn a_password_television_names_its_pairings_after_the_password() {
    let mut summary = Summary::new(true);
    let lines: Vec<Option<String>> = vec![
        summary.request(1.0, &pair_setup(1, false), 200, None),
        summary.request(2.0, &pair_setup(5, false), 200, None),
    ];
    assert_eq!(
        lines,
        vec![
            Some("[    1.000] password pairing started #1".to_owned()),
            Some("[    2.000] password pairing done #1".to_owned()),
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
    .map(|request| summary.request(4.0, request, 200, None).expect("a line"))
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
        summary.request(5.0, &request(Method::Flush, "rtsp://tv/1", &[]), 455, None),
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
        assert_eq!(summary.request(1.0, &request, 200, None), None);
    }
}

#[test]
fn volume_from_the_app_shows_its_level_whatever_the_status() {
    let mut summary = Summary::default();
    assert_eq!(
        summary.request(2.0, &volume(-20.0), 500, None),
        Some("[    2.000] volume from the app #1: -20.0 dB".to_owned())
    );
}

#[test]
fn vol_then_one_read_of_the_new_level_and_no_set_from_the_app() {
    let mut summary = Summary::default();
    let read = request(Method::GetParameter, "rtsp://tv/1", b"volume\r\n");
    assert_eq!(
        summary.pushed(10.0, -30.0),
        "[   10.000] volume set with vol #1: -30.0 dB"
    );
    assert_eq!(
        summary.request(10.5, &read, 200, Some("volume: -20.100000")),
        None,
        "a read answered before vol carries the old level"
    );
    assert_eq!(
        summary.request(11.0, &read, 200, Some("volume: -30.000000")),
        Some("[   11.000] app read the new level #1: -30.0 dB".to_owned())
    );
    assert_eq!(
        summary.request(12.0, &read, 200, Some("volume: -30.000000")),
        None
    );
}

#[test]
fn the_app_sending_the_vol_level_back_is_an_echo_however_late() {
    let mut summary = Summary::default();
    summary.pushed(10.0, -30.0);
    assert_eq!(
        summary.request(41.3, &volume(-29.7), 500, None),
        Some("[   41.300] ECHO #1: the app sent -29.7 dB back 31.3 s after vol".to_owned())
    );
    assert_eq!(
        summary.request(42.0, &volume(-12.0), 500, None),
        Some("[   42.000] volume from the app #1: -12.0 dB".to_owned())
    );
}

#[test]
fn the_app_undoing_the_vol_level_is_an_overwrite() {
    let mut summary = Summary::default();
    summary.pushed(10.0, -30.0);
    assert_eq!(
        summary.request(95.0, &volume(-20.0), 500, None),
        Some("[   95.000] OVERWRITE #1: app sent -20.0 dB, TV is at -30.0 dB".to_owned())
    );
    assert_eq!(
        summary.request(96.0, &volume(-30.0), 500, None),
        Some("[   96.000] volume from the app #1: -30.0 dB".to_owned()),
        "only the first set after a vol answers it"
    );
}
