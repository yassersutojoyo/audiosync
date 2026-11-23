//! Apps listening to a microphone while the whole system streams (S485).

use audiosync_core::policy::call_listeners::{
    CallListeners, Facts, HOLD, ListenerAction, STAYED_AFTER,
};

const CALL: &str = r"C:\Program Files\Call\call.exe";
const PLAYER: &str = "player.exe";

fn apps(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn poll(
    watch: &mut CallListeners,
    whole_system: bool,
    listening: &[&str],
    on_silent: &[&str],
    now: f64,
) -> Vec<ListenerAction> {
    let listening = apps(listening);
    let on_silent = apps(on_silent);
    watch.update(
        Facts {
            whole_system,
            listening: &listening,
            on_silent: &on_silent,
        },
        now,
    )
}

fn out(app: &str) -> ListenerAction {
    ListenerAction::LeaveOut(app.to_owned())
}

fn back(app: &str) -> ListenerAction {
    ListenerAction::Rejoin(app.to_owned())
}

// covers: S485
#[test]
fn a_listener_is_left_out_while_the_whole_system_streams() {
    let mut watch = CallListeners::default();
    assert_eq!(poll(&mut watch, true, &[CALL], &[], 0.0), vec![out(CALL)]);
    assert_eq!(poll(&mut watch, true, &[CALL], &[], 1.0), vec![]);
    assert_eq!(watch.left_out().collect::<Vec<_>>(), vec![CALL]);
}

// covers: S485
#[test]
fn nothing_is_left_out_unless_the_whole_system_streams() {
    let mut watch = CallListeners::default();
    assert_eq!(poll(&mut watch, false, &[CALL], &[], 0.0), vec![]);
    assert_eq!(watch.left_out().count(), 0);
}

// covers: S485
#[test]
fn a_listener_rejoins_only_after_the_hold() {
    let mut watch = CallListeners::default();
    poll(&mut watch, true, &[CALL], &[], 0.0);
    poll(&mut watch, true, &[CALL], &[], 10.0);
    assert_eq!(poll(&mut watch, true, &[], &[], 10.5), vec![]);
    assert_eq!(poll(&mut watch, true, &[], &[], 10.0 + HOLD - 0.1), vec![]);
    assert_eq!(
        poll(&mut watch, true, &[], &[], 10.0 + HOLD),
        vec![back(CALL)]
    );
    assert_eq!(watch.left_out().count(), 0);
}

// covers: S485
#[test]
fn a_microphone_reopened_within_the_hold_keeps_the_app_out() {
    let mut watch = CallListeners::default();
    poll(&mut watch, true, &[CALL], &[], 0.0);
    poll(&mut watch, true, &[], &[], 1.0);
    assert_eq!(
        poll(&mut watch, true, &[CALL], &[], 1.0 + HOLD - 0.5),
        vec![]
    );
    assert_eq!(poll(&mut watch, true, &[], &[], 1.0 + HOLD + 0.1), vec![]);
    assert_eq!(
        poll(&mut watch, true, &[CALL], &[], 1.0 + HOLD + 0.2),
        vec![]
    );
}

// covers: S485
#[test]
fn every_app_rejoins_when_the_whole_system_stops_streaming() {
    let mut watch = CallListeners::default();
    poll(&mut watch, true, &[CALL, PLAYER], &[], 0.0);
    assert_eq!(
        poll(&mut watch, false, &[CALL, PLAYER], &[], 1.0),
        vec![back(CALL), back(PLAYER)]
    );
    assert_eq!(poll(&mut watch, false, &[CALL], &[], 2.0), vec![]);
}

// covers: S485
#[test]
fn a_listener_still_on_the_silent_endpoint_stays_and_is_reported_once() {
    let mut watch = CallListeners::default();
    poll(&mut watch, true, &[CALL], &[CALL], 0.0);
    assert_eq!(
        poll(&mut watch, true, &[CALL], &[CALL], STAYED_AFTER - 0.01),
        vec![]
    );
    assert_eq!(
        poll(&mut watch, true, &[CALL], &[CALL], STAYED_AFTER),
        vec![ListenerAction::Stayed(CALL.to_owned())]
    );
    assert_eq!(poll(&mut watch, true, &[CALL], &[CALL], 5.0), vec![]);
    assert_eq!(
        poll(&mut watch, true, &[], &[], 5.0 + HOLD),
        vec![back(CALL)]
    );
}

// covers: S485
#[test]
fn the_stayed_check_counts_from_a_later_act() {
    let mut watch = CallListeners::default();
    poll(&mut watch, true, &[CALL], &[], 0.0);
    poll(&mut watch, true, &[CALL], &[], STAYED_AFTER);
    watch.acted(CALL, 2.0);
    assert_eq!(
        poll(
            &mut watch,
            true,
            &[CALL],
            &[CALL],
            2.0 + STAYED_AFTER - 0.01
        ),
        vec![]
    );
    assert_eq!(
        poll(&mut watch, true, &[CALL], &[CALL], 2.0 + STAYED_AFTER),
        vec![ListenerAction::Stayed(CALL.to_owned())]
    );
}

// covers: S485
#[test]
fn a_listener_that_moved_is_never_reported_as_stayed() {
    let mut watch = CallListeners::default();
    poll(&mut watch, true, &[CALL], &[CALL], 0.0);
    assert_eq!(poll(&mut watch, true, &[CALL], &[], STAYED_AFTER), vec![]);
    assert_eq!(poll(&mut watch, true, &[CALL], &[CALL], 1.0), vec![]);
}
