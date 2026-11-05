//! The per-device key pins: what each challenge outcome does to a device's
//! entry, its log line, and the file (C119, C129, C131 to C133).

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiosync_cast::auth::{Failure, KeyPin, Outcome, Verified};
use audiosync_cast::device_keys::{DeviceKeyBook, Entry, Verdict, auth_wait, decide};

const OLD: KeyPin = KeyPin([0x11; 32]);
const NEW: KeyPin = KeyPin([0x22; 32]);

fn verified(key: KeyPin) -> Outcome {
    Outcome::Verified(Verified {
        key,
        sha1: false,
        nonce_echoed: true,
        audio_only: false,
        crl: None,
    })
}

const NO_ANSWER: Outcome = Outcome::NoAnswer(Duration::from_secs(5));

// covers: C129, C132
#[test]
fn a_verified_answer_pins_keeps_or_re_pins_the_key() {
    for entry in [None, Some(Entry::Unauthenticated)] {
        let (decision, after) = decide("dev", entry, &verified(NEW), false);
        assert_eq!(decision.verdict, Verdict::Proceed);
        assert!(!decision.warn);
        assert_eq!(decision.line, "cast auth dev: pinned 2222222222222222");
        assert_eq!(after, Some(Entry::Pin(NEW)));
    }
    let (decision, after) = decide("dev", Some(Entry::Pin(NEW)), &verified(NEW), false);
    assert_eq!(decision.verdict, Verdict::Proceed);
    assert_eq!(
        decision.line,
        "cast auth dev: verified, sha256, nonce echoed, policy none, crl none, key 2222222222222222"
    );
    assert_eq!(after, None);
    let (decision, after) = decide("dev", Some(Entry::Pin(OLD)), &verified(NEW), false);
    assert_eq!(decision.verdict, Verdict::Proceed);
    assert_eq!(
        decision.line,
        "cast auth dev: re-pinned, key changed from 1111111111111111 to 2222222222222222"
    );
    assert_eq!(after, Some(Entry::Pin(NEW)));
}

// covers: C129, C130, C132
#[test]
fn a_failed_answer_is_refused_whatever_the_entry() {
    for entry in [None, Some(Entry::Unauthenticated), Some(Entry::Pin(OLD))] {
        for group in [false, true] {
            let (decision, after) = decide("dev", entry, &Outcome::Failed(Failure::Chain), group);
            assert_eq!(decision.verdict, Verdict::Refused);
            assert_eq!(decision.line, "cast auth dev: failed, chain, refused");
            assert_eq!(after, None);
        }
    }
}

// covers: C129, C131, C132
#[test]
fn no_answer_or_a_decline_depends_on_the_entry() {
    for outcome in [NO_ANSWER, Outcome::Declined(0)] {
        let (decision, after) = decide("dev", Some(Entry::Pin(OLD)), &outcome, false);
        assert_eq!(decision.verdict, Verdict::ConnectFailed);
        assert_eq!(
            decision.line,
            format!("cast auth dev: {outcome}, connect failed")
        );
        assert_eq!(after, None);

        let (decision, after) = decide("dev", None, &outcome, false);
        assert_eq!(decision.verdict, Verdict::Proceed);
        assert!(
            decision.warn,
            "the notice shows once, when the mark is written"
        );
        assert_eq!(
            decision.line,
            format!("cast auth dev: {outcome}, unauthenticated, warned")
        );
        assert_eq!(after, Some(Entry::Unauthenticated));

        let (decision, after) = decide("dev", Some(Entry::Unauthenticated), &outcome, false);
        assert_eq!(decision.verdict, Verdict::Proceed);
        assert!(!decision.warn);
        assert_eq!(
            decision.line,
            format!("cast auth dev: {outcome}, unauthenticated")
        );
        assert_eq!(after, None);
    }
}

// covers: C129
#[test]
fn a_group_is_never_pinned() {
    let (decision, after) = decide("group", None, &verified(NEW), true);
    assert_eq!(decision.verdict, Verdict::Proceed);
    assert!(decision.line.starts_with("cast auth group: verified, "));
    assert_eq!(after, None);
    let (decision, after) = decide("group", None, &NO_ANSWER, true);
    assert!(decision.warn);
    assert_eq!(after, Some(Entry::Unauthenticated));
}

// covers: C119
#[test]
fn a_marked_device_is_waited_on_for_one_second() {
    assert_eq!(auth_wait(None), Duration::from_secs(5));
    assert_eq!(auth_wait(Some(Entry::Pin(OLD))), Duration::from_secs(5));
    assert_eq!(
        auth_wait(Some(Entry::Unauthenticated)),
        Duration::from_secs(1)
    );
}

fn file_book(text: &str) -> (DeviceKeyBook, Arc<Mutex<Vec<String>>>, Option<io::Error>) {
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&written);
    let text = text.to_owned();
    let (book, error) = DeviceKeyBook::open(
        PathBuf::from("cast-device-keys.txt"),
        move |_| Ok(text.into_bytes()),
        move |_, bytes| {
            sink.lock()
                .expect("written")
                .push(String::from_utf8(bytes.to_vec()).expect("text"));
            Ok(())
        },
    );
    (book, written, error)
}

// covers: C129, C131
#[test]
fn the_book_keeps_pins_and_marks_across_launches_and_writes_only_changes() {
    let pin = "1111111111111111111111111111111111111111111111111111111111111111";
    let (mut book, written, error) = file_book(&format!("a {pin}\r\nb unauthenticated\n"));
    assert!(error.is_none());
    assert_eq!(book.entry("a"), Some(Entry::Pin(OLD)));
    assert_eq!(book.entry("b"), Some(Entry::Unauthenticated));
    let decision = book.judge("b", &NO_ANSWER, false);
    assert!(
        !decision.warn,
        "the notice does not come back while the mark stays"
    );
    assert!(written.lock().expect("written").is_empty());
    book.judge("c", &NO_ANSWER, false);
    book.judge("b", &verified(NEW), false);
    let last = written.lock().expect("written").last().cloned();
    assert_eq!(
        last.as_deref(),
        Some(
            format!(
                "a {pin}\nb 2222222222222222222222222222222222222222222222222222222222222222\nc unauthenticated\n"
            )
            .as_str()
        )
    );
    let (book, _, error) = file_book("a not-a-pin\nb unauthenticated\n");
    assert_eq!(
        error.map(|error| error.kind()),
        Some(io::ErrorKind::InvalidData)
    );
    assert_eq!(book.entry("a"), None);
    assert_eq!(
        book.entry("b"),
        Some(Entry::Unauthenticated),
        "a bad line costs only itself"
    );
}

// covers: C133
#[test]
fn removing_a_device_clears_its_pin_or_mark() {
    let mut book = DeviceKeyBook::new();
    book.judge("a", &verified(OLD), false);
    book.judge("b", &NO_ANSWER, false);
    assert_eq!(book.clear("a").as_deref(), Some("cast auth a: pin cleared"));
    assert_eq!(
        book.clear("b").as_deref(),
        Some("cast auth b: mark cleared")
    );
    assert_eq!(book.clear("b"), None);
    let decision = book.judge("a", &NO_ANSWER, false);
    assert!(decision.warn, "judged as a device with no entry");
}
