//! The CASTV2 channel to one device as a sans-I/O state machine (sections 3
//! and 4): messages and the time go in, messages to send and events come
//! out. [`crate::client`] runs it over TLS.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::error::{EndReason, LaunchError};
use crate::group::MultizoneMessage;
use crate::ids::RequestIds;
use crate::media::{
    IdleReason, LoadRequest, MAX_CONTENT_ID, MediaError, MediaStatus, PlayerState, VolumeError,
    device_text,
};
use crate::message::{
    self, BROADCAST, CastMessage, DEFAULT_MEDIA_RECEIVER, Namespace, PLATFORM, PayloadError, SENDER,
};
use crate::status::{AppSession, ControlType, ReceiverStatus};

/// How long the first `RECEIVER_STATUS` may take (C33).
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// The sender's `PING` interval (C34).
pub const PING_INTERVAL: Duration = Duration::from_secs(5);
/// Silence that marks the channel dead (C36).
pub const DEAD_AFTER: Duration = Duration::from_secs(15);
/// How long a launch may take (C41).
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
/// How long each teardown step waits for its reply (C87).
pub const STOP_STEP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the `LOAD` reply may take, and then `PLAYING` (C49).
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// The shortest gap between two `SET_VOLUME` levels while dragging (C70).
pub const VOLUME_INTERVAL: Duration = Duration::from_millis(100);

/// What the channel reports to its driver.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The first `RECEIVER_STATUS` arrived; a launch may start (C33).
    Ready(ReceiverStatus),
    /// Any `RECEIVER_STATUS`, asked for or not: the current state (C38).
    Status(ReceiverStatus),
    /// The Default Media Receiver runs and its virtual connection is open
    /// (C40 to C42). `reused` when it was already running.
    Launched {
        app: AppSession,
        reused: bool,
    },
    LaunchFailed(LaunchError),
    /// The app answered `LOAD` with this media session (C50).
    Loaded {
        media_session_id: i64,
    },
    /// The first `PLAYING` after a `LOAD` (C49).
    Playing {
        media_session_id: i64,
    },
    /// A `LOAD` attempt failed (C49, C83, C84).
    LoadFailed(MediaError),
    /// A `MEDIA_STATUS` entry of the sender's media (C51, C78).
    MediaStatus(MediaStatus),
    /// `BUFFERING` after `PLAYING`: the receiver ran dry (C51, C79).
    Rebuffering,
    /// `PLAYING` again after [`Event::Rebuffering`].
    Resumed,
    /// Playback that had started is over (C86).
    PlaybackEnded(PlaybackEnd),
    /// The app's virtual connection closed while the app still runs; the
    /// sender opened it again, which it does once (C82).
    AppReconnected,
    /// A message on the multizone namespace of a group (C94, C96).
    Multizone(MultizoneMessage),
    /// A message the sender dropped, for the log (C26 to C28, C31).
    Dropped(Dropped),
    /// The channel is over; nothing more will be sent (sections 4 and 8).
    Ended(EndReason),
}

/// How playback that had started ended (C86).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackEnd {
    /// `IDLE` with `INTERRUPTED` or `CANCELLED`, or another media session
    /// or content: someone else took over, and the channel ends as taken
    /// over (C43).
    Replaced,
    /// `IDLE` with `ERROR`, or with no reason the sender knows.
    Error(Option<IdleReason>),
    /// `IDLE` with `FINISHED`: the device saw the stream end.
    Finished,
    /// The media `GET_STATUS` after a reconnect got an empty list or no
    /// answer in time: the media ended while the channel was down (C81).
    Gone,
}

/// Why an inbound message was dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// For a destination other than `sender-0` or `*` (C26).
    Destination(String),
    /// On a namespace the sender doesn't use (C27).
    Namespace(String),
    /// On the device authentication namespace, which this version ignores
    /// (C31).
    DeviceAuth,
    /// Not a JSON object (C28).
    Payload(PayloadError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    AwaitingStatus,
    Ready,
    Launching { retried: bool },
    Running,
    Stopping,
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    FirstStatus,
    Launch,
    /// A platform status asked for after the app sent `CLOSE` (C82).
    AppCheck,
    Stop,
    /// `LOAD`, until its reply (C49).
    Load,
    /// The wait for `PLAYING` after the `LOAD` reply (C49); never matched
    /// by id.
    Playing,
    /// Media `STOP`, the first teardown step (C87).
    MediaStop,
    /// Media `GET_STATUS` after a reconnect (C81).
    MediaStatus,
}

impl Request {
    fn name(self) -> &'static str {
        match self {
            Request::Launch => "LAUNCH",
            Request::Load | Request::Playing => "LOAD",
            Request::Stop | Request::MediaStop => "STOP",
            Request::FirstStatus | Request::AppCheck | Request::MediaStatus => "GET_STATUS",
        }
    }
}

/// The sender's media on the app (C50).
#[derive(Clone, Debug)]
struct Media {
    media_session_id: Option<i64>,
    content_id: String,
    /// The first `PLAYING` arrived.
    played: bool,
    rebuffering: bool,
}

#[derive(Debug)]
struct Pending {
    id: u32,
    request: Request,
    deadline: Instant,
}

/// The channel to one device.
#[derive(Debug)]
pub struct Channel {
    ids: RequestIds,
    phase: Phase,
    status: Option<ReceiverStatus>,
    app: Option<AppSession>,
    app_reconnected: bool,
    last_inbound: Instant,
    next_ping: Instant,
    pending: Vec<Pending>,
    outbox: VecDeque<CastMessage>,
    events: VecDeque<Event>,
    media: Option<Media>,
    volume_sent: Option<Instant>,
    /// The level a drag asked for inside [`VOLUME_INTERVAL`]; the latest
    /// wins (C70).
    volume_queued: Option<f64>,
    /// The level last sent, until the next `RECEIVER_STATUS` reports one.
    volume_unconfirmed: Option<f64>,
    /// The member list was asked for: the channel is a group's, and the
    /// multizone namespace is read rather than dropped (C27, C94).
    multizone: bool,
}

impl Channel {
    /// A channel whose TLS handshake just finished at `now`: it sends
    /// `CONNECT` and `GET_STATUS` to the platform (C32, C33).
    pub fn open(now: Instant, ids: RequestIds) -> Channel {
        let mut channel = Channel {
            ids,
            phase: Phase::AwaitingStatus,
            status: None,
            app: None,
            app_reconnected: false,
            last_inbound: now,
            next_ping: now + PING_INTERVAL,
            pending: Vec::new(),
            outbox: VecDeque::new(),
            events: VecDeque::new(),
            media: None,
            volume_sent: None,
            volume_queued: None,
            volume_unconfirmed: None,
            multizone: false,
        };
        channel.connect(PLATFORM);
        channel.request(
            now,
            Request::FirstStatus,
            STATUS_TIMEOUT,
            PLATFORM,
            Namespace::Receiver,
            json!({"type": "GET_STATUS"}),
        );
        channel
    }

    /// Asks a group's platform for its members (C94). Nothing waits for the
    /// answer: without one the member list stays unknown.
    pub fn request_members(&mut self) {
        self.multizone = true;
        self.send_request(
            PLATFORM,
            Namespace::Multizone,
            json!({"type": "GET_STATUS"}),
        );
    }

    /// The next message to send, in order.
    pub fn poll_outbound(&mut self) -> Option<CastMessage> {
        self.outbox.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// The latest platform status.
    pub fn status(&self) -> Option<&ReceiverStatus> {
        self.status.as_ref()
    }

    /// The app the sender launched or reused.
    pub fn app(&self) -> Option<&AppSession> {
        self.app.as_ref()
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.phase, Phase::Ready | Phase::Running)
    }

    pub fn is_ended(&self) -> bool {
        self.phase == Phase::Ended
    }

    /// The media session of the latest `LOAD`, once answered (C50).
    pub fn media_session_id(&self) -> Option<i64> {
        self.media.as_ref().and_then(|media| media.media_session_id)
    }

    /// Whether a `LOAD` is outstanding or waits for `PLAYING` (C49).
    pub fn is_loading(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(pending.request, Request::Load | Request::Playing))
    }

    /// Ends the channel from outside, for a socket error or a framing
    /// fault seen by the driver.
    pub fn end(&mut self, reason: EndReason) {
        if self.phase == Phase::Ended {
            return;
        }
        let launching = matches!(self.phase, Phase::Launching { .. });
        let loading = self.is_loading();
        self.phase = Phase::Ended;
        self.pending.clear();
        self.outbox.clear();
        if launching {
            self.events
                .push_back(Event::LaunchFailed(LaunchError::Ended(reason.clone())));
        }
        if loading {
            self.events
                .push_back(Event::LoadFailed(MediaError::Ended(reason.clone())));
        }
        self.events.push_back(Event::Ended(reason));
    }

    /// Starts the Default Media Receiver, or reuses it when the status
    /// already lists it (C40, C41).
    pub fn launch(&mut self, now: Instant) -> Result<(), LaunchError> {
        if self.phase != Phase::Ready {
            return Err(LaunchError::NotReady);
        }
        let running = self
            .status
            .as_ref()
            .and_then(ReceiverStatus::default_media_receiver)
            .and_then(|app| app.session());
        if let Some(app) = running {
            self.attach(app, true);
            return Ok(());
        }
        self.send_launch(now, false);
        Ok(())
    }

    /// After a reconnect (C81): when the status still lists the Default
    /// Media Receiver with `app`'s `sessionId`, opens its virtual connection
    /// again, asks for the media status and carries on with
    /// `media_session_id`. False when the app is gone and has to be
    /// launched and loaded again.
    pub fn resume(
        &mut self,
        now: Instant,
        app: &AppSession,
        media_session_id: Option<i64>,
        content_id: &str,
    ) -> bool {
        if self.phase != Phase::Ready {
            return false;
        }
        let running = self
            .status
            .as_ref()
            .and_then(ReceiverStatus::default_media_receiver)
            .and_then(|found| found.session())
            .filter(|found| found.session_id == app.session_id);
        let Some(found) = running else {
            return false;
        };
        self.attach(found.clone(), true);
        if let Some(id) = media_session_id {
            self.media = Some(Media {
                media_session_id: Some(id),
                content_id: content_id.to_owned(),
                played: true,
                rebuffering: false,
            });
            self.request(
                now,
                Request::MediaStatus,
                STATUS_TIMEOUT,
                &found.transport_id,
                Namespace::Media,
                json!({"type": "GET_STATUS", "mediaSessionId": id}),
            );
        }
        true
    }

    /// Sends `LOAD` to the app (C45 to C49). One at a time: a `LOAD` waiting
    /// for its reply or for `PLAYING` refuses another.
    pub fn load(&mut self, now: Instant, request: &LoadRequest) -> Result<(), MediaError> {
        let app = match (&self.app, self.phase) {
            (Some(app), Phase::Running) => app.clone(),
            _ => return Err(MediaError::NotRunning),
        };
        if request.url.len() > MAX_CONTENT_ID {
            return Err(MediaError::UrlTooLong);
        }
        if self.is_loading() {
            return Err(MediaError::LoadPending);
        }
        // An answer to the GET_STATUS of a resume says nothing about this
        // load.
        self.pending
            .retain(|pending| pending.request != Request::MediaStatus);
        self.media = Some(Media {
            media_session_id: None,
            content_id: request.url.clone(),
            played: false,
            rebuffering: false,
        });
        self.request(
            now,
            Request::Load,
            LOAD_TIMEOUT,
            &app.transport_id,
            Namespace::Media,
            request.payload(),
        );
        Ok(())
    }

    /// Sets the device volume level, clamped to 0.0 to 1.0 (C65, C67). While
    /// a drag sends faster than [`VOLUME_INTERVAL`], the latest level waits
    /// for the interval to pass (C70).
    pub fn set_volume(&mut self, now: Instant, level: f64) -> Result<(), VolumeError> {
        self.volume_check()?;
        if level.is_nan() {
            return Err(VolumeError::Invalid);
        }
        let level = level.clamp(0.0, 1.0);
        match self.volume_sent {
            Some(sent) if now < sent + VOLUME_INTERVAL => self.volume_queued = Some(level),
            _ => self.send_level(now, level),
        }
        Ok(())
    }

    /// Mutes or unmutes the device, apart from its level (C65).
    pub fn set_muted(&mut self, muted: bool) -> Result<(), VolumeError> {
        self.volume_check()?;
        self.send_request(
            PLATFORM,
            Namespace::Receiver,
            json!({"type": "SET_VOLUME", "volume": {"muted": muted}}),
        );
        Ok(())
    }

    /// Moves the level one `stepInterval` up or down (C69); returns the
    /// level asked for.
    pub fn step_volume(&mut self, now: Instant, up: bool) -> Result<f64, VolumeError> {
        let status = self.volume_check()?;
        let step = status
            .volume
            .step_interval
            .filter(|step| step.is_finite() && *step > 0.0)
            .ok_or(VolumeError::NoStep)?;
        // Steps pressed faster than the device reports build on each other.
        let base = self
            .volume_queued
            .or(self.volume_unconfirmed)
            .unwrap_or(status.volume.level);
        let level = if up { base + step } else { base - step }.clamp(0.0, 1.0);
        self.set_volume(now, level)?;
        Ok(level)
    }

    fn volume_check(&self) -> Result<&ReceiverStatus, VolumeError> {
        if !matches!(self.phase, Phase::Ready | Phase::Running) {
            return Err(VolumeError::NotReady);
        }
        let status = self.status.as_ref().ok_or(VolumeError::NotReady)?;
        if status.volume.control_type == ControlType::Fixed {
            return Err(VolumeError::Fixed);
        }
        Ok(status)
    }

    fn send_level(&mut self, now: Instant, level: f64) {
        self.volume_sent = Some(now);
        self.volume_queued = None;
        self.volume_unconfirmed = Some(level);
        self.send_request(
            PLATFORM,
            Namespace::Receiver,
            json!({"type": "SET_VOLUME", "volume": {"level": level}}),
        );
    }

    /// Tears the session down (C44, C87): media `STOP` for the sender's
    /// media, receiver `STOP` for the sender's own app while the status
    /// still lists it, then `CLOSE` to the app and the platform. Each step
    /// waits at most [`STOP_STEP_TIMEOUT`].
    pub fn stop(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Ended | Phase::Stopping) {
            return;
        }
        let media_stop = self
            .app
            .as_ref()
            .filter(|app| {
                self.status
                    .as_ref()
                    .is_some_and(|status| status.has_session(&app.session_id))
            })
            .zip(self.media_session_id())
            .map(|(app, id)| (app.transport_id.clone(), id));
        self.pending.clear();
        self.volume_queued = None;
        self.phase = Phase::Stopping;
        self.media = None;
        match media_stop {
            Some((transport_id, id)) => self.request(
                now,
                Request::MediaStop,
                STOP_STEP_TIMEOUT,
                &transport_id,
                Namespace::Media,
                json!({"type": "STOP", "mediaSessionId": id}),
            ),
            None => self.stop_app(now),
        }
    }

    fn stop_app(&mut self, now: Instant) {
        let ours = self
            .app
            .as_ref()
            .filter(|app| {
                self.status
                    .as_ref()
                    .is_some_and(|status| status.has_session(&app.session_id))
            })
            .map(|app| app.session_id.clone());
        self.pending.clear();
        match ours {
            Some(session_id) => self.request(
                now,
                Request::Stop,
                STOP_STEP_TIMEOUT,
                PLATFORM,
                Namespace::Receiver,
                json!({"type": "STOP", "sessionId": session_id}),
            ),
            None => self.finish_stop(),
        }
    }

    /// One inbound message, received at `now`.
    pub fn handle(&mut self, now: Instant, message: CastMessage) {
        if self.phase == Phase::Ended {
            return;
        }
        self.last_inbound = now;
        if message.destination_id != SENDER && message.destination_id != BROADCAST {
            self.drop_message(Dropped::Destination(message.destination_id));
            return;
        }
        let namespace = match Namespace::from_name(&message.namespace) {
            Some(Namespace::DeviceAuth) => return self.drop_message(Dropped::DeviceAuth),
            Some(Namespace::Multizone) if !self.multizone => {
                return self.drop_message(Dropped::Namespace(message.namespace));
            }
            Some(namespace) => namespace,
            None => return self.drop_message(Dropped::Namespace(message.namespace)),
        };
        let payload = match message.json_payload() {
            Ok(payload) => payload,
            Err(error) => return self.drop_message(Dropped::Payload(error)),
        };
        let Some(kind) = message::message_type(&payload) else {
            return;
        };
        let id = message::request_id(&payload);
        match (namespace, kind) {
            (Namespace::Heartbeat, "PING") => {
                let pong = CastMessage::json(
                    &message.source_id,
                    Namespace::Heartbeat,
                    &json!({"type": "PONG"}),
                );
                self.outbox.push_back(pong);
            }
            (Namespace::Connection, "CLOSE") => self.on_close(now, &message.source_id),
            (Namespace::Multizone, kind) => {
                let message = MultizoneMessage::from_payload(kind, &payload);
                self.events.push_back(Event::Multizone(message));
            }
            (Namespace::Receiver, "RECEIVER_STATUS") => {
                self.on_status(id, ReceiverStatus::from_payload(&payload));
            }
            (Namespace::Receiver, "LAUNCH_ERROR" | "INVALID_REQUEST") => {
                let reason = device_text(
                    payload
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(kind),
                );
                self.on_launch_error(now, id, reason);
            }
            (Namespace::Media, "MEDIA_STATUS") if self.is_from_app(&message.source_id) => {
                self.on_media_status(now, id, MediaStatus::list_from_payload(&payload));
            }
            (
                Namespace::Media,
                "LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_PLAYER_STATE" | "INVALID_REQUEST",
            ) if self.is_from_app(&message.source_id) => {
                self.on_media_error(now, id, kind, &payload);
            }
            _ => {}
        }
    }

    /// Advances timers to `now`: the heartbeat, the dead-channel check and
    /// request timeouts (C29, C34, C36).
    pub fn tick(&mut self, now: Instant) {
        if self.phase == Phase::Ended {
            return;
        }
        if now.saturating_duration_since(self.last_inbound) >= DEAD_AFTER {
            self.end(EndReason::Silent);
            return;
        }
        if let (Some(level), Some(sent)) = (self.volume_queued, self.volume_sent)
            && now >= sent + VOLUME_INTERVAL
            && matches!(self.phase, Phase::Ready | Phase::Running)
        {
            self.send_level(now, level);
        }
        if now >= self.next_ping {
            self.outbox.push_back(CastMessage::json(
                PLATFORM,
                Namespace::Heartbeat,
                &json!({"type": "PING"}),
            ));
            while self.next_ping <= now {
                self.next_ping += PING_INTERVAL;
            }
        }
        let expired: Vec<Request> = self
            .pending
            .iter()
            .filter(|pending| now >= pending.deadline)
            .map(|pending| pending.request)
            .collect();
        self.pending.retain(|pending| now < pending.deadline);
        for request in expired {
            match request {
                Request::FirstStatus => self.end(EndReason::NoStatus),
                Request::Launch => {
                    self.phase = Phase::Ready;
                    self.events
                        .push_back(Event::LaunchFailed(LaunchError::Timeout));
                }
                Request::AppCheck => self.end(EndReason::AppClosed),
                Request::Stop => self.finish_stop(),
                Request::Load | Request::Playing => {
                    self.pending.retain(|pending| {
                        !matches!(pending.request, Request::Load | Request::Playing)
                    });
                    self.media = None;
                    self.events
                        .push_back(Event::LoadFailed(MediaError::Timeout));
                }
                Request::MediaStop => self.stop_app(now),
                Request::MediaStatus if self.media.is_some() => {
                    self.playback_ended(PlaybackEnd::Gone)
                }
                Request::MediaStatus => {}
            }
        }
    }

    /// The next moment [`Channel::tick`] has something to do.
    pub fn next_deadline(&self) -> Instant {
        let mut next = self.next_ping.min(self.last_inbound + DEAD_AFTER);
        for pending in &self.pending {
            next = next.min(pending.deadline);
        }
        if let (Some(_), Some(sent)) = (self.volume_queued, self.volume_sent) {
            next = next.min(sent + VOLUME_INTERVAL);
        }
        next
    }

    fn is_from_app(&self, source: &str) -> bool {
        self.app
            .as_ref()
            .is_some_and(|app| app.transport_id == source)
    }

    fn on_media_status(&mut self, now: Instant, id: u32, statuses: Vec<MediaStatus>) {
        let answered = match self
            .pending
            .iter()
            .find(|pending| id != 0 && pending.id == id)
        {
            Some(pending)
                if matches!(
                    pending.request,
                    Request::Load | Request::MediaStop | Request::MediaStatus
                ) =>
            {
                self.take_pending(id)
            }
            _ => None,
        };
        if self.phase == Phase::Stopping {
            if answered == Some(Request::MediaStop) {
                self.stop_app(now);
            }
            return;
        }
        if self.phase != Phase::Running {
            return;
        }
        let Some(media) = self.media.as_mut() else {
            return;
        };
        if answered == Some(Request::MediaStatus) && statuses.is_empty() {
            return self.playback_ended(PlaybackEnd::Gone);
        }
        if answered == Some(Request::Load) {
            // A reply without a media session leaves it to the first status
            // that names one.
            media.media_session_id = statuses.iter().find_map(|status| status.media_session_id);
            if let Some(media_session_id) = media.media_session_id {
                self.events.push_back(Event::Loaded { media_session_id });
            }
            self.pending.push(Pending {
                id: 0,
                request: Request::Playing,
                deadline: now + LOAD_TIMEOUT,
            });
        }
        for status in statuses {
            if self.phase != Phase::Running {
                return;
            }
            self.on_media_entry(status);
        }
    }

    fn on_media_entry(&mut self, status: MediaStatus) {
        let awaiting_reply = self.pending_is(Request::Load);
        let Some(media) = self.media.as_mut() else {
            return;
        };
        if media.media_session_id.is_none() && !awaiting_reply {
            media.media_session_id = status.media_session_id;
            if let Some(media_session_id) = status.media_session_id {
                self.events.push_back(Event::Loaded { media_session_id });
            }
        }
        let other_session = status.media_session_id.is_some()
            && media.media_session_id.is_some()
            && status.media_session_id != media.media_session_id;
        let other_content = status
            .content_id
            .as_ref()
            .is_some_and(|content| *content != media.content_id);
        if !media.played {
            // Before PLAYING, the status of what played before may still
            // arrive; only the sender's own media session counts.
            if other_session || other_content || media.media_session_id.is_none() {
                return;
            }
        } else if other_session || other_content {
            self.events.push_back(Event::MediaStatus(status));
            self.playback_ended(PlaybackEnd::Replaced);
            return;
        }
        let media_session_id = media.media_session_id.unwrap_or_default();
        let played = media.played;
        let state = status.player_state.clone();
        let reason = status.idle_reason.clone();
        match state {
            PlayerState::Playing if !played => {
                media.played = true;
                self.events.push_back(Event::MediaStatus(status));
                self.pending
                    .retain(|pending| pending.request != Request::Playing);
                self.events.push_back(Event::Playing { media_session_id });
            }
            PlayerState::Playing => {
                let resumed = std::mem::take(&mut media.rebuffering);
                self.events.push_back(Event::MediaStatus(status));
                if resumed {
                    self.events.push_back(Event::Resumed);
                }
            }
            PlayerState::Buffering => {
                let rebuffer = played && !media.rebuffering;
                media.rebuffering |= rebuffer;
                self.events.push_back(Event::MediaStatus(status));
                if rebuffer {
                    self.events.push_back(Event::Rebuffering);
                }
            }
            PlayerState::Idle if !played => {
                self.events.push_back(Event::MediaStatus(status));
                self.pending
                    .retain(|pending| !matches!(pending.request, Request::Load | Request::Playing));
                self.media = None;
                let reason = reason.unwrap_or(IdleReason::Other("IDLE".to_owned()));
                self.events
                    .push_back(Event::LoadFailed(MediaError::Idle(reason)));
            }
            PlayerState::Idle => {
                self.events.push_back(Event::MediaStatus(status));
                let end = match reason {
                    Some(IdleReason::Interrupted | IdleReason::Cancelled) => PlaybackEnd::Replaced,
                    Some(IdleReason::Finished) => PlaybackEnd::Finished,
                    other => PlaybackEnd::Error(other),
                };
                self.playback_ended(end);
            }
            PlayerState::Paused | PlayerState::Other(_) => {
                self.events.push_back(Event::MediaStatus(status));
            }
        }
    }

    fn playback_ended(&mut self, end: PlaybackEnd) {
        self.media = None;
        let replaced = end == PlaybackEnd::Replaced;
        self.events.push_back(Event::PlaybackEnded(end));
        if replaced {
            self.end(EndReason::TakenOver);
        }
    }

    fn on_media_error(
        &mut self,
        now: Instant,
        id: u32,
        kind: &str,
        payload: &serde_json::Map<String, serde_json::Value>,
    ) {
        let Some(request) = self.take_pending(id) else {
            return;
        };
        let error = match kind {
            "LOAD_FAILED" => MediaError::LoadFailed {
                detailed_error_code: payload
                    .get("detailedErrorCode")
                    .and_then(serde_json::Value::as_i64),
            },
            "LOAD_CANCELLED" => MediaError::LoadCancelled,
            "INVALID_PLAYER_STATE" => MediaError::InvalidPlayerState,
            _ => MediaError::InvalidRequest {
                reason: device_text(
                    payload
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(kind),
                ),
                request: request.name(),
            },
        };
        match request {
            Request::Load => {
                self.pending
                    .retain(|pending| pending.request != Request::Playing);
                self.media = None;
                self.events.push_back(Event::LoadFailed(error));
            }
            Request::MediaStop => self.stop_app(now),
            _ => {}
        }
    }

    fn pending_is(&self, request: Request) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.request == request)
    }

    fn on_status(&mut self, id: u32, status: ReceiverStatus) {
        // A status that answers LAUNCH may not list the app yet; the launch
        // stays pending until one does, or until its timeout.
        let answered = match self
            .pending
            .iter()
            .find(|pending| id != 0 && pending.id == id)
        {
            Some(pending) if pending.request == Request::Launch => None,
            Some(_) => self.take_pending(id),
            None => None,
        };
        if status.volume.control_type == ControlType::Fixed {
            // A drag value still waiting for its interval must not reach a
            // control that turned fixed meanwhile (C68).
            self.volume_queued = None;
        }
        self.status = Some(status.clone());
        self.volume_unconfirmed = None;
        self.events.push_back(Event::Status(status.clone()));
        match self.phase {
            Phase::AwaitingStatus => {
                if answered == Some(Request::FirstStatus) {
                    self.phase = Phase::Ready;
                    self.events.push_back(Event::Ready(status));
                }
            }
            Phase::Launching { .. } => {
                if let Some(app) = status
                    .default_media_receiver()
                    .and_then(|app| app.session())
                {
                    self.pending
                        .retain(|pending| pending.request != Request::Launch);
                    self.attach(app, false);
                }
            }
            Phase::Running => {
                let Some(app) = self.app.clone() else {
                    return;
                };
                if !status.has_session(&app.session_id) {
                    self.end(EndReason::TakenOver);
                } else if answered == Some(Request::AppCheck) {
                    self.app_reconnected = true;
                    self.connect(&app.transport_id);
                    self.events.push_back(Event::AppReconnected);
                }
            }
            Phase::Stopping => {
                if answered == Some(Request::Stop) {
                    self.finish_stop();
                }
            }
            Phase::Ready | Phase::Ended => {}
        }
    }

    fn on_launch_error(&mut self, now: Instant, id: u32, reason: String) {
        let Phase::Launching { retried } = self.phase else {
            return;
        };
        let ours = self
            .pending
            .iter()
            .any(|pending| pending.request == Request::Launch && pending.id == id);
        if !ours {
            return;
        }
        self.take_pending(id);
        if reason == "CANCELLED" && !retried {
            self.send_launch(now, true);
            return;
        }
        self.phase = Phase::Ready;
        self.events
            .push_back(Event::LaunchFailed(LaunchError::Refused { reason }));
    }

    fn on_close(&mut self, now: Instant, source: &str) {
        if self.phase == Phase::Stopping {
            return;
        }
        if source == PLATFORM {
            self.end(EndReason::ClosedByDevice);
            return;
        }
        let from_app = self
            .app
            .as_ref()
            .is_some_and(|app| app.transport_id == source);
        if !from_app || self.phase != Phase::Running {
            return;
        }
        if self.app_reconnected {
            self.end(EndReason::AppClosed);
            return;
        }
        self.request(
            now,
            Request::AppCheck,
            STATUS_TIMEOUT,
            PLATFORM,
            Namespace::Receiver,
            json!({"type": "GET_STATUS"}),
        );
    }

    fn send_launch(&mut self, now: Instant, retried: bool) {
        self.phase = Phase::Launching { retried };
        self.request(
            now,
            Request::Launch,
            LAUNCH_TIMEOUT,
            PLATFORM,
            Namespace::Receiver,
            json!({"type": "LAUNCH", "appId": DEFAULT_MEDIA_RECEIVER}),
        );
    }

    fn attach(&mut self, app: AppSession, reused: bool) {
        self.connect(&app.transport_id);
        self.app = Some(app.clone());
        self.app_reconnected = false;
        self.phase = Phase::Running;
        self.events.push_back(Event::Launched { app, reused });
    }

    fn finish_stop(&mut self) {
        if let Some(app) = self.app.take() {
            self.close(&app.transport_id);
        }
        self.close(PLATFORM);
        // The CLOSE messages stay in the outbox for the driver to flush.
        let outbox = std::mem::take(&mut self.outbox);
        self.end(EndReason::Stopped);
        self.outbox = outbox;
    }

    fn connect(&mut self, destination: &str) {
        self.outbox.push_back(CastMessage::json(
            destination,
            Namespace::Connection,
            &json!({"type": "CONNECT"}),
        ));
    }

    fn close(&mut self, destination: &str) {
        self.outbox.push_back(CastMessage::json(
            destination,
            Namespace::Connection,
            &json!({"type": "CLOSE"}),
        ));
    }

    /// Sends a request whose reply nothing waits for; returns its id.
    fn send_request(
        &mut self,
        destination: &str,
        namespace: Namespace,
        mut payload: serde_json::Value,
    ) -> u32 {
        let id = self.ids.next_id();
        payload["requestId"] = json!(id);
        self.outbox
            .push_back(CastMessage::json(destination, namespace, &payload));
        id
    }

    fn request(
        &mut self,
        now: Instant,
        request: Request,
        timeout: Duration,
        destination: &str,
        namespace: Namespace,
        payload: serde_json::Value,
    ) {
        let id = self.send_request(destination, namespace, payload);
        self.pending.push(Pending {
            id,
            request,
            deadline: now + timeout,
        });
    }

    fn take_pending(&mut self, id: u32) -> Option<Request> {
        if id == 0 {
            return None;
        }
        let index = self.pending.iter().position(|pending| pending.id == id)?;
        Some(self.pending.remove(index).request)
    }

    fn drop_message(&mut self, reason: Dropped) {
        self.events.push_back(Event::Dropped(reason));
    }
}
