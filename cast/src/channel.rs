//! The CASTV2 channel to one device as a sans-I/O state machine (sections 3
//! and 4): messages and the time go in, messages to send and events come
//! out. [`crate::client`] runs it over TLS.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::error::{EndReason, LaunchError};
use crate::ids::RequestIds;
use crate::message::{
    self, BROADCAST, CastMessage, DEFAULT_MEDIA_RECEIVER, Namespace, PLATFORM, PayloadError, SENDER,
};
use crate::status::{AppSession, ReceiverStatus};

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
    /// The app's virtual connection closed while the app still runs; the
    /// sender opened it again, which it does once (C82).
    AppReconnected,
    /// A message the sender dropped, for the log (C26 to C28, C31).
    Dropped(Dropped),
    /// The channel is over; nothing more will be sent (sections 4 and 8).
    Ended(EndReason),
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

    /// Ends the channel from outside, for a socket error or a framing
    /// fault seen by the driver.
    pub fn end(&mut self, reason: EndReason) {
        if self.phase == Phase::Ended {
            return;
        }
        let launching = matches!(self.phase, Phase::Launching { .. });
        self.phase = Phase::Ended;
        self.pending.clear();
        self.outbox.clear();
        if launching {
            self.events
                .push_back(Event::LaunchFailed(LaunchError::Ended(reason.clone())));
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

    /// Tears the session down (C44, C87): receiver `STOP` for the sender's
    /// own app while the status still lists it, then `CLOSE` to the app and
    /// the platform. Each step waits at most [`STOP_STEP_TIMEOUT`].
    pub fn stop(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Ended | Phase::Stopping) {
            return;
        }
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
        self.phase = Phase::Stopping;
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
            (Namespace::Receiver, "RECEIVER_STATUS") => {
                self.on_status(id, ReceiverStatus::from_payload(&payload));
            }
            (Namespace::Receiver, "LAUNCH_ERROR" | "INVALID_REQUEST") => {
                let reason = payload
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(kind)
                    .to_owned();
                self.on_launch_error(now, id, reason);
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
            }
        }
    }

    /// The next moment [`Channel::tick`] has something to do.
    pub fn next_deadline(&self) -> Instant {
        let mut next = self.next_ping.min(self.last_inbound + DEAD_AFTER);
        for pending in &self.pending {
            next = next.min(pending.deadline);
        }
        next
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
        self.status = Some(status.clone());
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

    fn request(
        &mut self,
        now: Instant,
        request: Request,
        timeout: Duration,
        destination: &str,
        namespace: Namespace,
        mut payload: serde_json::Value,
    ) {
        let id = self.ids.next_id();
        payload["requestId"] = json!(id);
        self.outbox
            .push_back(CastMessage::json(destination, namespace, &payload));
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
