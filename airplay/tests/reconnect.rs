//! The retry schedule of a lost speaker (P142), on a fake clock. The
//! transport tests check the same schedule over real sockets.

use std::time::{Duration, Instant};

use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::reconnect::{Backoff, MAX_BACK_OFF, RETRY_INTERVAL, retry_delay};

const SECOND: Duration = Duration::from_secs(1);

// covers: P142
#[test]
fn retries_come_after_1_2_4_8_16_then_every_30_seconds() {
    let waits: Vec<Duration> = (0..9).map(retry_delay).collect();
    assert_eq!(
        waits,
        [1, 2, 4, 8, 16, 30, 30, 30, 30]
            .map(|s| s * SECOND)
            .to_vec()
    );
    assert_eq!(retry_delay(u32::MAX), RETRY_INTERVAL);

    let start = Instant::now();
    let mut backoff = Backoff::new();
    let mut now = start;
    let mut attempts = Vec::new();
    for kind in [
        ErrorKind::Lost,
        ErrorKind::Unreachable,
        ErrorKind::Timeout,
        ErrorKind::Busy,
        ErrorKind::Refused,
        ErrorKind::ProtocolError,
        ErrorKind::PairingCancelled,
    ] {
        let at = backoff.on_failure(kind, now).expect("retried");
        attempts.push(at - now);
        now = at;
    }
    assert_eq!(
        attempts,
        [1, 2, 4, 8, 16, 30, 30].map(|s| s * SECOND).to_vec()
    );
    assert_eq!(backoff.failures(), 7);
}

#[test]
fn playing_again_starts_the_schedule_over() {
    let now = Instant::now();
    let mut backoff = Backoff::new();
    backoff.on_failure(ErrorKind::Lost, now);
    backoff.on_failure(ErrorKind::Lost, now);
    backoff.on_playing();
    assert_eq!(backoff.on_failure(ErrorKind::Lost, now), Some(now + SECOND));
}

#[test]
fn errors_that_need_the_user_stop_the_retries() {
    let now = Instant::now();
    for kind in [
        ErrorKind::PasswordRequired,
        ErrorKind::WrongPassword,
        ErrorKind::PairingRequired,
        ErrorKind::PairingFailed,
        ErrorKind::Unsupported(UnsupportedReason::Codec),
    ] {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.on_failure(kind, now), None, "{kind:?}");
        assert_eq!(backoff.failures(), 0);
    }
}

// A pair-verify refused for the receiver's state waits for the back-off
// it names, capped, and otherwise follows the usual schedule.
// covers: P142, P154
#[test]
fn a_receiver_state_refusal_waits_for_its_back_off() {
    let now = Instant::now();
    let mut backoff = Backoff::new();
    assert_eq!(
        backoff.on_receiver_state(30 * SECOND, now),
        now + 30 * SECOND
    );
    assert_eq!(
        backoff.on_receiver_state(Duration::ZERO, now),
        now + 2 * SECOND
    );
    assert_eq!(backoff.failures(), 2);
    let huge = Duration::from_secs(u64::MAX);
    assert_eq!(backoff.on_receiver_state(huge, now), now + MAX_BACK_OFF);
}
