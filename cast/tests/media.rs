//! The media and volume parts of the channel on a test clock (sections 5,
//! 6 and 8): `LOAD`, `MEDIA_STATUS`, error replies, `SET_VOLUME`.

mod common;

use std::time::{Duration, Instant};

use audiosync_cast::channel::{
    Channel, Event, LOAD_TIMEOUT, PlaybackEnd, STOP_STEP_TIMEOUT, VOLUME_INTERVAL,
};
use audiosync_cast::error::EndReason;
use audiosync_cast::ids::RequestIds;
use audiosync_cast::media::{
    IdleReason, LoadRequest, MAX_DEVICE_TEXT_CHARS, MediaError, Metadata, VolumeError,
};
use audiosync_cast::message::{
    CastMessage, DEFAULT_MEDIA_RECEIVER, NS_CONNECTION, NS_HEARTBEAT, NS_MEDIA, NS_RECEIVER,
    PLATFORM, SENDER,
};
use common::{app, json_message, media_status};
use serde_json::{Value, json};

const URL: &str = "http://192.0.2.10:45123/0123456789abcdef0123456789abcdef.wav";

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

fn status(request_id: u32, apps: Vec<Value>, volume: Value) -> CastMessage {
    json_message(
        PLATFORM,
        SENDER,
        NS_RECEIVER,
        json!({"type": "RECEIVER_STATUS", "requestId": request_id, "status": {"applications": apps, "volume": volume}}),
    )
}

/// Something from the device at `at`, so the dead-channel timer (C36)
/// stays out of a test about other timers.
fn alive(channel: &mut Channel, at: Instant) {
    channel.handle(
        at,
        json_message(PLATFORM, SENDER, NS_HEARTBEAT, json!({"type": "PONG"})),
    );
}

fn volume() -> Value {
    json!({"level": 0.5, "muted": false, "controlType": "attenuation", "stepInterval": 0.1})
}

fn running(start: Instant, volume: Value) -> Channel {
    let mut channel = Channel::open(start, RequestIds::starting_at(100));
    channel.handle(start, status(100, Vec::new(), volume.clone()));
    channel.launch(start).expect("ready");
    channel.handle(
        start,
        status(101, vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")], volume),
    );
    sent(&mut channel);
    events(&mut channel);
    channel
}

fn request(metadata: Metadata) -> LoadRequest {
    LoadRequest {
        url: URL.to_owned(),
        content_type: "audio/wav".to_owned(),
        metadata,
    }
}

/// A channel whose `LOAD` (request id 102) was answered with media session
/// 7 and then played.
fn playing(start: Instant) -> Channel {
    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    sent(&mut channel);
    channel.handle(start, media_status("t-1", 102, 7, "BUFFERING", None));
    channel.handle(start, media_status("t-1", 0, 7, "PLAYING", None));
    events(&mut channel);
    channel
}

// covers: C1, C5, C45, C46, C47, C48, C53
#[test]
fn load_carries_the_url_in_content_id_live_with_music_metadata_and_nothing_else() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    let metadata = Metadata {
        title: Some("Track".to_owned()),
        artist: None,
        album_name: Some("  ".to_owned()),
        images: vec!["http://192.0.2.10:45123/art.png".to_owned()],
    };
    channel.load(start, &request(metadata)).expect("loads");
    let out = sent(&mut channel);
    assert_eq!(out.len(), 1);
    let (destination, namespace, payload) = &out[0];
    assert_eq!(destination, "t-1", "to the app's transportId");
    assert_eq!(namespace, NS_MEDIA);
    assert_eq!(payload["type"], "LOAD");
    assert_eq!(payload["requestId"], 102);
    assert_eq!(payload["autoplay"], true);
    assert!(payload.get("currentTime").is_none());
    let media = payload["media"].as_object().expect("media");
    let mut keys: Vec<&str> = media.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "contentId",
            "contentType",
            "duration",
            "metadata",
            "streamType"
        ],
        "no contentUrl, no tracks, no DRM or custom data"
    );
    assert_eq!(media["contentId"], URL);
    assert_eq!(media["contentType"], "audio/wav", "audio only");
    assert_eq!(media["streamType"], "LIVE");
    assert_eq!(media["duration"], -1);
    assert_eq!(
        media["metadata"],
        json!({"metadataType": 3, "title": "Track", "images": [{"url": "http://192.0.2.10:45123/art.png"}]}),
        "unknown and blank fields are left out"
    );
    assert_eq!(Metadata::default().to_json(), json!({"metadataType": 3}));

    let mut long = request(Metadata::default());
    long.url = format!("http://192.0.2.10:1/{}", "a".repeat(1024));
    let mut other = running(start, volume());
    assert_eq!(other.load(start, &long), Err(MediaError::UrlTooLong));
}

// covers: C49, C50, C51, C52, C66
#[test]
fn one_load_at_a_time_its_media_session_and_rebuffers_are_tracked() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    let mut wire = Vec::new();
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    wire.extend(sent(&mut channel));
    assert_eq!(
        channel.load(start, &request(Metadata::default())),
        Err(MediaError::LoadPending),
        "never two LOADs outstanding"
    );
    channel.handle(start, media_status("t-1", 102, 7, "BUFFERING", None));
    assert!(events(&mut channel).contains(&Event::Loaded {
        media_session_id: 7
    }));
    assert_eq!(channel.media_session_id(), Some(7));
    assert_eq!(
        channel.load(start, &request(Metadata::default())),
        Err(MediaError::LoadPending),
        "still waiting for PLAYING"
    );
    let later = start + Duration::from_secs(3);
    channel.handle(later, media_status("t-1", 0, 7, "PLAYING", None));
    assert!(events(&mut channel).contains(&Event::Playing {
        media_session_id: 7
    }));
    channel.handle(later, media_status("t-1", 0, 7, "BUFFERING", None));
    channel.handle(later, media_status("t-1", 0, 7, "BUFFERING", None));
    let rebuffers = events(&mut channel)
        .into_iter()
        .filter(|event| *event == Event::Rebuffering)
        .count();
    assert_eq!(rebuffers, 1, "one rebuffer until PLAYING again");
    channel.handle(later, media_status("t-1", 0, 7, "PLAYING", None));
    assert!(events(&mut channel).contains(&Event::Resumed));
    // A status from another endpoint is not the app's.
    channel.handle(
        later,
        media_status("t-9", 0, 8, "IDLE", Some("INTERRUPTED")),
    );
    assert!(!channel.is_ended());
    alive(&mut channel, later + LOAD_TIMEOUT);
    channel.tick(later + LOAD_TIMEOUT);
    assert!(
        !events(&mut channel)
            .iter()
            .any(|event| matches!(event, Event::LoadFailed(_))),
        "the PLAYING wait is over once it played"
    );
    channel.stop(later);
    wire.extend(sent(&mut channel));
    assert_eq!(
        wire.last().expect("media STOP").2,
        json!({"type": "STOP", "mediaSessionId": 7, "requestId": 103})
    );
    assert_eq!(
        channel.media_session_id(),
        None,
        "STOP ends the media session"
    );
    for (_, namespace, payload) in &wire {
        let kind = payload["type"].as_str().unwrap_or("");
        assert!(!["PAUSE", "PLAY", "SEEK"].contains(&kind));
        assert!(!(namespace == NS_MEDIA && kind == "VOLUME"));
    }
}

#[test]
fn no_reply_or_no_playing_in_20_s_fails_the_load() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    alive(&mut channel, start + Duration::from_secs(10));
    channel.tick(start + LOAD_TIMEOUT - Duration::from_millis(1));
    assert!(events(&mut channel).is_empty());
    channel.tick(start + LOAD_TIMEOUT);
    assert_eq!(
        events(&mut channel),
        [Event::LoadFailed(MediaError::Timeout)]
    );

    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    let replied = start + Duration::from_secs(5);
    channel.handle(replied, media_status("t-1", 102, 3, "BUFFERING", None));
    events(&mut channel);
    alive(&mut channel, replied + Duration::from_secs(10));
    channel.tick(replied + LOAD_TIMEOUT - Duration::from_millis(1));
    assert!(events(&mut channel).is_empty(), "20 s count from the reply");
    channel.tick(replied + LOAD_TIMEOUT);
    assert_eq!(
        events(&mut channel),
        [Event::LoadFailed(MediaError::Timeout)]
    );
    assert!(
        channel
            .load(replied + LOAD_TIMEOUT, &request(Metadata::default()))
            .is_ok()
    );
}

// covers: C83
#[test]
fn each_media_error_reply_ends_the_load_with_its_own_reason() {
    let start = Instant::now();
    let cases = [
        (
            json!({"type": "LOAD_FAILED", "detailedErrorCode": 104}),
            MediaError::LoadFailed {
                detailed_error_code: Some(104),
            },
        ),
        (json!({"type": "LOAD_CANCELLED"}), MediaError::LoadCancelled),
        (
            json!({"type": "INVALID_PLAYER_STATE"}),
            MediaError::InvalidPlayerState,
        ),
        (
            json!({"type": "INVALID_REQUEST", "reason": "INVALID_COMMAND"}),
            MediaError::InvalidRequest {
                reason: "INVALID_COMMAND".to_owned(),
                request: "LOAD",
            },
        ),
        (
            json!({"type": "INVALID_REQUEST", "reason": "DUPLICATE_REQUESTID"}),
            MediaError::InvalidRequest {
                reason: "DUPLICATE_REQUESTID".to_owned(),
                request: "LOAD",
            },
        ),
    ];
    let mut texts = Vec::new();
    for (mut reply, error) in cases {
        let mut channel = running(start, volume());
        channel
            .load(start, &request(Metadata::default()))
            .expect("loads");
        reply["requestId"] = json!(102);
        channel.handle(start, json_message("t-1", SENDER, NS_MEDIA, reply));
        assert_eq!(events(&mut channel), [Event::LoadFailed(error.clone())]);
        assert!(!channel.is_ended(), "the channel stays for another try");
        assert!(!channel.is_loading());
        texts.push(error.to_string());
    }
    texts.dedup();
    assert_eq!(texts.len(), 5, "each reason reads differently");
    assert!(MediaError::NoFetch.to_string().contains("firewall"));
}

#[test]
fn idle_before_playing_fails_the_load() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    channel.handle(start, media_status("t-1", 102, 4, "BUFFERING", None));
    events(&mut channel);
    channel.handle(start, media_status("t-1", 0, 4, "IDLE", Some("ERROR")));
    assert!(events(&mut channel).contains(&Event::LoadFailed(MediaError::Idle(IdleReason::Error))));
}

#[test]
fn device_reasons_are_cut_short_and_printable() {
    let start = Instant::now();
    let long = format!("BAD\n\u{1b}[2J{}", "\u{e9}".repeat(70_000));
    let expected = format!("BAD??[2J{}", "\u{e9}".repeat(MAX_DEVICE_TEXT_CHARS - 8));

    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    channel.handle(start, media_status("t-1", 102, 4, "BUFFERING", None));
    events(&mut channel);
    channel.handle(start, media_status("t-1", 0, 4, "IDLE", Some(&long)));
    assert!(
        events(&mut channel).contains(&Event::LoadFailed(MediaError::Idle(IdleReason::Other(
            expected.clone()
        ))))
    );

    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    let reply = json!({"type": "INVALID_REQUEST", "reason": long, "requestId": 102});
    channel.handle(start, json_message("t-1", SENDER, NS_MEDIA, reply));
    assert_eq!(
        events(&mut channel),
        [Event::LoadFailed(MediaError::InvalidRequest {
            reason: expected,
            request: "LOAD",
        })]
    );
}

#[test]
fn idle_while_playing_says_why_and_a_replacement_ends_the_channel() {
    let start = Instant::now();
    let replaced = [
        media_status("t-1", 0, 7, "IDLE", Some("INTERRUPTED")),
        media_status("t-1", 0, 7, "IDLE", Some("CANCELLED")),
        media_status("t-1", 0, 8, "PLAYING", None),
        json_message(
            "t-1",
            "*",
            NS_MEDIA,
            json!({"type": "MEDIA_STATUS", "requestId": 0, "status": [{"mediaSessionId": 7, "playerState": "PLAYING", "media": {"contentId": "http://192.0.2.99/other.mp3"}}]}),
        ),
    ];
    for message in replaced {
        let mut channel = playing(start);
        channel.handle(start, message);
        let seen = events(&mut channel);
        assert!(seen.contains(&Event::PlaybackEnded(PlaybackEnd::Replaced)));
        assert!(seen.contains(&Event::Ended(EndReason::TakenOver)));
        assert!(sent(&mut channel).is_empty(), "no reload, no relaunch");
    }
    let mut channel = playing(start);
    channel.handle(start, media_status("t-1", 0, 7, "IDLE", Some("ERROR")));
    assert!(
        events(&mut channel).contains(&Event::PlaybackEnded(PlaybackEnd::Error(Some(
            IdleReason::Error
        ))))
    );
    assert!(!channel.is_ended());
    let mut channel = playing(start);
    channel.handle(start, media_status("t-1", 0, 7, "IDLE", Some("FINISHED")));
    assert!(events(&mut channel).contains(&Event::PlaybackEnded(PlaybackEnd::Finished)));
}

// covers: C65, C67, C68
#[test]
fn set_volume_goes_to_the_platform_clamped_and_mute_on_its_own() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    channel.set_volume(start, 1.7).expect("sent");
    let later = start + VOLUME_INTERVAL;
    channel.set_volume(later, -0.3).expect("sent");
    channel.set_muted(true).expect("sent");
    let out = sent(&mut channel);
    assert_eq!(out.len(), 3);
    for (destination, namespace, _) in &out {
        assert_eq!(destination, PLATFORM);
        assert_eq!(namespace, NS_RECEIVER);
    }
    assert_eq!(out[0].2["type"], "SET_VOLUME");
    assert_eq!(out[0].2["volume"], json!({"level": 1.0}));
    assert_eq!(out[1].2["volume"], json!({"level": 0.0}));
    assert_eq!(out[2].2["volume"], json!({"muted": true}));
    assert!(
        out.iter()
            .all(|(_, _, payload)| payload["requestId"].as_u64() > Some(0))
    );
    assert_eq!(
        channel.set_volume(later + VOLUME_INTERVAL, f64::NAN),
        Err(VolumeError::Invalid)
    );

    let fixed = json!({"level": 1.0, "controlType": "fixed"});
    let mut channel = running(start, fixed);
    assert_eq!(channel.set_volume(start, 0.3), Err(VolumeError::Fixed));
    assert_eq!(channel.set_muted(true), Err(VolumeError::Fixed));
    assert_eq!(channel.step_volume(start, true), Err(VolumeError::Fixed));
    assert!(sent(&mut channel).is_empty(), "fixed sends nothing");
    let master = json!({"level": 0.2, "controlType": "master"});
    let mut channel = running(start, master);
    assert!(channel.set_volume(start, 0.3).is_ok());
}

// covers: C69
#[test]
fn steps_move_by_the_step_interval_from_the_reported_level() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    let level = channel.step_volume(start, true).expect("step");
    assert!((level - 0.6).abs() < 1e-9);
    let level = channel.step_volume(start, true).expect("step");
    assert!(
        (level - 0.7).abs() < 1e-9,
        "a quick second step builds on the first"
    );
    channel.tick(start + VOLUME_INTERVAL);
    let out = sent(&mut channel);
    assert_eq!(out.len(), 2);
    let last_id = out[1].2["requestId"].as_u64().expect("id");
    channel.handle(
        start,
        status(
            u32::try_from(last_id).expect("id"),
            vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")],
            json!({"level": 0.65, "stepInterval": 0.1}),
        ),
    );
    let level = channel
        .step_volume(start + VOLUME_INTERVAL * 2, false)
        .expect("step");
    assert!((level - 0.55).abs() < 1e-9, "the latest status is the base");
    let no_step = json!({"level": 0.5});
    let mut channel = running(start, no_step);
    assert_eq!(channel.step_volume(start, true), Err(VolumeError::NoStep));
}

// covers: C69
#[test]
fn a_step_before_any_reported_level_builds_only_on_a_level_sent() {
    let start = Instant::now();
    let no_level = json!({"controlType": "attenuation", "stepInterval": 0.05});
    let mut channel = running(start, no_level);
    assert_eq!(
        channel.step_volume(start, false),
        Err(VolumeError::NotReady),
        "no level to step from"
    );
    assert!(sent(&mut channel).is_empty(), "no level sent");
    channel.set_volume(start, 0.3).expect("sent");
    let level = channel
        .step_volume(start + VOLUME_INTERVAL, true)
        .expect("step");
    assert!((level - 0.35).abs() < 1e-9, "builds on the 0.3 sent: {level}");
}

// covers: C69, C70, C104
#[test]
fn a_level_equal_to_the_reported_one_is_not_awaited() {
    let start = Instant::now();
    let top = json!({"level": 1.0, "controlType": "attenuation", "stepInterval": 0.05});
    let mut channel = running(start, top);
    assert_eq!(channel.step_volume(start, true), Ok(1.0));
    sent(&mut channel);
    channel.handle(
        start,
        status(
            0,
            vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")],
            json!({"level": 0.3, "controlType": "attenuation", "stepInterval": 0.05}),
        ),
    );
    let level = channel
        .step_volume(start + VOLUME_INTERVAL, false)
        .expect("step");
    assert!(
        (level - 0.25).abs() < 1e-9,
        "builds on the reported 0.3: {level}"
    );
}

// covers: C69, C70, C104
#[test]
fn a_level_the_device_rounds_onto_the_reported_one_is_not_awaited() {
    let start = Instant::now();
    let reported = json!({"level": 0.7, "controlType": "attenuation", "stepInterval": 0.1});
    let mut channel = running(start, reported);
    channel.set_volume(start, 0.72).expect("sent");
    sent(&mut channel);
    channel.handle(
        start,
        status(
            0,
            vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")],
            json!({"level": 0.3, "controlType": "attenuation", "stepInterval": 0.1}),
        ),
    );
    let level = channel
        .step_volume(start + VOLUME_INTERVAL, false)
        .expect("step");
    assert!(
        (level - 0.2).abs() < 1e-9,
        "builds on the reported 0.3: {level}"
    );
}

// covers: C69, C70
#[test]
fn a_rounded_answer_without_the_request_id_answers_the_level() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    channel.set_volume(start, 0.73).expect("sent");
    sent(&mut channel);
    channel.handle(
        start,
        status(
            0,
            vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")],
            json!({"level": 0.7, "controlType": "attenuation", "stepInterval": 0.1}),
        ),
    );
    let level = channel
        .step_volume(start + VOLUME_INTERVAL, false)
        .expect("step");
    assert!(
        (level - 0.6).abs() < 1e-9,
        "builds on the reported 0.7: {level}"
    );
}

// covers: C70
#[test]
fn a_drag_sends_at_most_one_level_per_100_ms_and_status_changes_are_not_echoed() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    channel.set_volume(start, 0.1).expect("sent");
    channel
        .set_volume(start + Duration::from_millis(10), 0.2)
        .expect("queued");
    channel
        .set_volume(start + Duration::from_millis(20), 0.3)
        .expect("queued");
    assert_eq!(sent(&mut channel).len(), 1);
    assert_eq!(channel.next_deadline(), start + VOLUME_INTERVAL);
    channel.tick(start + Duration::from_millis(99));
    assert!(sent(&mut channel).is_empty());
    channel.tick(start + VOLUME_INTERVAL);
    let out = sent(&mut channel);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].2["volume"], json!({"level": 0.3}), "the latest wins");

    channel.handle(
        start + Duration::from_secs(1),
        status(
            0,
            vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")],
            json!({"level": 0.9, "muted": true}),
        ),
    );
    channel.tick(start + Duration::from_secs(1));
    assert!(sent(&mut channel).is_empty(), "nothing sent back");
    let reported = events(&mut channel)
        .into_iter()
        .find_map(|event| match event {
            Event::Status(status) => Some(status.volume),
            _ => None,
        });
    let reported = reported.expect("the status reaches the host");
    assert_eq!(reported.level, 0.9);
    assert!(reported.muted);
}

// covers: C68, C70
#[test]
fn a_drag_queued_before_the_control_turns_fixed_is_never_sent() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    channel.set_volume(start, 0.1).expect("sent");
    channel
        .set_volume(start + Duration::from_millis(10), 0.3)
        .expect("queued");
    assert_eq!(sent(&mut channel).len(), 1);
    channel.handle(
        start + Duration::from_millis(50),
        status(
            0,
            vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")],
            json!({"level": 1.0, "controlType": "fixed"}),
        ),
    );
    channel.tick(start + VOLUME_INTERVAL);
    let volumes: Vec<Value> = sent(&mut channel)
        .into_iter()
        .filter(|(_, _, payload)| payload["type"] == "SET_VOLUME")
        .map(|(_, _, payload)| payload)
        .collect();
    assert!(volumes.is_empty(), "fixed sends nothing: {volumes:?}");
    channel.handle(
        start + Duration::from_millis(150),
        status(0, vec![app(DEFAULT_MEDIA_RECEIVER, "s-1", "t-1")], volume()),
    );
    channel.tick(start + VOLUME_INTERVAL * 3);
    assert!(
        sent(&mut channel)
            .iter()
            .all(|(_, _, payload)| payload["type"] != "SET_VOLUME"),
        "the dropped drag value stays dropped"
    );
}

// covers: C87
#[test]
fn a_silent_media_stop_moves_on_to_the_receiver_stop_after_two_seconds() {
    let start = Instant::now();
    let mut channel = playing(start);
    channel.stop(start);
    let out = sent(&mut channel);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, "t-1");
    assert_eq!(out[0].2["type"], "STOP");
    assert_eq!(out[0].2["mediaSessionId"], 7);
    let media_stop_id = out[0].2["requestId"]
        .as_u64()
        .expect("media STOP has a requestId");
    channel.tick(start + STOP_STEP_TIMEOUT - Duration::from_millis(1));
    assert!(sent(&mut channel).is_empty(), "the media STOP still waits");
    let receiver_stop_at = start + STOP_STEP_TIMEOUT;
    channel.tick(receiver_stop_at);
    let out = sent(&mut channel);
    assert_eq!(out.len(), 1);
    let receiver_stop_id = out[0].2["requestId"]
        .as_u64()
        .expect("receiver STOP has a requestId");
    assert!(receiver_stop_id > 0 && receiver_stop_id != media_stop_id);
    assert_eq!(
        out,
        vec![(
            PLATFORM.to_owned(),
            NS_RECEIVER.to_owned(),
            json!({"type": "STOP", "sessionId": "s-1", "requestId": receiver_stop_id})
        )]
    );
    assert!(!channel.is_ended());
    channel.tick(receiver_stop_at + STOP_STEP_TIMEOUT);
    assert!(channel.is_ended());
    let closes: Vec<String> = sent(&mut channel)
        .into_iter()
        .filter(|(_, namespace, payload)| namespace == NS_CONNECTION && payload["type"] == "CLOSE")
        .map(|(destination, _, _)| destination)
        .collect();
    assert_eq!(closes, ["t-1", PLATFORM]);
    assert!(events(&mut channel).contains(&Event::Ended(EndReason::Stopped)));
}

// covers: C49, C86
#[test]
fn before_playing_a_status_of_an_earlier_media_session_changes_nothing() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    sent(&mut channel);
    channel.handle(start, media_status("t-1", 102, 7, "BUFFERING", None));
    events(&mut channel);
    channel.handle(
        start,
        media_status("t-1", 0, 6, "IDLE", Some("INTERRUPTED")),
    );
    let seen = events(&mut channel);
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            Event::LoadFailed(_) | Event::PlaybackEnded(_) | Event::Ended(_)
        )),
        "{seen:?}"
    );
    assert!(channel.is_loading());
    channel.handle(start, media_status("t-1", 0, 7, "PLAYING", None));
    assert!(events(&mut channel).contains(&Event::Playing {
        media_session_id: 7
    }));
}

// covers: C50
#[test]
fn a_load_reply_without_a_media_session_waits_for_this_loads_own_status() {
    let start = Instant::now();
    let empty_reply = |request_id: u32| {
        json_message(
            "t-1",
            "*",
            NS_MEDIA,
            json!({"type": "MEDIA_STATUS", "requestId": request_id, "status": []}),
        )
    };
    let with_content = |id: i64, content_id: &str, state: &str| {
        json_message(
            "t-1",
            "*",
            NS_MEDIA,
            json!({"type": "MEDIA_STATUS", "requestId": 0, "status": [
                {"mediaSessionId": id, "playerState": state, "media": {"contentId": content_id}}
            ]}),
        )
    };

    // A late status of another URL's media.
    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    sent(&mut channel);
    channel.handle(start, empty_reply(102));
    channel.handle(
        start,
        with_content(1, "http://192.0.2.10:1/other.wav", "IDLE"),
    );
    channel.handle(start, with_content(2, URL, "PLAYING"));
    let seen = events(&mut channel);
    assert!(
        seen.contains(&Event::Loaded {
            media_session_id: 2
        }) && seen.contains(&Event::Playing {
            media_session_id: 2
        }),
        "{seen:?}"
    );
    assert!(!seen.contains(&Event::Loaded {
        media_session_id: 1
    }));

    // A reload of the same URL: a late status of the media it replaces.
    let mut channel = playing(start);
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads again");
    sent(&mut channel);
    channel.handle(start, empty_reply(103));
    channel.handle(start, media_status("t-1", 0, 7, "PLAYING", None));
    channel.handle(start, media_status("t-1", 0, 8, "PLAYING", None));
    let seen = events(&mut channel);
    assert!(
        seen.contains(&Event::Loaded {
            media_session_id: 8
        }) && seen.contains(&Event::Playing {
            media_session_id: 8
        }),
        "{seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PlaybackEnded(_))),
        "{seen:?}"
    );
}

// covers: C29, C83
#[test]
fn an_error_reply_to_a_request_the_sender_never_sent_changes_nothing() {
    let start = Instant::now();
    let mut channel = running(start, volume());
    channel
        .load(start, &request(Metadata::default()))
        .expect("loads");
    sent(&mut channel);
    for id in [0, 55, 103] {
        channel.handle(
            start,
            json_message(
                "t-1",
                SENDER,
                NS_MEDIA,
                json!({"type": "LOAD_FAILED", "requestId": id}),
            ),
        );
    }
    assert!(events(&mut channel).is_empty());
    assert!(channel.is_loading(), "the LOAD still waits for its reply");
    channel.handle(start, media_status("t-1", 102, 7, "BUFFERING", None));
    assert_eq!(channel.media_session_id(), Some(7));
}
