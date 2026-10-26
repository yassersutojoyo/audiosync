//! The connect schedule of new sessions (P222), on a fake clock. The
//! staggered-connect tests check the same schedule over real sockets.

use std::time::{Duration, Instant};

use audiosync_airplay::connects::{CONNECT_SPACING, ConnectQueue, MAX_CONNECT_WAIT, StartKind};
use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::reconnect::Backoff;

fn at(origin: Instant, seconds: f64) -> Instant {
    origin + Duration::from_secs_f64(seconds)
}

fn seconds(origin: Instant, time: Instant) -> f64 {
    let s = time.duration_since(origin).as_secs_f64();
    (s * 1000.0).round() / 1000.0
}

/// A start the test asks for, or takes back, at a time in seconds.
enum Step {
    Start(&'static str, StartKind, f64),
    Remove(&'static str, f64),
}

impl Step {
    fn time(&self) -> f64 {
        match self {
            Step::Start(_, _, t) | Step::Remove(_, t) => *t,
        }
    }
}

/// Runs the queue on a fake clock: starts are pushed when they fall due,
/// every turn that has come is taken and reported as a connect at once.
/// Returns each connect with its time in seconds.
fn run(steps: Vec<Step>) -> Vec<(&'static str, f64)> {
    let origin = Instant::now();
    let mut queue = ConnectQueue::new();
    let mut steps = steps;
    steps.sort_by(|a, b| a.time().total_cmp(&b.time()));
    let mut steps = steps.into_iter().peekable();
    let mut connects = Vec::new();
    loop {
        let next_step = steps.peek().map(|step| at(origin, step.time()));
        let now = match (next_step, queue.poll_timeout()) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        while steps
            .peek()
            .is_some_and(|step| at(origin, step.time()) <= now)
        {
            match steps.next() {
                Some(Step::Start(name, kind, t)) => queue.push(name, kind, at(origin, t)),
                Some(Step::Remove(name, _)) => {
                    assert!(queue.remove(&name), "{name} waits");
                }
                None => {}
            }
        }
        while let Some((name, kind)) = queue.pop(now) {
            connects.push((name, seconds(origin, now)));
            if kind != StartKind::OpenConnection {
                queue.connected(now);
            }
        }
    }
    connects
}

use StartKind::{Host, OpenConnection, Retry};

// covers: P222, P225
#[test]
fn starts_due_together_connect_half_a_second_apart_in_order() {
    let connects = run(vec![
        Step::Start("a", Host, 0.0),
        Step::Start("b", Host, 0.0),
        Step::Start("c", Host, 0.0),
        Step::Start("d", Host, 0.2),
        Step::Start("e", Host, 3.0),
    ]);
    assert_eq!(
        connects,
        vec![("a", 0.0), ("b", 0.5), ("c", 1.0), ("d", 1.5), ("e", 3.0)]
    );
    assert_eq!(CONNECT_SPACING, Duration::from_millis(500));
}

// covers: P222, P225
#[test]
fn a_single_start_connects_at_once() {
    assert_eq!(run(vec![Step::Start("a", Retry, 4.0)]), vec![("a", 4.0)]);
    // One that falls due within the spacing of the last connect waits for it.
    assert_eq!(
        run(vec![
            Step::Start("a", Host, 0.0),
            Step::Start("b", Host, 0.3)
        ]),
        vec![("a", 0.0), ("b", 0.5)]
    );
}

// covers: P222, P225
#[test]
fn the_cap_puts_the_twenty_first_and_twenty_second_of_a_burst_together() {
    const NAMES: [&str; 22] = [
        "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "s12", "s13",
        "s14", "s15", "s16", "s17", "s18", "s19", "s20", "s21",
    ];
    let connects = run(NAMES
        .iter()
        .map(|name| Step::Start(name, Host, 0.0))
        .collect());
    let names: Vec<&str> = connects.iter().map(|(name, _)| *name).collect();
    assert_eq!(names, NAMES.to_vec());
    for (k, (_, time)) in connects.iter().enumerate().take(21) {
        assert_eq!(*time, 0.5 * k as f64, "start {k}");
    }
    assert_eq!(connects[21].1, 10.0);
    assert_eq!(MAX_CONNECT_WAIT, Duration::from_secs(10));
    // The sixteenth starts its connect 7.5 s after the first.
    assert_eq!(connects[15].1, 7.5);
}

// covers: P222, P223, P225
#[test]
fn a_host_start_takes_the_next_turn_ahead_of_waiting_retries() {
    let connects = run(vec![
        Step::Start("host", Host, 0.0),
        Step::Start("r1", Retry, 0.1),
        Step::Start("r2", Retry, 0.1),
        Step::Start("r3", Retry, 0.15),
        Step::Start("click", Host, 0.2),
    ]);
    assert_eq!(
        connects,
        vec![
            ("host", 0.0),
            ("click", 0.5),
            ("r1", 1.0),
            ("r2", 1.5),
            ("r3", 2.0),
        ]
    );
    // A retry never holds up a host start that fell due after it, and a
    // host start never waits for a later one of its own kind.
    let connects = run(vec![
        Step::Start("r1", Retry, 0.0),
        Step::Start("r2", Retry, 0.0),
        Step::Start("h1", Host, 0.1),
        Step::Start("h2", Host, 0.3),
    ]);
    assert_eq!(
        connects,
        vec![("r1", 0.0), ("h1", 0.5), ("h2", 1.0), ("r2", 1.5)]
    );
}

// covers: P222, P225
#[test]
fn a_delayed_retry_counts_its_next_back_off_from_its_actual_connect() {
    let connects = run(vec![
        Step::Start("a", Host, 0.0),
        Step::Start("b", Host, 0.0),
        Step::Start("c", Host, 0.0),
        Step::Start("retry", Retry, 0.2),
    ]);
    let (_, connected) = connects
        .iter()
        .find(|(name, _)| *name == "retry")
        .copied()
        .expect("the retry connects");
    assert_eq!(connected, 1.5);

    // Its session fails at once again: the second back-off (2 s) counts
    // from the connect at 1.5 s, not from 0.2 s when it fell due.
    let origin = Instant::now();
    let mut backoff = Backoff::new();
    backoff.on_failure(ErrorKind::Lost, origin);
    let next = backoff
        .on_failure(ErrorKind::Unreachable, at(origin, connected))
        .expect("retried");
    assert_eq!(seconds(origin, next), 3.5);
}

// covers: P222, P224, P225
#[test]
fn a_removed_start_opens_no_connection_and_the_rest_keep_their_order() {
    let connects = run(vec![
        Step::Start("a", Host, 0.0),
        Step::Start("b", Host, 0.0),
        Step::Start("c", Host, 0.0),
        Step::Start("d", Host, 0.0),
        Step::Remove("c", 0.6),
    ]);
    assert_eq!(connects, vec![("a", 0.0), ("b", 0.5), ("d", 1.0)]);

    // Taken back before its turn while the last connect is longer ago:
    // the next connects no sooner than 0.5 s after the last that started.
    let connects = run(vec![
        Step::Start("a", Host, 0.0),
        Step::Start("b", Retry, 0.0),
        Step::Start("c", Retry, 0.0),
        Step::Remove("b", 0.4),
    ]);
    assert_eq!(connects, vec![("a", 0.0), ("c", 0.5)]);
}

// covers: P222, P225
#[test]
fn a_session_over_an_open_pairing_connection_takes_no_turn() {
    let connects = run(vec![
        Step::Start("a", Host, 0.0),
        Step::Start("b", Host, 0.0),
        Step::Start("paired", OpenConnection, 0.1),
        Step::Start("c", Host, 0.0),
    ]);
    assert_eq!(
        connects,
        vec![("a", 0.0), ("paired", 0.1), ("b", 0.5), ("c", 1.0)]
    );
}

// covers: P222
#[test]
fn a_start_pushed_twice_waits_once_and_a_host_ask_lifts_a_retry() {
    let origin = Instant::now();
    let mut queue = ConnectQueue::new();
    queue.push("a", Host, origin);
    assert_eq!(queue.pop(origin), Some(("a", Host)));
    queue.connected(origin);
    queue.push("r", Retry, at(origin, 0.1));
    queue.push("h", Host, at(origin, 0.2));
    // The host asks for the retrying speaker itself: it goes as a host start.
    queue.push("r", Host, at(origin, 0.3));
    assert_eq!(queue.pop(at(origin, 0.4)), None);
    assert_eq!(queue.pop(at(origin, 0.5)), Some(("r", Host)));
    queue.connected(at(origin, 0.5));
    assert_eq!(queue.pop(at(origin, 0.6)), None);
    assert!(queue.contains(&"h"));
    assert_eq!(queue.clear(), vec!["h"]);
    assert!(queue.is_empty());
}
