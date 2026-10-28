//! The channel state machine on a test clock (sections 3 and 4): what it
//! sends, when, and what it reports, without a socket.

mod common;

use std::time::{Duration, Instant};

use audiosync_cast::channel::{
    Channel, DEAD_AFTER, Dropped, Event, LAUNCH_TIMEOUT, PING_INTERVAL, STATUS_TIMEOUT,
    STOP_STEP_TIMEOUT,
};
use audiosync_cast::error::{EndReason, LaunchError};
use audiosync_cast::ids::RequestIds;
use audiosync_cast::media::MAX_DEVICE_TEXT_CHARS;
use audiosync_cast::message::{
    CastMessage, DEFAULT_MEDIA_RECEIVER, NS_CONNECTION, NS_DEVICE_AUTH, NS_HEARTBEAT, NS_MEDIA,
    NS_MULTIZONE, NS_RECEIVER, PLATFORM, PayloadError, PayloadType, SENDER,
};
use audiosync_cast::status::ControlType;
use common::{app, json_message};
use serde_json::{Value, json};

fn sent(channel: &mut Channel) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    while let Some(message) = channel.poll_outbound() {
        assert_eq!(message.source_id, SENDER);
        let payload = message.json_payload().expect("a JSON object");
        out.push((
            message.destination_id,
            message.namespace,
            Value::Object(payload),
        ));
    }
    out
}

fn events(channel: &mut Channel) -> Vec<Event> {
    std::iter::from_fn(|| channel.poll_event()).collect()
}

fn status(request_id: u32, apps: Vec<Value>) -> CastMessage {
    json_message(
        PLATFORM,
        SENDER,
        NS_RECEIVER,
        json!({"type": "RECEIVER_STATUS", "requestId": request_id, "status": {"applications": apps, "volume": {"level": 0.25}}}),
    )
}

/// A channel that got its first status at `start`.
fn ready(start: Instant, apps: Vec<Value>) -> Channel {
    let mut channel = Channel::open(start, RequestIds::starting_at(100));
    sent(&mut channel);
    channel.handle(start, status(100, apps));
    assert!(channel.is_ready());
    events(&mut channel);
    channel
}

fn running(start: Instant) -> Channel {
    let mut channel = ready(start, Vec::new());
    channel.launch(start).expect("ready");
    sent(&mut channel);
    channel.handle(
        start,
        status(101, vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")]),
    );
    sent(&mut channel);
    events(&mut channel);
    channel
}

// covers: C32, C33
#[test]
fn connect_then_get_status_go_to_the_platform_and_the_first_status_makes_it_ready() {
    let start = Instant::now();
    let mut channel = Channel::open(start, RequestIds::starting_at(7));
    let out = sent(&mut channel);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].0, PLATFORM);
    assert_eq!(out[0].1, NS_CONNECTION);
    assert_eq!(out[0].2, json!({"type": "CONNECT"}));
    assert_eq!(out[1].0, PLATFORM);
    assert_eq!(out[1].1, NS_RECEIVER);
    assert_eq!(out[1].2, json!({"type": "GET_STATUS", "requestId": 7}));
    assert!(!channel.is_ready());
    channel.handle(start, status(7, Vec::new()));
    assert!(channel.is_ready());
    assert!(
        events(&mut channel)
            .iter()
            .any(|event| matches!(event, Event::Ready(_)))
    );
}

// covers: C33
#[test]
fn no_status_within_five_seconds_ends_the_attempt() {
    let start = Instant::now();
    let mut channel = Channel::open(start, RequestIds::starting_at(1));
    channel.tick(start + STATUS_TIMEOUT - Duration::from_millis(1));
    assert!(!channel.is_ended());
    channel.tick(start + STATUS_TIMEOUT);
    assert!(channel.is_ended());
    assert_eq!(
        events(&mut channel).last(),
        Some(&Event::Ended(EndReason::NoStatus))
    );
    assert_eq!(STATUS_TIMEOUT, Duration::from_secs(5));
}

// covers: C29
#[test]
fn request_ids_start_random_and_positive_and_grow_by_one() {
    let starts: Vec<u32> = (0..32).map(|_| RequestIds::random().next_id()).collect();
    assert!(starts.iter().all(|&id| id > 0 && id <= i32::MAX as u32));
    let mut distinct = starts.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(distinct.len() > 1, "the start is random: {starts:?}");
    assert_eq!(RequestIds::starting_at(0).next_id(), 1, "never 0");
    let mut ids = RequestIds::starting_at(41);
    let run: Vec<u32> = (0..5).map(|_| ids.next_id()).collect();
    assert_eq!(run, [41, 42, 43, 44, 45]);
    let mut top = RequestIds::starting_at(i32::MAX as u32);
    assert_eq!(top.next_id(), i32::MAX as u32);
    assert_eq!(top.next_id(), 1, "wraps without passing through 0");

    // Requests on one channel carry increasing ids; heartbeat and connection
    // messages carry none.
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.tick(start + PING_INTERVAL);
    channel.launch(start + PING_INTERVAL).expect("ready");
    let out = sent(&mut channel);
    let ping = out.iter().find(|m| m.2["type"] == "PING").expect("ping");
    assert!(ping.2.get("requestId").is_none());
    let launch = out
        .iter()
        .find(|m| m.2["type"] == "LAUNCH")
        .expect("launch");
    assert_eq!(launch.2["requestId"], 101);
}

// covers: C29, C41
#[test]
fn a_launch_without_success_in_fifteen_seconds_fails() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.launch(start).expect("ready");
    sent(&mut channel);
    // A status that answers the launch but doesn't list the app yet keeps
    // it pending.
    channel.handle(start + Duration::from_secs(1), status(101, Vec::new()));
    let mut now = start;
    while now < start + LAUNCH_TIMEOUT - Duration::from_secs(1) {
        now += Duration::from_secs(1);
        // Keep the channel alive with device pings.
        channel.handle(
            now,
            json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"type": "PING"})),
        );
        channel.tick(now);
    }
    assert!(
        !events(&mut channel)
            .iter()
            .any(|event| matches!(event, Event::LaunchFailed(_)))
    );
    channel.tick(start + LAUNCH_TIMEOUT);
    assert!(
        events(&mut channel).contains(&Event::LaunchFailed(LaunchError::Timeout)),
        "no success within 15 s fails the launch"
    );
    assert_eq!(LAUNCH_TIMEOUT, Duration::from_secs(15));
    // The channel is ready for another try.
    assert!(channel.launch(start + LAUNCH_TIMEOUT).is_ok());
}

// covers: C41
#[test]
fn cancelled_retries_once_and_other_reasons_fail_at_once() {
    let start = Instant::now();
    let error = |id: u32, reason: &str| {
        json_message(
            PLATFORM,
            SENDER,
            NS_RECEIVER,
            json!({"type": "LAUNCH_ERROR", "requestId": id, "reason": reason}),
        )
    };

    let mut channel = ready(start, Vec::new());
    channel.launch(start).expect("ready");
    sent(&mut channel);
    channel.handle(start, error(101, "CANCELLED"));
    let retry = sent(&mut channel);
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].2["type"], "LAUNCH");
    assert_eq!(retry[0].2["requestId"], 102);
    assert!(events(&mut channel).is_empty());
    channel.handle(start, error(102, "CANCELLED"));
    assert!(sent(&mut channel).is_empty(), "only one retry");
    assert_eq!(
        events(&mut channel),
        vec![Event::LaunchFailed(LaunchError::Refused {
            reason: "CANCELLED".to_owned()
        })]
    );

    let mut channel = ready(start, Vec::new());
    channel.launch(start).expect("ready");
    sent(&mut channel);
    channel.handle(start, error(101, "NOT_ALLOWED"));
    assert!(sent(&mut channel).is_empty());
    assert_eq!(
        events(&mut channel),
        vec![Event::LaunchFailed(LaunchError::Refused {
            reason: "NOT_ALLOWED".to_owned()
        })]
    );
}

#[test]
fn launch_refusal_reasons_are_cut_short_and_printable() {
    let start = Instant::now();
    let long = format!("BAD\n\u{1b}[2J{}", "\u{e9}".repeat(70_000));
    let expected = format!("BAD??[2J{}", "\u{e9}".repeat(MAX_DEVICE_TEXT_CHARS - 8));
    for kind in ["LAUNCH_ERROR", "INVALID_REQUEST"] {
        let mut channel = ready(start, Vec::new());
        channel.launch(start).expect("ready");
        sent(&mut channel);
        let reply = json!({"type": kind, "requestId": 101, "reason": long});
        channel.handle(start, json_message(PLATFORM, SENDER, NS_RECEIVER, reply));
        assert_eq!(
            events(&mut channel),
            vec![Event::LaunchFailed(LaunchError::Refused {
                reason: expected.clone()
            })],
            "{kind}"
        );
    }
}

// covers: C39, C40, C41, C42
#[test]
fn an_idle_device_gets_a_launch_and_the_app_gets_connect() {
    let start = Instant::now();
    let other = app("0F5096E8", "other-session", "other-transport");
    let mut channel = ready(start, vec![other]);
    channel.launch(start).expect("ready");
    let out = sent(&mut channel);
    assert_eq!(
        out,
        vec![(
            PLATFORM.to_owned(),
            NS_RECEIVER.to_owned(),
            json!({"type": "LAUNCH", "appId": "CC1AD845", "requestId": 101})
        )]
    );
    assert_eq!(DEFAULT_MEDIA_RECEIVER, "CC1AD845");
    channel.handle(
        start,
        status(101, vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")]),
    );
    let out = sent(&mut channel);
    assert_eq!(
        out,
        vec![(
            "t-1".to_owned(),
            NS_CONNECTION.to_owned(),
            json!({"type": "CONNECT"})
        )]
    );
    let launched = events(&mut channel)
        .into_iter()
        .find_map(|event| match event {
            Event::Launched { app, reused } => Some((app, reused)),
            _ => None,
        })
        .expect("launched");
    assert_eq!(launched.0.session_id, "s-1");
    assert_eq!(launched.0.transport_id, "t-1");
    assert!(!launched.1);
    assert_eq!(channel.app().map(|a| a.session_id.as_str()), Some("s-1"));
}

// covers: C40
#[test]
fn a_running_default_media_receiver_is_reused_without_a_launch() {
    let start = Instant::now();
    let mut channel = ready(start, vec![app(DEFAULT_MEDIA_RECEIVER, "s-9", "t-9")]);
    channel.launch(start).expect("ready");
    let out = sent(&mut channel);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, "t-9");
    assert_eq!(out[0].2, json!({"type": "CONNECT"}));
    assert!(
        events(&mut channel)
            .iter()
            .any(|event| matches!(event, Event::Launched { reused: true, .. }))
    );
}

// covers: C34
#[test]
fn ping_goes_to_the_platform_every_five_seconds() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    let mut pings = Vec::new();
    for tenth in 1..=120u64 {
        let now = start + Duration::from_millis(tenth * 100);
        // The device keeps talking, so the channel stays alive.
        if tenth % 10 == 0 {
            channel.handle(now, status(0, Vec::new()));
        }
        channel.tick(now);
        for (destination, namespace, payload) in sent(&mut channel) {
            if payload["type"] == "PING" {
                assert_eq!(destination, PLATFORM);
                assert_eq!(namespace, NS_HEARTBEAT);
                pings.push(now - start);
            }
        }
    }
    assert_eq!(pings, [Duration::from_secs(5), Duration::from_secs(10)]);
}

// covers: C35
#[test]
fn every_device_ping_gets_a_pong_to_its_sender() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    for source in [PLATFORM, "transport-7"] {
        channel.handle(
            start,
            json_message(source, SENDER, NS_HEARTBEAT, json!({"type": "PING"})),
        );
        let out = sent(&mut channel);
        assert_eq!(
            out,
            vec![(
                source.to_owned(),
                NS_HEARTBEAT.to_owned(),
                json!({"type": "PONG"})
            )]
        );
    }
}

// covers: C36
#[test]
fn fifteen_seconds_without_any_message_ends_the_channel() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.handle(
        start + Duration::from_secs(10),
        json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"type": "PONG"})),
    );
    channel.tick(start + Duration::from_secs(24));
    assert!(!channel.is_ended(), "any inbound message resets the timer");
    channel.tick(start + Duration::from_secs(10) + DEAD_AFTER);
    assert!(channel.is_ended());
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::Silent)));
    assert_eq!(DEAD_AFTER, Duration::from_secs(15));
}

// covers: C26
#[test]
fn messages_for_another_destination_are_dropped() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.handle(
        start,
        json_message(PLATFORM, "sender-7", NS_HEARTBEAT, json!({"type": "PING"})),
    );
    assert!(sent(&mut channel).is_empty());
    assert_eq!(
        events(&mut channel),
        vec![Event::Dropped(Dropped::Destination("sender-7".to_owned()))]
    );
    // A broadcast is accepted.
    channel.handle(
        start,
        json_message(PLATFORM, "*", NS_HEARTBEAT, json!({"type": "PING"})),
    );
    assert_eq!(sent(&mut channel).len(), 1);
}

// covers: C27
#[test]
fn messages_on_other_namespaces_are_dropped() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.handle(
        start,
        json_message(
            PLATFORM,
            SENDER,
            "urn:x-cast:com.example.custom",
            json!({"type": "PING"}),
        ),
    );
    assert!(sent(&mut channel).is_empty());
    assert_eq!(
        events(&mut channel),
        vec![Event::Dropped(Dropped::Namespace(
            "urn:x-cast:com.example.custom".to_owned()
        ))]
    );
    // The four namespaces of C27 and their names.
    assert_eq!(NS_CONNECTION, "urn:x-cast:com.google.cast.tp.connection");
    assert_eq!(NS_HEARTBEAT, "urn:x-cast:com.google.cast.tp.heartbeat");
    assert_eq!(NS_RECEIVER, "urn:x-cast:com.google.cast.receiver");
    assert_eq!(NS_MEDIA, "urn:x-cast:com.google.cast.media");
}

// covers: C28
#[test]
fn malformed_json_drops_one_message_and_keeps_the_channel() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    let mut broken = json_message(PLATFORM, SENDER, NS_RECEIVER, json!({}));
    broken.payload_utf8 = Some("{\"type\": \"RECEIVER_STATUS\",".to_owned());
    channel.handle(start, broken);
    let mut array = json_message(PLATFORM, SENDER, NS_RECEIVER, json!([1, 2]));
    array.payload_utf8 = Some("[1, 2]".to_owned());
    channel.handle(start, array);
    // Unknown types and fields are ignored.
    channel.handle(
        start,
        json_message(
            PLATFORM,
            SENDER,
            NS_RECEIVER,
            json!({"type": "SOMETHING_NEW", "extra": 1}),
        ),
    );
    assert_eq!(
        events(&mut channel),
        vec![
            Event::Dropped(Dropped::Payload(PayloadError::InvalidJson)),
            Event::Dropped(Dropped::Payload(PayloadError::NotAnObject)),
        ]
    );
    assert!(!channel.is_ended());
    channel.handle(
        start,
        json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"type": "PING"})),
    );
    assert_eq!(sent(&mut channel).len(), 1, "the channel still answers");
}

// covers: C30, C31, C6
#[test]
fn device_auth_is_never_sent_and_inbound_device_auth_is_ignored() {
    let start = Instant::now();
    let mut channel = Channel::open(start, RequestIds::starting_at(1));
    let mut out = sent(&mut channel);
    channel.handle(start, status(1, Vec::new()));
    channel.launch(start).expect("ready");
    out.extend(sent(&mut channel));
    assert!(
        out.iter()
            .all(|(_, namespace, _)| namespace != NS_DEVICE_AUTH)
    );
    events(&mut channel);
    let challenge = CastMessage {
        namespace: NS_DEVICE_AUTH.to_owned(),
        payload_type: PayloadType::Binary as i32,
        payload_utf8: None,
        payload_binary: Some(vec![0x0a, 0x00]),
        ..json_message(PLATFORM, SENDER, NS_DEVICE_AUTH, json!({}))
    };
    channel.handle(start, challenge);
    assert!(sent(&mut channel).is_empty());
    assert_eq!(
        events(&mut channel),
        vec![Event::Dropped(Dropped::DeviceAuth)]
    );
    assert!(!channel.is_ended());
}

// covers: C37
#[test]
fn receiver_status_fields_and_their_defaults() {
    let start = Instant::now();
    let mut channel = Channel::open(start, RequestIds::starting_at(1));
    channel.handle(
        start,
        json_message(
            PLATFORM,
            SENDER,
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "requestId": 1, "status": {
                "applications": [app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")],
                "volume": {},
                "isStandBy": false,
            }}),
        ),
    );
    let status = channel.status().expect("status").clone();
    assert_eq!(status.volume.level, 1.0);
    assert!(!status.volume.muted);
    assert_eq!(status.volume.control_type, ControlType::Attenuation);
    assert_eq!(status.volume.step_interval, None);
    let app = &status.applications[0];
    assert_eq!(app.app_id, DEFAULT_MEDIA_RECEIVER);
    assert_eq!(app.display_name.as_deref(), Some("Default Media Receiver"));
    assert_eq!(app.session_id.as_deref(), Some("s-1"));
    assert_eq!(app.transport_id.as_deref(), Some("t-1"));
    assert_eq!(app.namespaces, ["urn:x-cast:com.google.cast.media"]);
    assert_eq!(app.status_text.as_deref(), Some("Ready to cast"));
    assert!(status.other.iter().any(|(key, _)| key == "isStandBy"));

    channel.handle(
        start,
        json_message(
            PLATFORM,
            SENDER,
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "requestId": 0, "status": {
                "volume": {"level": 0.3, "muted": true, "controlType": "fixed", "stepInterval": 0.05},
            }}),
        ),
    );
    let status = channel.status().expect("status");
    assert_eq!(status.volume.level, 0.3);
    assert!(status.volume.muted);
    assert_eq!(status.volume.control_type, ControlType::Fixed);
    assert_eq!(status.volume.step_interval, Some(0.05));
    assert_eq!(
        status.applications.len(),
        1,
        "no applications field keeps the known ones"
    );
}

// covers: C38
#[test]
fn an_unasked_status_becomes_the_current_state() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.handle(
        start,
        json_message(
            PLATFORM,
            "*",
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "requestId": 0, "status": {"volume": {"level": 0.75}}}),
        ),
    );
    assert_eq!(channel.status().expect("status").volume.level, 0.75);
    assert!(
        events(&mut channel)
            .iter()
            .any(|event| matches!(event, Event::Status(status) if status.volume.level == 0.75))
    );
}

// covers: C43
#[test]
fn the_app_vanishing_from_the_status_ends_the_session_as_taken_over() {
    let start = Instant::now();
    let mut channel = running(start);
    channel.handle(
        start,
        status(0, vec![app("0F5096E8", "someone-else", "t-2")]),
    );
    assert!(channel.is_ended());
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::TakenOver)));
    assert!(sent(&mut channel).is_empty(), "no relaunch");
}

// covers: C37, C43
#[test]
fn a_status_missing_its_applications_or_volume_keeps_what_was_known() {
    let start = Instant::now();
    let mut channel = running(start);
    let receiver_status = |body: Value| json_message(PLATFORM, SENDER, NS_RECEIVER, body);
    channel.handle(
        start,
        receiver_status(json!({"type": "RECEIVER_STATUS", "requestId": 0})),
    );
    channel.handle(
        start,
        receiver_status(
            json!({"type": "RECEIVER_STATUS", "requestId": 0, "status": {"volume": {"level": 0.6}}}),
        ),
    );
    channel.handle(
        start,
        receiver_status(
            json!({"type": "RECEIVER_STATUS", "requestId": 0, "status": {
                "applications": [app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")],
            }}),
        ),
    );
    let seen = events(&mut channel);
    assert!(!channel.is_ended(), "{seen:?}");
    let levels: Vec<f64> = seen
        .iter()
        .filter_map(|event| match event {
            Event::Status(status) => Some(status.volume.level),
            _ => None,
        })
        .collect();
    assert_eq!(levels, [0.6, 0.6], "no status without a status object");
    assert!(channel.status().expect("status").has_session("s-1"));
}

// covers: C37
#[test]
fn a_volume_without_a_level_keeps_the_known_level() {
    let start = Instant::now();
    let mut channel = running(start);
    let known = channel.status().expect("status").volume.level;
    assert!(known < 1.0);
    channel.handle(
        start,
        json_message(
            PLATFORM,
            SENDER,
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "requestId": 0, "status": {"volume": {"muted": false}}}),
        ),
    );
    let seen = events(&mut channel);
    assert_eq!(channel.status().expect("status").volume.level, known);
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::Status(status) if status.volume.level != known)),
        "{seen:?}"
    );
}

// covers: C70, C81
#[test]
fn a_level_sent_but_unanswered_goes_to_the_next_channel() {
    let start = Instant::now();
    let mut channel = running(start);
    channel.set_volume(start, 0.3).expect("sent");
    channel
        .set_volume(start, 0.5)
        .expect("waits for the interval");
    channel.tick(start + Duration::from_millis(100));
    // Sent into a channel that then went silent, with no status to answer it.
    channel.end(EndReason::Silent);
    assert_eq!(channel.take_queued_volume(), Some(0.5));
}

// covers: C44
#[test]
fn stop_sends_receiver_stop_with_the_session_then_closes_both_endpoints() {
    let start = Instant::now();
    let mut channel = running(start);
    channel.stop(start);
    let out = sent(&mut channel);
    assert_eq!(
        out,
        vec![(
            PLATFORM.to_owned(),
            NS_RECEIVER.to_owned(),
            json!({"type": "STOP", "sessionId": "s-1", "requestId": 102})
        )]
    );
    channel.handle(start, status(102, Vec::new()));
    let out = sent(&mut channel);
    let closes: Vec<&str> = out
        .iter()
        .filter(|(_, namespace, payload)| namespace == NS_CONNECTION && payload["type"] == "CLOSE")
        .map(|(destination, _, _)| destination.as_str())
        .collect();
    assert_eq!(closes, ["t-1", PLATFORM]);
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::Stopped)));
}

#[test]
fn a_silent_stop_reply_holds_the_teardown_at_most_two_seconds() {
    let start = Instant::now();
    let mut channel = running(start);
    channel.stop(start);
    sent(&mut channel);
    channel.tick(start + STOP_STEP_TIMEOUT - Duration::from_millis(1));
    assert!(!channel.is_ended());
    channel.tick(start + STOP_STEP_TIMEOUT);
    assert!(channel.is_ended());
    let closes = sent(&mut channel)
        .into_iter()
        .filter(|(_, _, payload)| payload["type"] == "CLOSE")
        .count();
    assert_eq!(closes, 2);
}

#[test]
fn stop_never_stops_an_app_the_sender_did_not_launch_or_reuse() {
    let start = Instant::now();
    let mut channel = ready(start, vec![app(DEFAULT_MEDIA_RECEIVER, "s-5", "t-5")]);
    channel.stop(start);
    let out = sent(&mut channel);
    assert!(out.iter().all(|(_, _, payload)| payload["type"] != "STOP"));
    assert_eq!(out.len(), 1, "only the CLOSE to the platform");
    assert_eq!(out[0].0, PLATFORM);
}

#[test]
fn a_close_from_the_app_checks_the_status_and_reconnects_once() {
    let start = Instant::now();
    let mut channel = running(start);
    let close = json_message("t-1", SENDER, NS_CONNECTION, json!({"type": "CLOSE"}));
    channel.handle(start, close.clone());
    let out = sent(&mut channel);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].2["type"], "GET_STATUS");
    let id = out[0].2["requestId"].as_u64().expect("id") as u32;
    channel.handle(
        start,
        status(id, vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")]),
    );
    let out = sent(&mut channel);
    assert_eq!(
        out,
        vec![(
            "t-1".to_owned(),
            NS_CONNECTION.to_owned(),
            json!({"type": "CONNECT"})
        )]
    );
    assert!(events(&mut channel).contains(&Event::AppReconnected));
    channel.handle(start, close);
    assert!(channel.is_ended());
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::AppClosed)));
}

#[test]
fn a_close_from_the_platform_ends_the_channel() {
    let start = Instant::now();
    let mut channel = running(start);
    channel.handle(
        start,
        json_message(PLATFORM, SENDER, NS_CONNECTION, json!({"type": "CLOSE"})),
    );
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::ClosedByDevice)));
}

// covers: C33
#[test]
fn a_launch_is_refused_before_the_first_status_and_while_one_runs() {
    let start = Instant::now();
    let mut channel = Channel::open(start, RequestIds::starting_at(1));
    assert_eq!(channel.launch(start), Err(LaunchError::NotReady));
    let out = sent(&mut channel);
    assert_eq!(out.len(), 2, "only CONNECT and GET_STATUS: {out:?}");
    channel.handle(start, status(1, Vec::new()));
    assert!(channel.launch(start).is_ok());
    sent(&mut channel);
    assert_eq!(channel.launch(start), Err(LaunchError::NotReady));
    assert!(sent(&mut channel).is_empty(), "no second LAUNCH");
}

#[test]
fn ending_mid_launch_fails_the_launch_once_and_then_nothing_moves() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.launch(start).expect("ready");
    let reason = EndReason::Io("reset".to_owned());
    channel.end(reason.clone());
    assert!(
        channel.poll_outbound().is_none(),
        "the unsent LAUNCH is dropped"
    );
    assert_eq!(
        events(&mut channel),
        vec![
            Event::LaunchFailed(LaunchError::Ended(reason.clone())),
            Event::Ended(reason),
        ]
    );
    channel.end(EndReason::Silent);
    assert!(events(&mut channel).is_empty(), "a channel ends once");

    channel.handle(
        start,
        json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"type": "PING"})),
    );
    channel.handle(start, status(101, Vec::new()));
    channel.tick(start + LAUNCH_TIMEOUT + DEAD_AFTER);
    assert!(sent(&mut channel).is_empty());
    assert!(events(&mut channel).is_empty());
    assert_eq!(channel.launch(start), Err(LaunchError::NotReady));
}

// covers: C36
#[test]
fn a_message_without_a_type_or_for_someone_else_still_keeps_the_channel_alive() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    let untyped = start + Duration::from_secs(10);
    channel.handle(
        untyped,
        json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"requestId": 3})),
    );
    assert!(sent(&mut channel).is_empty());
    assert!(events(&mut channel).is_empty(), "ignored without a report");
    channel.tick(start + DEAD_AFTER);
    assert!(!channel.is_ended());

    let elsewhere = untyped + Duration::from_secs(10);
    channel.handle(
        elsewhere,
        json_message(PLATFORM, "sender-7", NS_HEARTBEAT, json!({"type": "PING"})),
    );
    channel.tick(untyped + DEAD_AFTER);
    assert!(!channel.is_ended());
    channel.tick(elsewhere + DEAD_AFTER - Duration::from_millis(1));
    assert!(!channel.is_ended());
    channel.tick(elsewhere + DEAD_AFTER);
    assert!(channel.is_ended());
}

/// A device `PING` at each second up to `until`, ticking the channel, so
/// only the launch timer can end anything.
fn keep_alive(channel: &mut Channel, from: Instant, until: Instant) {
    let mut now = from;
    while now < until {
        now = (now + Duration::from_secs(1)).min(until);
        channel.handle(
            now,
            json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"type": "PING"})),
        );
        channel.tick(now);
    }
}

fn launch_failures(channel: &mut Channel) -> Vec<LaunchError> {
    events(channel)
        .into_iter()
        .filter_map(|event| match event {
            Event::LaunchFailed(error) => Some(error),
            _ => None,
        })
        .collect()
}

// covers: C41
#[test]
fn the_retry_after_cancelled_gets_its_own_fifteen_seconds() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.launch(start).expect("ready");
    sent(&mut channel);
    let cancelled = start + Duration::from_secs(10);
    keep_alive(&mut channel, start, cancelled);
    channel.handle(
        cancelled,
        json_message(
            PLATFORM,
            SENDER,
            NS_RECEIVER,
            json!({"type": "LAUNCH_ERROR", "requestId": 101, "reason": "CANCELLED"}),
        ),
    );
    assert!(sent(&mut channel).iter().any(|m| m.2["type"] == "LAUNCH"));
    keep_alive(
        &mut channel,
        cancelled,
        cancelled + LAUNCH_TIMEOUT - Duration::from_millis(1),
    );
    assert!(
        launch_failures(&mut channel).is_empty(),
        "the first LAUNCH's 15 s don't bound the retry"
    );
    channel.tick(cancelled + LAUNCH_TIMEOUT);
    assert_eq!(launch_failures(&mut channel), [LaunchError::Timeout]);
}

// covers: C38, C41
#[test]
fn an_unasked_status_listing_the_app_completes_the_launch() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.launch(start).expect("ready");
    sent(&mut channel);
    // Without a transportId the app can't be addressed yet.
    channel.handle(
        start,
        status(
            0,
            vec![json!({"appId": DEFAULT_MEDIA_RECEIVER, "sessionId": "s-1"})],
        ),
    );
    assert!(sent(&mut channel).is_empty());
    assert!(
        !events(&mut channel)
            .iter()
            .any(|event| matches!(event, Event::Launched { .. }))
    );
    channel.handle(
        start,
        status(0, vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")]),
    );
    assert_eq!(
        sent(&mut channel),
        vec![(
            "t-1".to_owned(),
            NS_CONNECTION.to_owned(),
            json!({"type": "CONNECT"})
        )]
    );
    assert!(events(&mut channel).iter().any(|event| matches!(
        event,
        Event::Launched { app, reused: false } if app.session_id == "s-1" && app.transport_id == "t-1"
    )));
    keep_alive(&mut channel, start, start + LAUNCH_TIMEOUT);
    assert!(
        launch_failures(&mut channel).is_empty(),
        "the launch is over"
    );
}

// covers: C33
#[test]
fn only_the_status_that_answers_get_status_makes_the_channel_ready() {
    let start = Instant::now();
    let mut channel = Channel::open(start, RequestIds::starting_at(7));
    sent(&mut channel);
    channel.handle(start, status(0, Vec::new()));
    channel.handle(start, status(8, Vec::new()));
    assert!(!channel.is_ready());
    assert_eq!(channel.launch(start), Err(LaunchError::NotReady));
    channel.handle(start, status(7, Vec::new()));
    assert!(channel.is_ready());
}

// covers: C43
#[test]
fn another_session_of_the_default_media_receiver_is_a_take_over() {
    let start = Instant::now();
    let mut channel = running(start);
    channel.handle(
        start,
        status(0, vec![app(DEFAULT_MEDIA_RECEIVER, "s-2", "t-2")]),
    );
    assert!(channel.is_ended());
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::TakenOver)));
    assert!(sent(&mut channel).is_empty(), "no relaunch");
}

// covers: C82
#[test]
fn a_close_from_the_app_ends_as_taken_over_when_the_status_lacks_it() {
    let start = Instant::now();
    let mut channel = running(start);
    channel.handle(
        start,
        json_message("t-9", SENDER, NS_CONNECTION, json!({"type": "CLOSE"})),
    );
    assert!(
        sent(&mut channel).is_empty(),
        "a stranger's CLOSE is ignored"
    );
    assert!(!channel.is_ended());

    channel.handle(
        start,
        json_message("t-1", SENDER, NS_CONNECTION, json!({"type": "CLOSE"})),
    );
    let out = sent(&mut channel);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, PLATFORM);
    assert_eq!(out[0].2["type"], "GET_STATUS");
    let id = out[0].2["requestId"].as_u64().expect("id") as u32;
    channel.handle(start, status(id, Vec::new()));
    assert!(channel.is_ended());
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::TakenOver)));
    assert!(sent(&mut channel).is_empty(), "no CONNECT, no relaunch");
}

// covers: C87
#[test]
fn a_stop_while_the_launch_waits_sends_only_the_platform_close() {
    let start = Instant::now();
    let mut channel = ready(start, Vec::new());
    channel.launch(start).expect("ready");
    sent(&mut channel);
    channel.stop(start);
    assert_eq!(
        sent(&mut channel),
        vec![(
            PLATFORM.to_owned(),
            NS_CONNECTION.to_owned(),
            json!({"type": "CLOSE"})
        )]
    );
    assert!(channel.is_ended());
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::Stopped)));
    // The app that launch starts is left running; its late status is
    // ignored.
    channel.handle(
        start + Duration::from_secs(1),
        status(101, vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")]),
    );
    assert!(sent(&mut channel).is_empty());
    assert!(events(&mut channel).is_empty());
}

// covers: C40, C87
#[test]
fn a_reused_app_is_the_senders_to_stop() {
    let start = Instant::now();
    let mut channel = ready(start, vec![app(DEFAULT_MEDIA_RECEIVER, "s-5", "t-5")]);
    channel.launch(start).expect("ready");
    sent(&mut channel);
    channel.stop(start);
    let out = sent(&mut channel);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, PLATFORM);
    assert_eq!(out[0].2["type"], "STOP");
    assert_eq!(out[0].2["sessionId"], "s-5");
}

// covers: C27, C94
#[test]
fn multizone_messages_reach_the_host_only_on_a_group_channel() {
    let start = Instant::now();
    let members = || {
        json_message(
            PLATFORM,
            SENDER,
            NS_MULTIZONE,
            json!({"type": "MULTIZONE_STATUS", "requestId": 0, "status": {"devices": []}}),
        )
    };
    let mut single = ready(start, Vec::new());
    single.handle(start, members());
    assert_eq!(
        events(&mut single),
        vec![Event::Dropped(Dropped::Namespace(NS_MULTIZONE.to_owned()))]
    );

    let mut group = ready(start, Vec::new());
    group.request_members();
    let out = sent(&mut group);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, PLATFORM);
    assert_eq!(out[0].1, NS_MULTIZONE);
    assert_eq!(out[0].2["type"], "GET_STATUS");
    group.handle(start, members());
    assert!(
        events(&mut group)
            .iter()
            .any(|event| matches!(event, Event::Multizone(_)))
    );
    // No reply within 5 s is not an error.
    let mut silent = ready(start, Vec::new());
    silent.request_members();
    sent(&mut silent);
    silent.handle(
        start + STATUS_TIMEOUT,
        json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"type": "PONG"})),
    );
    silent.tick(start + STATUS_TIMEOUT * 2);
    assert!(!silent.is_ended());
    assert!(
        !events(&mut silent)
            .iter()
            .any(|event| matches!(event, Event::Ended(_) | Event::LaunchFailed(_)))
    );
}
