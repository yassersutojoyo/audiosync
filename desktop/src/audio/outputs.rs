//! Which outputs play, at what delay and gain, and in what format, and what
//! kind of device each is.
//!
//! The user's settings are kept per endpoint ID, whether the device is
//! connected or not. The output policy of the core decides which renderers to
//! add and remove (spec S117, S161); this module turns each output's settings
//! into the delay and gain its renderer applies (S3, S71, S176) and picks the
//! format its stream uses.
//!
//! Delays follow the local delay model (S172 to S178). The reference is the
//! listed output of kind BuiltIn with the smallest endpoint ID (S174,
//! [`reference_output`]): it defines time 0 and has no delay setting. Every
//! other output has an offset relative to it, −2000 to +20 000 ms, 0 until
//! the user sets one: negative for a speaker that sounds late, such as a
//! Bluetooth speaker. The outputs taking part (switched on, with their
//! devices listed) play at hold-backs the core computes from their offsets
//! ([`hold_backs`], S176): the earliest plays at once and the others wait for
//! it by the difference. While an AirPlay speaker is chosen (S79), the
//! AirPlay latency L comes on top: the sender's base latency L_s plus the
//! user's adjustment A (S178), so the reference sounds with the AirPlay
//! speakers and every other output by its offset. A switched-on output
//! whose device leaves the list keeps counting for 10 s ([`BriefLosses`],
//! S328), so a brief loss re-times no other output.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use audiosync_core::policy::delay::{
    BriefLosses, Participant, hold_backs, native_airplay_latency_ms,
    sanitize_airplay_adjustment_ms, sanitize_offset_ms,
};
use audiosync_core::policy::session::sync_outputs;
use audiosync_core::reader::delay_ms_to_frames;
use audiosync_core::ring::is_supported_rate;
use audiosync_core::volume::{
    DEFAULT_LOCAL_POSITION, DEFAULT_MASTER_POSITION, NO_ZONE_POSITION, sanitize_position,
    zoned_output_gain,
};

use super::SESSION_RATE;
use super::backend::{BackendError, OutputKind};

/// The L_s a new native sender starts with: the AirPlay delay adjustment A
/// is set against it until a sender has reported one (S178).
pub const DEFAULT_SENDER_LATENCY_MS: f64 = audiosync_airplay::stream::DEFAULT_LATENCY_FRAMES as f64
    * 1000.0
    / audiosync_airplay::audio::SAMPLE_RATE as f64;

/// A connected render endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The endpoint ID.
    pub id: String,
    /// The name Windows shows for it.
    pub name: String,
    /// Whether it is Windows' default output.
    pub is_default: bool,
    /// What kind of device it is.
    pub kind: OutputKind,
    /// What Windows said about it, as read.
    pub traits: DeviceTraits,
}

/// The endpoint properties an output's kind comes from, as read; `None`
/// when Windows couldn't tell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceTraits {
    /// `PKEY_AudioEndpoint_FormFactor`.
    pub form_factor: Option<u32>,
    /// `PKEY_Device_EnumeratorName`.
    pub enumerator: Option<String>,
}

impl DeviceTraits {
    pub fn mic_kind(&self) -> MicKind {
        mic_kind(self.form_factor, self.enumerator.as_deref())
    }
}

/// What microphone calibration makes of an output: only external speakers
/// can be heard by a microphone in the room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicKind {
    Speaker,
    /// Headphones, a headset or a handset: they play into the ears.
    Headphones,
    /// A software device: nothing comes out of it into the room.
    Virtual,
}

/// The [`MicKind`] of an output from its form factor and the enumerator of
/// its device. A property that couldn't be read leaves the output in.
///
/// The `ROOT` enumerator marks a device on no hardware bus, which is how
/// virtual cables and meeting apps install their outputs; it decides first,
/// since such a device reports whatever form factor it likes. Then the form
/// factor: headphones, a headset or a handset play into the ears, whatever
/// bus they are on, while a Bluetooth or USB speaker says `Speakers`. The
/// name isn't used: the user can change it and Windows translates it.
pub fn mic_kind(form_factor: Option<u32>, enumerator: Option<&str>) -> MicKind {
    if enumerator.is_some_and(|bus| bus.eq_ignore_ascii_case("ROOT")) {
        return MicKind::Virtual;
    }
    match form_factor {
        Some(form_factor::HEADPHONES | form_factor::HEADSET | form_factor::HANDSET) => {
            MicKind::Headphones
        }
        _ => MicKind::Speaker,
    }
}

/// `EndpointFormFactor` values, as Windows documents them.
pub mod form_factor {
    /// `Speakers`.
    pub const SPEAKERS: u32 = 1;
    /// `LineLevel`: a line out.
    pub const LINE_LEVEL: u32 = 2;
    /// `Headphones`.
    pub const HEADPHONES: u32 = 3;
    /// `Headset`.
    pub const HEADSET: u32 = 5;
    /// `Handset`: a phone's earpiece.
    pub const HANDSET: u32 = 6;
    /// `DigitalAudioDisplayDevice`: a display or TV, over HDMI or
    /// DisplayPort.
    pub const DIGITAL_AUDIO_DISPLAY: u32 = 9;
}

/// The kind of an output, from what Windows says about its endpoint: its form
/// factor (`PKEY_AudioEndpoint_FormFactor`, an `EndpointFormFactor` value),
/// the enumerator of its device (`PKEY_Device_EnumeratorName`: `HDAUDIO` for
/// the PC's own audio, `USB`, `BTHENUM` and other `BTH` names for Bluetooth,
/// `ROOT` for most software devices), and whether its jack is DisplayPort
/// (`PKEY_AudioEndpoint_JackSubType`).
///
/// The enumerator decides first, since a USB or Bluetooth headset reports the
/// same form factor as the PC's headphone jack; a USB device keeps its form
/// (headphones or speakers) beside the bus. Any other enumerator leaves it to
/// the form factor.
pub fn output_kind(form_factor: Option<u32>, enumerator: &str, display_port: bool) -> OutputKind {
    let bus = enumerator.to_ascii_uppercase();
    if bus.starts_with("BTH") {
        return OutputKind::Bluetooth;
    }
    match bus.as_str() {
        "USB" => {
            return match form_factor {
                Some(form_factor::HEADPHONES | form_factor::HEADSET) => OutputKind::UsbHeadphones,
                Some(form_factor::SPEAKERS) => OutputKind::UsbSpeakers,
                _ => OutputKind::Usb,
            };
        }
        "ROOT" => return OutputKind::Virtual,
        _ => {}
    }
    match form_factor {
        Some(form_factor::DIGITAL_AUDIO_DISPLAY) if display_port => OutputKind::DisplayPort,
        Some(form_factor::DIGITAL_AUDIO_DISPLAY) => OutputKind::Hdmi,
        Some(form_factor::HEADPHONES | form_factor::HEADSET) => OutputKind::Headphones,
        Some(form_factor::SPEAKERS | form_factor::LINE_LEVEL) => OutputKind::BuiltIn,
        _ => OutputKind::Other,
    }
}

/// What the listing read about one render endpoint, before deciding whether
/// to list it. A call that failed keeps its error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointReading {
    /// Its position in Windows' collection of endpoints.
    pub index: u32,
    /// Its endpoint ID, or why the endpoint or its ID couldn't be read.
    pub id: Result<String, String>,
    /// Its properties, or why its property store couldn't be opened.
    pub properties: Result<EndpointProperties, String>,
}

/// The properties of an endpoint that the listing uses. One that can't be
/// read is left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndpointProperties {
    /// The name Windows shows (`PKEY_Device_FriendlyName`).
    pub name: Option<String>,
    /// Its form factor (`PKEY_AudioEndpoint_FormFactor`).
    pub form_factor: Option<u32>,
    /// The enumerator of its device (`PKEY_Device_EnumeratorName`).
    pub enumerator: Option<String>,
    /// Whether its jack is DisplayPort (`PKEY_AudioEndpoint_JackSubType`).
    pub display_port: bool,
    /// The channels of the format the audio engine runs it at
    /// (`PKEY_AudioEngine_DeviceFormat`); only input devices use it, to
    /// offer their channel pairs.
    pub channels: Option<u16>,
}

/// Something the listing did with an endpoint that the log should say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingNote {
    /// Left out: a Bluetooth hands-free endpoint.
    HandsFree {
        /// Its endpoint ID.
        id: String,
        /// Its name, if it could be read.
        name: Option<String>,
    },
    /// Left out: an AirPlay speaker the sound system publishes itself
    /// (PipeWire's RAOP sink), which AudioSync reaches through its own
    /// sender.
    #[cfg_attr(
        all(not(target_os = "linux"), not(test)),
        expect(
            dead_code,
            reason = "only PipeWire lists its AirPlay speakers as outputs"
        )
    )]
    NetworkSpeaker {
        /// Its node name.
        id: String,
        /// Its name, if it has one.
        name: Option<String>,
    },
    /// Left out: the endpoint or its ID couldn't be read, so nothing could
    /// play to it.
    NoId {
        /// Its position in Windows' collection.
        index: u32,
        /// Why.
        error: String,
    },
    /// Listed under its ID, as an output of unknown kind: its properties
    /// couldn't be read.
    NoProperties {
        /// Its endpoint ID.
        id: String,
        /// Why.
        error: String,
    },
}

/// The outputs to list, by ID, and what the log should say about the
/// endpoints left out or listed without their properties.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The endpoints to list, sorted by ID.
    pub endpoints: Vec<Endpoint>,
    /// What the log should say, in the order Windows gave the endpoints.
    pub notes: Vec<ListingNote>,
}

impl std::fmt::Display for ListingNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListingNote::HandsFree { id, name } => {
                let name = name.as_deref().unwrap_or("(no name)");
                write!(
                    f,
                    "output {name:?} ({id}) left out: a Bluetooth hands-free endpoint, \
                     for calls only"
                )
            }
            ListingNote::NetworkSpeaker { id, name } => {
                let name = name.as_deref().unwrap_or("(no name)");
                write!(
                    f,
                    "output {name:?} ({id}) left out: an AirPlay speaker the sound system \
                     publishes, which AudioSync plays to through its own sender"
                )
            }
            ListingNote::NoId { index, error } => write!(
                f,
                "render endpoint {index} left out: it couldn't be read ({error})"
            ),
            ListingNote::NoProperties { id, error } => write!(
                f,
                "output {id} listed under its ID, kind unknown: its properties couldn't \
                 be read ({error})"
            ),
        }
    }
}

/// The enumerator name (`PKEY_Device_EnumeratorName`) the endpoints of
/// Windows' Bluetooth Hands-Free Profile audio driver report.
pub const HANDS_FREE_ENUMERATOR: &str = "BTHHFENUM";

/// Whether an endpoint whose device has this enumerator is a Bluetooth
/// hands-free one, which AudioSync doesn't list.
///
/// A Bluetooth headset gets two render endpoints from Windows: a stereo one
/// from its A2DP profile (enumerator `BTHENUM`), and a call one from its
/// Hands-Free Profile (`BTHHFENUM`). The call one carries mono,
/// low-bandwidth audio, and opening it switches the headset to call mode,
/// which interrupts its stereo sound. A phone that uses the PC for its calls
/// gets a hands-free endpoint too.
///
/// The enumerator tells them apart; the form factor can't, since a
/// headset's hands-free endpoint says `Headset` and a phone's says
/// `Speakers`, while a USB or wired headset that plays in stereo says
/// `Headset` as well.
pub fn is_hands_free(enumerator: &str) -> bool {
    enumerator.eq_ignore_ascii_case(HANDS_FREE_ENUMERATOR)
}

/// Turns what was read about each active render endpoint into the outputs to
/// list. `default_id` is the ID of Windows' default output, if there is one.
///
/// Every endpoint is listed except a Bluetooth hands-free one
/// ([`is_hands_free`]) and one without an ID, which nothing could play to.
/// One whose properties can't be read is listed under its ID as an output
/// of unknown kind. The notes say what was left out, or listed without its
/// properties, and why, so nothing goes missing without a word in the log.
pub fn list_endpoints(readings: Vec<EndpointReading>, default_id: Option<&str>) -> Listing {
    let mut listing = Listing::default();
    for reading in readings {
        let id = match reading.id {
            Ok(id) => id,
            Err(error) => {
                listing.notes.push(ListingNote::NoId {
                    index: reading.index,
                    error,
                });
                continue;
            }
        };
        let properties = match reading.properties {
            Ok(properties) => properties,
            Err(error) => {
                listing.notes.push(ListingNote::NoProperties {
                    id: id.clone(),
                    error,
                });
                EndpointProperties::default()
            }
        };
        let enumerator = properties.enumerator.as_deref().unwrap_or_default();
        if is_hands_free(enumerator) {
            listing.notes.push(ListingNote::HandsFree {
                id,
                name: properties.name,
            });
            continue;
        }
        listing.endpoints.push(Endpoint {
            is_default: default_id == Some(id.as_str()),
            kind: output_kind(properties.form_factor, enumerator, properties.display_port),
            traits: DeviceTraits {
                form_factor: properties.form_factor,
                enumerator: properties.enumerator,
            },
            name: properties.name.unwrap_or_else(|| id.clone()),
            id,
        });
    }
    listing.endpoints.sort_by(|a, b| a.id.cmp(&b.id));
    listing
}

/// The reference output among the listed ones (S174): the one of kind
/// BuiltIn, the PC's own audio hardware, with the smallest endpoint ID in
/// byte order, so it stays the same across launches; `None` without one.
/// Windows' default output plays no part, since the user changes it freely.
pub fn reference_output(listed: &[Endpoint]) -> Option<&str> {
    listed
        .iter()
        .filter(|endpoint| endpoint.kind == OutputKind::BuiltIn)
        .map(|endpoint| endpoint.id.as_str())
        .min()
}

/// Whether AirPlay speakers exist to line local outputs up with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AirPlay {
    /// No AirPlay sender runs: outputs line up with each other only.
    Unavailable,
    /// The AirPlay sender runs; `chosen` says whether an AirPlay speaker is
    /// chosen (S79).
    Available {
        /// Whether an AirPlay speaker is chosen.
        chosen: bool,
        /// The sender's base latency L_s (P98, P113).
        latency: Duration,
    },
}

impl AirPlay {
    /// L_s while an AirPlay speaker is chosen, nothing otherwise.
    fn anchor(self) -> Option<Duration> {
        match self {
            AirPlay::Available {
                chosen: true,
                latency,
            } => Some(latency),
            _ => None,
        }
    }

    /// The sender's base latency L_s in ms while a sender runs.
    pub fn sender_latency_ms(self) -> Option<f64> {
        match self {
            AirPlay::Available { latency, .. } => Some(latency.as_secs_f64() * 1000.0),
            AirPlay::Unavailable => None,
        }
    }
}

/// One output's settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputSettings {
    /// Whether the user turned it on.
    pub enabled: bool,
    /// Its offset relative to the reference in ms, −2000 to +20 000 (S175);
    /// kept, but not used, while it is the reference.
    pub delay_ms: f64,
    /// The volume slider position, 0 to 100.
    pub volume: f64,
}

/// What a renderer applies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputParams {
    /// The delay in frames at [`SESSION_RATE`].
    pub delay_frames: i64,
    /// The linear gain: the output's volume times the master volume.
    pub gain: f32,
}

/// The renderers to add and remove.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputPlan {
    /// Enabled, connected endpoints without a renderer, by ID.
    pub attach: Vec<Endpoint>,
    /// IDs with a renderer that are no longer enabled, sorted.
    pub detach: Vec<String>,
}

impl OutputPlan {
    /// Whether nothing needs to change.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.attach.is_empty() && self.detach.is_empty()
    }
}

/// The time one UI poll stands for in [`OutputPolicy::tick`]: the UI polls
/// twice a second.
pub(crate) const POLL_PERIOD: Duration = Duration::from_millis(500);

/// Every output's settings, the master volume, and the decisions made from
/// them.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputPolicy {
    settings: BTreeMap<String, OutputSettings>,
    master: f64,
    /// The zone volume of each endpoint ID in a zone (S249); one not here
    /// is in no zone.
    zones: BTreeMap<String, f64>,
    airplay: AirPlay,
    /// The native sender's adjustment A in ms, −2000 to +20 000 (S178).
    airplay_adjustment_ms: f64,
    /// The L_s the last sender reported, kept after it stops.
    last_sender_latency_ms: Option<f64>,
    /// The endpoint IDs listed now.
    listed: BTreeSet<String>,
    /// The reference among them (S174).
    reference: Option<String>,
    /// The switched-on outputs whose devices left the list a moment ago,
    /// still counted (S328).
    brief_losses: BriefLosses,
    /// The time [`OutputPolicy::tick`] has counted, which the brief losses
    /// are timed by.
    clock: Duration,
    /// The measured offset of the Google Cast speaker that is on (spec
    /// C77); `None` while none is.
    cast_offset: Option<Duration>,
}

impl OutputPolicy {
    /// No output set or listed yet, the master at its default.
    pub fn new(airplay: AirPlay) -> OutputPolicy {
        OutputPolicy {
            settings: BTreeMap::new(),
            master: DEFAULT_MASTER_POSITION,
            zones: BTreeMap::new(),
            airplay,
            airplay_adjustment_ms: 0.0,
            last_sender_latency_ms: airplay.sender_latency_ms(),
            listed: BTreeSet::new(),
            reference: None,
            brief_losses: BriefLosses::default(),
            clock: Duration::ZERO,
            cast_offset: None,
        }
    }

    /// The lead the local outputs play at, as the debug log names it.
    pub fn describe_lead(&self) -> String {
        match self.network_latency_ms() {
            Some(wait) => format!(
                "lead: local outputs wait {} ms for the network speakers",
                wait.round()
            ),
            None => "lead: local outputs play at their device's start lead (logged in ms once \
                     measured)"
                .to_owned(),
        }
    }

    /// Sets the Cast speaker's measured offset, or `None` while none is
    /// on. Returns whether the local outputs' hold-backs change.
    pub fn set_cast_offset(&mut self, offset: Option<Duration>) -> bool {
        let before = self.network_latency_ms();
        self.cast_offset = offset;
        before != self.network_latency_ms()
    }

    /// What local outputs wait for: L while an AirPlay speaker is chosen,
    /// the Cast speaker's offset while one is on, the longer of the two
    /// when both are; the shorter one is then held back to it too.
    fn network_latency_ms(&self) -> Option<f64> {
        match (self.airplay_latency_ms(), self.cast_offset_ms()) {
            (Some(airplay), Some(cast)) => Some(airplay.max(cast)),
            (airplay, cast) => airplay.or(cast),
        }
    }

    /// How much later than L the AirPlay speakers play, so they meet a Cast
    /// speaker whose offset is longer (C77).
    pub fn airplay_hold_back_ms(&self) -> f64 {
        match (self.airplay_latency_ms(), self.cast_offset_ms()) {
            (Some(airplay), Some(cast)) => (cast - airplay).max(0.0),
            _ => 0.0,
        }
    }

    /// How long the Cast feed waits, so a Cast speaker whose offset is
    /// shorter than L plays at L (C77).
    pub fn cast_hold_back_ms(&self) -> f64 {
        match (self.airplay_latency_ms(), self.cast_offset_ms()) {
            (Some(airplay), Some(cast)) => (airplay - cast).max(0.0),
            _ => 0.0,
        }
    }

    fn cast_offset_ms(&self) -> Option<f64> {
        self.cast_offset.map(|offset| offset.as_secs_f64() * 1000.0)
    }

    /// Takes in the endpoints listed now: which outputs can take part, and
    /// which one is the reference (S174).
    pub fn set_listed(&mut self, listed: &[Endpoint]) {
        self.listed = listed.iter().map(|endpoint| endpoint.id.clone()).collect();
        self.reference = reference_output(listed).map(str::to_owned);
        let settings = &self.settings;
        self.brief_losses.note_listing(
            self.listed.iter().map(String::as_str),
            |id| settings.get(id).is_some_and(|settings| settings.enabled),
            self.reference.as_deref(),
            self.clock,
        );
    }

    /// One UI poll went by: the brief losses held [`BRIEF_LOSS_HOLD`] end
    /// (S328). Returns whether any did, so the outputs re-time once.
    ///
    /// [`BRIEF_LOSS_HOLD`]: audiosync_core::policy::delay::BRIEF_LOSS_HOLD
    pub fn tick(&mut self) -> bool {
        self.clock += POLL_PERIOD;
        self.brief_losses.expire(self.clock)
    }

    /// A session (re)starts: it counts only the devices listed now (S328).
    pub fn end_brief_losses(&mut self) {
        self.brief_losses.restart();
    }

    /// Whether output `id`, whose device left the list a moment ago, still
    /// counts toward the others' timing (S328).
    pub fn counts_while_lost(&self, id: &str) -> bool {
        self.brief_losses.holds(id)
    }

    /// The reference output's endpoint ID, if one is listed (S174).
    pub fn reference(&self) -> Option<&str> {
        self.reference.as_deref()
    }

    /// The native sender's adjustment A in ms (S178).
    pub fn airplay_adjustment_ms(&self) -> f64 {
        self.airplay_adjustment_ms
    }

    /// Sets the native sender's adjustment A. Returns false when the value
    /// was ignored because it isn't finite; others are clamped to −2000 to
    /// +20 000 ms (S178).
    pub fn set_airplay_adjustment_ms(&mut self, adjustment_ms: f64) -> bool {
        match sanitize_airplay_adjustment_ms(adjustment_ms) {
            Some(adjustment_ms) => {
                self.airplay_adjustment_ms = adjustment_ms;
                true
            }
            None => false,
        }
    }

    /// The sender's base latency L_s in ms, while a sender runs.
    pub fn sender_latency_ms(&self) -> Option<f64> {
        self.airplay.sender_latency_ms()
    }

    /// The L_s an AirPlay delay the user enters is turned into A against:
    /// the sender's now, else the last one reported, else a new sender's.
    pub fn adjustment_base_ms(&self) -> f64 {
        self.sender_latency_ms()
            .or(self.last_sender_latency_ms)
            .unwrap_or(DEFAULT_SENDER_LATENCY_MS)
    }

    /// L while an AirPlay speaker is chosen: L_s + A, clamped to 0 to
    /// 20 000 ms (S178); `None` otherwise.
    fn airplay_latency_ms(&self) -> Option<f64> {
        self.airplay.anchor().map(|latency| {
            native_airplay_latency_ms(latency.as_secs_f64() * 1000.0, self.airplay_adjustment_ms)
        })
    }

    /// The hold-back of output `id` in ms (S176): among the outputs taking
    /// part (switched on and listed, or lost a moment ago, S328), with `id`
    /// counted among them, so an output about to start gets the hold-back
    /// it will play at.
    pub fn hold_back_ms(&self, id: &str) -> f64 {
        let participant = |id: &str| Participant {
            id: "",
            offset_ms: self.settings(id).delay_ms,
            is_reference: self.reference.as_deref() == Some(id),
        };
        let others = self
            .settings
            .iter()
            .filter(|(other, settings)| {
                settings.enabled
                    && other.as_str() != id
                    && (self.listed.contains(other.as_str()) || self.brief_losses.holds(other))
            })
            .map(|(other, _)| participant(other));
        let mut taking_part = vec![participant(id)];
        taking_part.extend(others);
        hold_backs(&taking_part, self.network_latency_ms())
            .first()
            .map_or(0.0, |(_, held)| *held)
    }

    /// The settings of output `id`: what the user set, or the defaults (off,
    /// no delay, volume 60).
    pub fn settings(&self, id: &str) -> OutputSettings {
        self.settings.get(id).copied().unwrap_or(OutputSettings {
            enabled: false,
            delay_ms: 0.0,
            volume: DEFAULT_LOCAL_POSITION,
        })
    }

    fn entry(&mut self, id: &str) -> &mut OutputSettings {
        let defaults = self.settings(id);
        self.settings.entry(id.to_owned()).or_insert(defaults)
    }

    /// Whether output `id` has settings of its own: it was switched on or
    /// off, or its delay or volume set, at some point (saved ones count).
    pub fn has_settings(&self, id: &str) -> bool {
        self.settings.contains_key(id)
    }

    /// Forgets the settings of output `id`, and returns them: the defaults
    /// when it had none.
    pub fn take_settings(&mut self, id: &str) -> OutputSettings {
        let settings = self.settings(id);
        self.settings.remove(id);
        settings
    }

    /// Turns output `id` on or off.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        self.entry(id).enabled = enabled;
        if !enabled {
            self.brief_losses.switched_off(id);
        }
    }

    /// Sets the offset of output `id` relative to the reference. Returns
    /// false when the value was ignored because it isn't finite; others are
    /// clamped to −2000 to +20 000 ms (S175).
    pub fn set_delay_ms(&mut self, id: &str, delay_ms: f64) -> bool {
        match sanitize_offset_ms(delay_ms) {
            Some(delay_ms) => {
                self.entry(id).delay_ms = delay_ms;
                true
            }
            None => false,
        }
    }

    /// Sets the volume slider position of output `id`. Returns false when the
    /// value was ignored because it isn't finite; others are clamped.
    pub fn set_volume(&mut self, id: &str, position: f64) -> bool {
        match sanitize_position(position) {
            Some(position) => {
                self.entry(id).volume = position;
                true
            }
            None => false,
        }
    }

    /// Sets the master slider position. Returns false when the value was
    /// ignored because it isn't finite; others are clamped.
    pub fn set_master(&mut self, position: f64) -> bool {
        match sanitize_position(position) {
            Some(position) => {
                self.master = position;
                true
            }
            None => false,
        }
    }

    /// The master slider position.
    pub fn master(&self) -> f64 {
        self.master
    }

    /// Sets the zone volume of every endpoint ID in a zone (S249); the rest
    /// are in no zone. Returns the IDs whose zone volume changed.
    pub fn set_zones(&mut self, zones: BTreeMap<String, f64>) -> Vec<String> {
        let zones: BTreeMap<String, f64> = zones
            .into_iter()
            .filter_map(|(id, zone)| sanitize_position(zone).map(|zone| (id, zone)))
            .filter(|(_, zone)| *zone != NO_ZONE_POSITION)
            .collect();
        let changed = self
            .zones
            .keys()
            .chain(zones.keys())
            .filter(|id| self.zones.get(*id) != zones.get(*id))
            .cloned()
            .collect::<BTreeSet<String>>();
        self.zones = zones;
        changed.into_iter().collect()
    }

    /// The zone volume of output `id`: 100 when it is in no zone.
    fn zone(&self, id: &str) -> f64 {
        self.zones.get(id).copied().unwrap_or(NO_ZONE_POSITION)
    }

    /// Changes the AirPlay state. Returns true when the hold-backs may change
    /// because of it, so running renderers need re-timing (S177): an AirPlay
    /// speaker is chosen or no longer is, or L_s changed while one is.
    pub fn set_airplay(&mut self, airplay: AirPlay) -> bool {
        let changed = airplay.anchor() != self.airplay.anchor();
        if let Some(latency_ms) = airplay.sender_latency_ms() {
            self.last_sender_latency_ms = Some(latency_ms);
        }
        self.airplay = airplay;
        changed
    }

    /// The IDs of the enabled outputs, sorted.
    pub fn enabled_ids(&self) -> Vec<&str> {
        self.settings
            .iter()
            .filter(|(_, settings)| settings.enabled)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// The delay and gain a renderer for output `id` applies: its hold-back
    /// in frames at the session rate (S3, S176) and the product of its
    /// volume, its zone's and the master volume on the cubic curve (S70,
    /// S71, S249).
    pub fn params(&self, id: &str) -> OutputParams {
        let settings = self.settings(id);
        let delay_ms = self.hold_back_ms(id);
        OutputParams {
            delay_frames: delay_ms_to_frames(delay_ms, SESSION_RATE).unwrap_or(0),
            gain: zoned_output_gain(settings.volume, self.zone(id), self.master) as f32,
        }
    }

    /// The renderers to add and remove, given the connected endpoints
    /// `present` and the endpoint IDs that have a renderer (`active`), with
    /// the core's output sync (S117, S161): while `running`, enabled and
    /// connected outputs without a renderer are added; outputs that are no
    /// longer enabled are removed. An enabled output that isn't connected
    /// waits until it is.
    pub fn plan(&self, running: bool, present: &[Endpoint], active: &[&str]) -> OutputPlan {
        let present_ids: Vec<&str> = present
            .iter()
            .map(|endpoint| endpoint.id.as_str())
            .collect();
        let sync = sync_outputs(running, &self.enabled_ids(), &present_ids, active);
        let attach = sync
            .add
            .iter()
            .filter_map(|id| present.iter().find(|endpoint| endpoint.id == *id).cloned())
            .collect();
        OutputPlan {
            attach,
            detach: sync.remove,
        }
    }
}

/// An endpoint's mix format, the format the Windows audio engine mixes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixFormat {
    /// Whether its samples are 32-bit float.
    pub float32: bool,
    /// Frames per second.
    pub rate: u32,
    /// Channels per frame.
    pub channels: u16,
    /// Which speaker each channel feeds (`dwChannelMask`), 0 when not given.
    pub channel_mask: u32,
}

/// The format an output stream renders in: always 32-bit float, at the
/// endpoint's own rate and channel layout, so Windows doesn't resample; the
/// render pipeline converts from the session rate (S62).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderFormat {
    /// Frames per second.
    pub rate: u32,
    /// Channels per frame.
    pub channels: u16,
    /// Which speaker each channel feeds.
    pub channel_mask: u32,
    /// Whether the mix format isn't float, so the stream asks Windows to
    /// convert the float samples (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`);
    /// otherwise the mix format is used as it is.
    pub convert: bool,
}

/// How an output's stream was set up, for the debug log: its format and the
/// size of its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSetup {
    /// The format it renders in.
    pub format: RenderFormat,
    /// The stream buffer, in device frames.
    pub buffer_frames: u32,
}

/// `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT`.
const STEREO_MASK: u32 = 0x3;

/// `SPEAKER_FRONT_CENTER`.
const MONO_MASK: u32 = 0x4;

/// The usual channel mask for `channels` channels: front center for mono,
/// front left and right for stereo, none for other layouts (Windows then
/// assigns speakers in its standard order).
pub fn default_channel_mask(channels: u16) -> u32 {
    match channels {
        1 => MONO_MASK,
        2 => STEREO_MASK,
        _ => 0,
    }
}

/// The render format for an endpoint whose mix format is `mix`.
pub fn render_format(mix: MixFormat) -> Result<RenderFormat, BackendError> {
    if mix.channels == 0 || !is_supported_rate(mix.rate) {
        return Err(BackendError::UnsupportedFormat);
    }
    let channel_mask = if mix.channel_mask == 0 {
        default_channel_mask(mix.channels)
    } else {
        mix.channel_mask
    };
    Ok(RenderFormat {
        rate: mix.rate,
        channels: mix.channels,
        channel_mask,
        convert: !mix.float32,
    })
}

/// 100-nanosecond units per millisecond, the unit of WASAPI durations.
pub const HNS_PER_MS: i64 = 10_000;

/// The smallest render buffer, 20 ms.
pub const MIN_RENDER_BUFFER_HNS: i64 = 20 * HNS_PER_MS;

/// The largest render buffer, 200 ms.
pub const MAX_RENDER_BUFFER_HNS: i64 = 200 * HNS_PER_MS;

/// The buffer an event-driven shared-mode render stream asks for, given the
/// endpoint's default period: two periods, so a render thread that wakes up
/// late still finds the device a period ahead, and at least 20 ms (at most
/// 200 ms).
pub fn render_buffer_hns(default_period_hns: i64) -> i64 {
    default_period_hns
        .saturating_mul(2)
        .clamp(MIN_RENDER_BUFFER_HNS, MAX_RENDER_BUFFER_HNS)
}

/// The frames of one device period at `rate`, rounded up: what each wake of
/// an event-driven shared-mode stream refills when it is on time, which the
/// output's lead counts (S206). A period that isn't positive gives 0 (the
/// lead then goes by the callbacks), and one longer than
/// [`MAX_RENDER_BUFFER_HNS`] counts as that.
pub fn period_frames(default_period_hns: i64, rate: u32) -> usize {
    let hns = default_period_hns.clamp(0, MAX_RENDER_BUFFER_HNS) as u64;
    (hns * u64::from(rate)).div_ceil(1000 * HNS_PER_MS as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiosync_core::reader::DRIFT_TOLERANCE_MS;

    fn endpoint(id: &str) -> Endpoint {
        Endpoint {
            id: id.to_owned(),
            name: format!("Output {id}"),
            is_default: false,
            kind: OutputKind::Other,
            traits: DeviceTraits::default(),
        }
    }

    // covers: S174
    #[test]
    fn the_bus_tells_bluetooth_usb_and_software_devices() {
        let headphones = Some(form_factor::HEADPHONES);
        assert_eq!(
            output_kind(headphones, "BTHENUM", false),
            OutputKind::Bluetooth
        );
        assert_eq!(
            output_kind(headphones, "BthHFEnum", false),
            OutputKind::Bluetooth
        );
        assert_eq!(
            output_kind(Some(form_factor::SPEAKERS), "BTHLEDevice", false),
            OutputKind::Bluetooth
        );
        assert_eq!(
            output_kind(Some(form_factor::HEADSET), "USB", false),
            OutputKind::UsbHeadphones
        );
        assert_eq!(
            output_kind(Some(form_factor::HEADPHONES), "USB", false),
            OutputKind::UsbHeadphones
        );
        assert_eq!(
            output_kind(Some(form_factor::SPEAKERS), "USB", false),
            OutputKind::UsbSpeakers
        );
        assert_eq!(
            output_kind(Some(form_factor::LINE_LEVEL), "USB", false),
            OutputKind::Usb
        );
        assert_eq!(output_kind(None, "USB", false), OutputKind::Usb);
        assert_eq!(
            output_kind(Some(form_factor::SPEAKERS), "ROOT", false),
            OutputKind::Virtual
        );
    }

    #[test]
    fn the_form_factor_tells_the_rest() {
        let kind = |form, bus| output_kind(Some(form), bus, false);
        assert_eq!(kind(form_factor::SPEAKERS, "HDAUDIO"), OutputKind::BuiltIn);
        assert_eq!(
            kind(form_factor::LINE_LEVEL, "HDAUDIO"),
            OutputKind::BuiltIn
        );
        assert_eq!(
            kind(form_factor::HEADPHONES, "HDAUDIO"),
            OutputKind::Headphones
        );
        assert_eq!(
            kind(form_factor::HEADSET, "INTELAUDIO"),
            OutputKind::Headphones
        );
        assert_eq!(
            kind(form_factor::DIGITAL_AUDIO_DISPLAY, "HDAUDIO"),
            OutputKind::Hdmi
        );
        assert_eq!(
            output_kind(Some(form_factor::DIGITAL_AUDIO_DISPLAY), "HDAUDIO", true),
            OutputKind::DisplayPort
        );
        assert_eq!(kind(8, "HDAUDIO"), OutputKind::Other, "S/PDIF");
        assert_eq!(kind(0, "SWD"), OutputKind::Other, "a remote session");
        assert_eq!(output_kind(None, "", false), OutputKind::Other);
    }

    fn reading(index: u32, id: &str, name: &str, form: u32, bus: &str) -> EndpointReading {
        EndpointReading {
            index,
            id: Ok(id.to_owned()),
            properties: Ok(EndpointProperties {
                name: Some(name.to_owned()),
                form_factor: Some(form),
                enumerator: Some(bus.to_owned()),
                display_port: false,
                channels: None,
            }),
        }
    }

    #[test]
    fn bluetooth_hands_free_endpoints_are_left_out_and_noted() {
        let listing = list_endpoints(
            vec![
                reading(0, "b", "Headphones (Example Buds)", 3, "BTHENUM"),
                reading(1, "c", "Headset (Example Buds Hands-Free)", 5, "BTHHFENUM"),
                reading(2, "a", "Speakers (Example Phone HF Audio)", 1, "BthHfEnum"),
            ],
            Some("b"),
        );
        assert_eq!(
            listing.endpoints,
            vec![Endpoint {
                id: "b".to_owned(),
                name: "Headphones (Example Buds)".to_owned(),
                is_default: true,
                kind: OutputKind::Bluetooth,
                traits: DeviceTraits {
                    form_factor: Some(3),
                    enumerator: Some("BTHENUM".to_owned()),
                },
            }],
            "the stereo endpoint stays, whatever the hands-free one's form factor"
        );
        assert_eq!(
            listing.notes,
            vec![
                ListingNote::HandsFree {
                    id: "c".to_owned(),
                    name: Some("Headset (Example Buds Hands-Free)".to_owned()),
                },
                ListingNote::HandsFree {
                    id: "a".to_owned(),
                    name: Some("Speakers (Example Phone HF Audio)".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn headsets_on_other_buses_are_listed() {
        let listing = list_endpoints(
            vec![
                reading(0, "a", "Headset (Example USB Headset)", 5, "USB"),
                reading(1, "b", "Headset Earphone (Example Jack)", 5, "HDAUDIO"),
            ],
            None,
        );
        assert_eq!(ids(&listing.endpoints), ["a", "b"]);
        assert!(listing.notes.is_empty());
    }

    #[test]
    fn an_endpoint_whose_properties_fail_is_listed_under_its_id_and_noted() {
        let listing = list_endpoints(
            vec![EndpointReading {
                index: 0,
                id: Ok("a".to_owned()),
                properties: Err("IMMDevice::OpenPropertyStore: access denied".to_owned()),
            }],
            None,
        );
        assert_eq!(
            listing.endpoints,
            vec![Endpoint {
                id: "a".to_owned(),
                name: "a".to_owned(),
                is_default: false,
                kind: OutputKind::Other,
                traits: DeviceTraits::default(),
            }]
        );
        assert_eq!(
            listing.notes,
            vec![ListingNote::NoProperties {
                id: "a".to_owned(),
                error: "IMMDevice::OpenPropertyStore: access denied".to_owned(),
            }]
        );
    }

    #[test]
    fn an_endpoint_without_an_id_is_left_out_and_noted() {
        let listing = list_endpoints(
            vec![
                EndpointReading {
                    index: 0,
                    id: Err("IMMDevice::GetId: element not found".to_owned()),
                    properties: Ok(EndpointProperties::default()),
                },
                reading(1, "b", "Speakers (Example)", 1, "HDAUDIO"),
            ],
            None,
        );
        assert_eq!(ids(&listing.endpoints), ["b"]);
        assert_eq!(
            listing.notes,
            vec![ListingNote::NoId {
                index: 0,
                error: "IMMDevice::GetId: element not found".to_owned(),
            }]
        );
    }

    #[test]
    fn a_name_that_cannot_be_read_falls_back_to_the_id() {
        let mut nameless = reading(0, "a", "", 1, "HDAUDIO");
        if let Ok(properties) = &mut nameless.properties {
            properties.name = None;
        }
        let listing = list_endpoints(vec![nameless], None);
        assert_eq!(listing.endpoints[0].name, "a");
        assert_eq!(listing.endpoints[0].kind, OutputKind::BuiltIn);
        assert!(listing.notes.is_empty());
    }

    fn endpoint_of(id: &str, kind: OutputKind) -> Endpoint {
        Endpoint {
            id: id.to_owned(),
            name: format!("Output {id}"),
            is_default: false,
            kind,
            traits: DeviceTraits::default(),
        }
    }

    // covers: S174
    #[test]
    fn the_built_in_output_with_the_smallest_endpoint_id_is_the_reference() {
        let mut listed = vec![
            endpoint_of("{0.0.0.00000000}.{b2}", OutputKind::BuiltIn),
            endpoint_of("{0.0.0.00000000}.{0f}", OutputKind::Usb),
            endpoint_of("{0.0.0.00000000}.{a7}", OutputKind::BuiltIn),
        ];
        assert_eq!(reference_output(&listed), Some("{0.0.0.00000000}.{a7}"));
        // Windows' default output plays no part: the user changes it freely.
        listed[0].is_default = true;
        listed[1].is_default = true;
        assert_eq!(reference_output(&listed), Some("{0.0.0.00000000}.{a7}"));
        // Byte order: an uppercase letter sorts before a lowercase one.
        listed.push(endpoint_of("{0.0.0.00000000}.{B9}", OutputKind::BuiltIn));
        assert_eq!(reference_output(&listed), Some("{0.0.0.00000000}.{B9}"));
    }

    // covers: S174
    #[test]
    fn headphones_on_the_pc_jack_are_not_the_reference() {
        let listed = [
            endpoint_of("a", OutputKind::Headphones),
            endpoint_of("b", OutputKind::BuiltIn),
        ];
        assert_eq!(reference_output(&listed), Some("b"));
        assert_eq!(
            reference_output(&[endpoint_of("a", OutputKind::Headphones)]),
            None
        );
    }

    // covers: S174
    #[test]
    fn no_built_in_output_means_no_reference() {
        let listed: Vec<Endpoint> = [
            OutputKind::Bluetooth,
            OutputKind::Usb,
            OutputKind::Hdmi,
            OutputKind::DisplayPort,
            OutputKind::Virtual,
            OutputKind::Other,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| endpoint_of(&index.to_string(), kind))
        .collect();
        assert_eq!(reference_output(&listed), None);
        assert_eq!(reference_output(&[]), None);
    }

    /// The PC's own speakers (the reference), a USB speaker at +150 ms and a
    /// Bluetooth speaker at −250 ms, all listed; `enabled` are on.
    fn local_delay_policy(airplay: AirPlay, enabled: &[&str]) -> OutputPolicy {
        let mut policy = OutputPolicy::new(airplay);
        policy.set_listed(&[
            endpoint_of("builtin", OutputKind::BuiltIn),
            endpoint_of("usb", OutputKind::Usb),
            endpoint_of("bluetooth", OutputKind::Bluetooth),
        ]);
        policy.set_delay_ms("usb", 150.0);
        policy.set_delay_ms("bluetooth", -250.0);
        for id in enabled {
            policy.set_enabled(id, true);
        }
        policy
    }

    /// The hold-back of output `id` in whole ms.
    fn held_ms(policy: &OutputPolicy, id: &str) -> i64 {
        policy.params(id).delay_frames / 48
    }

    #[test]
    fn hold_backs_follow_the_offsets_of_the_outputs_taking_part() {
        let all = ["builtin", "usb", "bluetooth"];
        let policy = local_delay_policy(AirPlay::Unavailable, &all);
        assert_eq!(policy.reference(), Some("builtin"));
        let held: Vec<i64> = all.iter().map(|id| held_ms(&policy, id)).collect();
        assert_eq!(held, [250, 400, 0]);
        let policy = local_delay_policy(AirPlay::Unavailable, &["usb", "bluetooth"]);
        assert_eq!(held_ms(&policy, "usb"), 400, "a switched-off reference");
        let policy = local_delay_policy(sender(true, L), &all);
        let held: Vec<i64> = all.iter().map(|id| held_ms(&policy, id)).collect();
        assert_eq!(held, [1750, 1900, 1500], "L + o′");
        let policy = local_delay_policy(sender(false, L), &all);
        assert_eq!(
            held_ms(&policy, "builtin"),
            250,
            "no AirPlay speaker chosen"
        );
    }

    #[test]
    fn the_airplay_adjustment_moves_l() {
        let all = ["builtin", "usb", "bluetooth"];
        let mut policy = local_delay_policy(sender(true, L), &all);
        assert!(policy.set_airplay_adjustment_ms(250.0));
        assert_eq!(policy.airplay_adjustment_ms(), 250.0);
        assert_eq!(held_ms(&policy, "builtin"), 2000);
        assert!(!policy.set_airplay_adjustment_ms(f64::NAN));
        assert!(policy.set_airplay_adjustment_ms(-9000.0));
        assert_eq!(policy.airplay_adjustment_ms(), -2000.0);
        assert_eq!(
            held_ms(&policy, "builtin"),
            250,
            "L = 0: the Bluetooth speaker sets the base"
        );
    }

    fn ids(endpoints: &[Endpoint]) -> Vec<&str> {
        endpoints
            .iter()
            .map(|endpoint| endpoint.id.as_str())
            .collect()
    }

    /// A policy with `enabled` listed and switched on; none is the
    /// reference.
    fn policy_with(enabled: &[&str]) -> OutputPolicy {
        let mut policy = OutputPolicy::new(AirPlay::Unavailable);
        let listed: Vec<Endpoint> = enabled.iter().map(|id| endpoint(id)).collect();
        policy.set_listed(&listed);
        for id in enabled {
            policy.set_enabled(id, true);
        }
        policy
    }

    // covers: S161
    #[test]
    fn enabled_connected_outputs_play_sorted_by_id() {
        let policy = policy_with(&["c", "a", "b"]);
        let present = [endpoint("b"), endpoint("d"), endpoint("a")];
        let plan = policy.plan(true, &present, &[]);
        assert_eq!(ids(&plan.attach), ["a", "b"]);
        assert!(plan.detach.is_empty());
    }

    // covers: S161
    #[test]
    fn an_output_that_already_plays_is_not_added_again() {
        let policy = policy_with(&["a", "b"]);
        let plan = policy.plan(true, &[endpoint("a"), endpoint("b")], &["a"]);
        assert_eq!(ids(&plan.attach), ["b"]);
    }

    #[test]
    fn nothing_plays_while_stopped() {
        let policy = policy_with(&["a"]);
        let plan = policy.plan(false, &[endpoint("a")], &[]);
        assert!(plan.is_empty());
    }

    // covers: S161
    #[test]
    fn an_output_turned_off_is_removed() {
        let mut policy = policy_with(&["a", "b"]);
        policy.set_enabled("b", false);
        let plan = policy.plan(true, &[endpoint("a"), endpoint("b")], &["a", "b"]);
        assert!(plan.attach.is_empty());
        assert_eq!(plan.detach, ["b"]);
    }

    // covers: S161
    #[test]
    fn an_enabled_output_that_is_not_connected_waits() {
        let policy = policy_with(&["gone"]);
        assert!(policy.plan(true, &[endpoint("other")], &[]).is_empty());
    }

    // covers: S161
    #[test]
    fn a_renderer_whose_device_disappeared_is_kept_until_the_output_is_turned_off() {
        // The renderer stops by itself when its device goes away (S68); the
        // plan only removes renderers of outputs that are turned off.
        let policy = policy_with(&["a"]);
        assert!(policy.plan(true, &[], &["a"]).is_empty());
    }

    // covers: S161
    #[test]
    fn a_device_removed_and_added_again_plays_again() {
        let policy = policy_with(&["a"]);
        let present = [endpoint("a")];
        assert_eq!(ids(&policy.plan(true, &present, &[]).attach), ["a"]);
        // Unplugged: its renderer stopped and the device isn't listed.
        assert!(policy.plan(true, &[], &[]).is_empty());
        // Plugged in again: added again, still enabled.
        assert_eq!(ids(&policy.plan(true, &present, &[]).attach), ["a"]);
        assert!(policy.settings("a").enabled);
    }

    // covers: S71
    #[test]
    fn a_new_output_starts_off_at_volume_60_and_no_delay() {
        let policy = OutputPolicy::new(AirPlay::Unavailable);
        assert_eq!(
            policy.settings("new"),
            OutputSettings {
                enabled: false,
                delay_ms: 0.0,
                volume: 60.0,
            }
        );
    }

    /// The sender's default L: 77 175 frames at 44.1 kHz.
    const L: Duration = Duration::from_millis(1750);

    fn sender(chosen: bool, latency: Duration) -> AirPlay {
        AirPlay::Available { chosen, latency }
    }

    #[test]
    fn the_airplay_delay_base_is_the_last_sender_latency_known() {
        let mut policy = OutputPolicy::new(AirPlay::Unavailable);
        assert_eq!(DEFAULT_SENDER_LATENCY_MS, L.as_secs_f64() * 1000.0);
        assert_eq!(policy.adjustment_base_ms(), DEFAULT_SENDER_LATENCY_MS);
        policy.set_airplay(sender(false, Duration::from_secs(1)));
        assert_eq!(policy.adjustment_base_ms(), 1000.0);
        policy.set_airplay(AirPlay::Unavailable);
        assert_eq!(policy.adjustment_base_ms(), 1000.0, "kept after it stops");
        assert_eq!(
            OutputPolicy::new(sender(true, L)).adjustment_base_ms(),
            1750.0
        );
    }

    #[test]
    fn with_airplay_a_new_output_still_starts_with_no_delay() {
        let policy = OutputPolicy::new(sender(true, L));
        assert_eq!(policy.settings("new").delay_ms, 0.0);
    }

    #[test]
    fn a_delay_becomes_frames_at_the_session_rate() {
        let mut policy = policy_with(&["a", "b"]);
        policy.set_delay_ms("a", 2000.0);
        assert_eq!(policy.params("a").delay_frames, 96_000);
        policy.set_delay_ms("a", 1820.0);
        assert_eq!(policy.params("a").delay_frames, 87_360);
        policy.set_delay_ms("a", 0.0);
        assert_eq!(policy.params("a").delay_frames, 0);
    }

    #[test]
    fn delays_are_clamped_and_non_finite_ones_ignored() {
        let mut policy = policy_with(&["a", "b"]);
        assert!(policy.set_delay_ms("a", 500.0));
        assert!(!policy.set_delay_ms("a", f64::NAN));
        assert!(!policy.set_delay_ms("a", f64::INFINITY));
        assert_eq!(policy.settings("a").delay_ms, 500.0);
        assert!(policy.set_delay_ms("a", 25_000.0));
        assert_eq!(policy.settings("a").delay_ms, 20_000.0);
        assert_eq!(policy.params("a").delay_frames, 960_000);
        assert!(policy.set_delay_ms("a", -5.0));
        assert_eq!(policy.settings("a").delay_ms, -5.0);
        assert!(policy.set_delay_ms("a", -2500.0));
        assert_eq!(policy.settings("a").delay_ms, -2000.0);
        assert_eq!(policy.params("a").delay_frames, 0);
        assert_eq!(policy.params("b").delay_frames, 96_000, "b waits for a");
    }

    #[test]
    fn a_lone_output_plays_at_once_whatever_its_offset() {
        let mut policy = policy_with(&["a"]);
        policy.set_delay_ms("a", 2000.0);
        assert_eq!(policy.params("a").delay_frames, 0);
        policy.set_delay_ms("a", -250.0);
        assert_eq!(policy.params("a").delay_frames, 0);
    }

    #[test]
    fn each_output_has_its_own_delay_and_volume() {
        let mut policy = policy_with(&["a", "b"]);
        policy.set_delay_ms("a", 150.0);
        policy.set_delay_ms("b", 40.0);
        policy.set_volume("a", 50.0);
        policy.set_volume("b", 100.0);
        // b plays first, a 110 ms after it.
        assert_eq!(
            policy.params("a"),
            OutputParams {
                delay_frames: 5_280,
                gain: 0.125,
            }
        );
        assert_eq!(
            policy.params("b"),
            OutputParams {
                delay_frames: 0,
                gain: 1.0,
            }
        );
    }

    #[test]
    fn the_gain_follows_the_cubic_curve_times_the_master() {
        let mut policy = policy_with(&["a"]);
        assert!((policy.params("a").gain - 0.216).abs() < 1e-6, "volume 60");
        policy.set_volume("a", 50.0);
        policy.set_master(50.0);
        assert_eq!(policy.params("a").gain, 0.015_625);
        policy.set_master(0.0);
        assert_eq!(policy.params("a").gain, 0.0);
    }

    #[test]
    fn volumes_are_clamped_and_non_finite_ones_ignored() {
        let mut policy = policy_with(&["a"]);
        assert!(!policy.set_volume("a", f64::NAN));
        assert_eq!(policy.settings("a").volume, 60.0);
        assert!(policy.set_volume("a", 150.0));
        assert_eq!(policy.settings("a").volume, 100.0);
        assert!(!policy.set_master(f64::NEG_INFINITY));
        assert_eq!(policy.master(), 100.0);
        assert!(policy.set_master(-3.0));
        assert_eq!(policy.master(), 0.0);
    }

    #[test]
    fn settings_of_a_disconnected_output_are_kept() {
        let mut policy = OutputPolicy::new(AirPlay::Unavailable);
        policy.set_volume("headphones", 30.0);
        policy.set_delay_ms("headphones", 250.0);
        let settings = policy.settings("headphones");
        assert_eq!(settings.volume, 30.0);
        assert_eq!(settings.delay_ms, 250.0);
        assert!(!settings.enabled);
    }

    #[test]
    fn with_no_airplay_speaker_chosen_outputs_play_at_their_own_delay() {
        let mut policy = policy_with(&["a", "b"]);
        policy.set_delay_ms("a", 150.0);
        assert!(
            !policy.set_airplay(sender(false, L)),
            "a sender with no speaker chosen changes no delay"
        );
        assert_eq!(policy.params("a").delay_frames, 7_200);
    }

    #[test]
    fn with_an_airplay_speaker_chosen_outputs_play_at_l_plus_their_offset() {
        // S176: m = −L, so h = L + o.
        let mut policy = policy_with(&["a", "b"]);
        policy.set_delay_ms("a", 150.0);
        assert!(policy.set_airplay(sender(true, L)));
        assert_eq!(policy.params("a").delay_frames, 91_200, "1750 + 150 ms");
        assert_eq!(policy.params("b").delay_frames, 84_000, "1750 ms");
        assert!(!policy.set_airplay(sender(true, L)));
        // Turned off again: back to the output's own delay.
        assert!(policy.set_airplay(sender(false, L)));
        assert_eq!(policy.params("a").delay_frames, 7_200);
        assert_eq!(policy.params("b").delay_frames, 0);
        assert_eq!(policy.settings("a").delay_ms, 150.0, "the setting is kept");
    }

    #[test]
    fn an_offset_is_not_held_to_3000_ms_with_an_airplay_speaker_chosen() {
        // S178 replaces P114's d_local trim: only the hold-back is
        // capped, at 20 000 ms (S176).
        let mut policy = policy_with(&["a", "b"]);
        policy.set_delay_ms("a", 5000.0);
        policy.set_airplay(sender(true, L));
        assert_eq!(policy.params("a").delay_frames, 324_000, "1750 + 5000 ms");
        policy.set_airplay(sender(false, L));
        assert_eq!(policy.params("a").delay_frames, 240_000, "5000 ms");
        policy.set_delay_ms("a", 19_000.0);
        policy.set_airplay(sender(true, L));
        assert_eq!(policy.params("a").delay_frames, 960_000, "capped");
    }

    #[test]
    fn a_new_l_re_anchors_the_outputs() {
        let mut policy = policy_with(&["a"]);
        policy.set_airplay(sender(true, L));
        // Lf changed to 44 100 frames (P98): L is 1 s.
        assert!(policy.set_airplay(sender(true, Duration::from_secs(1))));
        assert_eq!(policy.params("a").delay_frames, 48_000);
        // A new L while no speaker is chosen changes no delay.
        policy.set_airplay(sender(false, L));
        assert!(!policy.set_airplay(sender(false, Duration::from_secs(1))));
    }

    #[test]
    fn outputs_wait_for_the_cast_offset_and_for_the_longer_of_it_and_l() {
        let mut policy = policy_with(&["a", "b"]);
        policy.set_delay_ms("a", 150.0);
        assert!(policy.set_cast_offset(Some(Duration::from_millis(500))));
        assert_eq!(policy.params("a").delay_frames, 31_200, "500 + 150 ms");
        assert_eq!(policy.params("b").delay_frames, 24_000, "500 ms");
        assert!(!policy.set_cast_offset(Some(Duration::from_millis(500))));

        policy.set_airplay(sender(true, L));
        assert_eq!(policy.params("b").delay_frames, 84_000, "L is longer");
        assert!(policy.set_cast_offset(Some(Duration::from_secs(3))));
        assert_eq!(
            policy.params("b").delay_frames,
            144_000,
            "the offset is longer"
        );

        policy.set_airplay(sender(false, L));
        assert!(policy.set_cast_offset(None));
        assert_eq!(policy.params("a").delay_frames, 7_200);
        assert_eq!(policy.params("b").delay_frames, 0);
    }

    #[test]
    fn airplay_cast_and_local_outputs_play_at_one_latency() {
        let all = ["builtin", "usb", "bluetooth"];
        for (adjustment_ms, cast_ms) in [(0.0, 3000), (0.0, 1000), (250.0, 1900), (0.0, 1750)] {
            let mut policy = local_delay_policy(sender(true, L), &all);
            policy.set_airplay_adjustment_ms(adjustment_ms);
            policy.set_cast_offset(Some(Duration::from_millis(cast_ms)));
            // When each is heard: a local output's offset is its own lateness, negated.
            let mut heard = vec![
                1750.0 + adjustment_ms + policy.airplay_hold_back_ms(),
                cast_ms as f64 + policy.cast_hold_back_ms(),
            ];
            heard.extend(all.iter().map(|id| {
                let offset = if *id == "builtin" {
                    0.0
                } else {
                    policy.settings(id).delay_ms
                };
                policy.hold_back_ms(id) - offset
            }));
            let spread = heard.iter().copied().fold(f64::MIN, f64::max)
                - heard.iter().copied().fold(f64::MAX, f64::min);
            assert!(
                spread <= f64::from(DRIFT_TOLERANCE_MS),
                "A {adjustment_ms} ms, Cast {cast_ms} ms: heard at {heard:?}"
            );
        }
    }

    fn mix(float32: bool, rate: u32, channels: u16, channel_mask: u32) -> MixFormat {
        MixFormat {
            float32,
            rate,
            channels,
            channel_mask,
        }
    }

    #[test]
    fn a_float_mix_format_is_used_as_it_is() {
        assert_eq!(
            render_format(mix(true, 48_000, 2, 0x3)),
            Ok(RenderFormat {
                rate: 48_000,
                channels: 2,
                channel_mask: 0x3,
                convert: false,
            })
        );
    }

    #[test]
    fn another_mix_format_gets_float_converted_by_windows_at_its_own_rate() {
        assert_eq!(
            render_format(mix(false, 44_100, 6, 0x3f)),
            Ok(RenderFormat {
                rate: 44_100,
                channels: 6,
                channel_mask: 0x3f,
                convert: true,
            })
        );
    }

    #[test]
    fn a_missing_channel_mask_gets_the_usual_one() {
        assert_eq!(
            render_format(mix(true, 48_000, 1, 0)).unwrap().channel_mask,
            0x4
        );
        assert_eq!(
            render_format(mix(true, 48_000, 2, 0)).unwrap().channel_mask,
            0x3
        );
        assert_eq!(
            render_format(mix(true, 48_000, 8, 0)).unwrap().channel_mask,
            0
        );
    }

    #[test]
    fn unusable_mix_formats_are_refused() {
        assert_eq!(
            render_format(mix(true, 48_000, 0, 0)),
            Err(BackendError::UnsupportedFormat)
        );
        assert_eq!(
            render_format(mix(true, 768_000, 2, 0x3)),
            Err(BackendError::UnsupportedFormat)
        );
    }

    #[test]
    fn the_render_buffer_holds_two_periods_within_bounds() {
        assert_eq!(render_buffer_hns(100_000), 200_000, "10 ms period");
        assert_eq!(
            render_buffer_hns(30_000),
            MIN_RENDER_BUFFER_HNS,
            "3 ms period"
        );
        assert_eq!(render_buffer_hns(300_000), 600_000, "30 ms period");
        assert_eq!(render_buffer_hns(50_000_000), MAX_RENDER_BUFFER_HNS);
        assert_eq!(render_buffer_hns(0), MIN_RENDER_BUFFER_HNS);
        assert_eq!(render_buffer_hns(-1), MIN_RENDER_BUFFER_HNS);
        assert_eq!(render_buffer_hns(i64::MAX), MAX_RENDER_BUFFER_HNS);
    }

    // covers: S206
    #[test]
    fn a_period_in_frames_is_rounded_up() {
        assert_eq!(period_frames(100_000, 48_000), 480, "10 ms");
        assert_eq!(period_frames(100_000, 44_100), 441);
        assert_eq!(period_frames(30_000, 44_100), 133, "3 ms is 132.3 frames");
        assert_eq!(period_frames(0, 48_000), 0);
        assert_eq!(period_frames(-1, 48_000), 0);
        assert_eq!(period_frames(i64::MAX, 48_000), 9_600, "at most 200 ms");
    }

    #[test]
    fn microphone_calibration_hears_external_speakers_only() {
        let kind = |form: Option<u32>, bus: Option<&str>| mic_kind(form, bus);
        let headphones = Some(form_factor::HEADPHONES);
        let speakers = Some(form_factor::SPEAKERS);
        assert_eq!(kind(headphones, Some("HDAUDIO")), MicKind::Headphones);
        assert_eq!(kind(headphones, Some("BTHENUM")), MicKind::Headphones);
        assert_eq!(
            kind(Some(form_factor::HEADSET), Some("USB")),
            MicKind::Headphones
        );
        assert_eq!(
            kind(Some(form_factor::HANDSET), Some("BTHENUM")),
            MicKind::Headphones
        );
        assert_eq!(kind(headphones, None), MicKind::Headphones);
        assert_eq!(kind(speakers, Some("ROOT")), MicKind::Virtual);
        assert_eq!(kind(headphones, Some("Root")), MicKind::Virtual);
        assert_eq!(kind(None, Some("ROOT")), MicKind::Virtual);
        assert_eq!(
            kind(speakers, Some("BTHENUM")),
            MicKind::Speaker,
            "a Bluetooth speaker"
        );
        assert_eq!(kind(speakers, Some("BTHLEDevice")), MicKind::Speaker);
        assert_eq!(kind(speakers, Some("HDAUDIO")), MicKind::Speaker);
        assert_eq!(kind(speakers, Some("USB")), MicKind::Speaker);
        assert_eq!(
            kind(Some(form_factor::LINE_LEVEL), Some("HDAUDIO")),
            MicKind::Speaker
        );
        assert_eq!(
            kind(Some(form_factor::DIGITAL_AUDIO_DISPLAY), Some("HDAUDIO")),
            MicKind::Speaker
        );
        assert_eq!(kind(Some(8), Some("HDAUDIO")), MicKind::Speaker, "S/PDIF");
        assert_eq!(
            kind(Some(0), Some("SWD")),
            MicKind::Speaker,
            "a remote session"
        );
        assert_eq!(kind(None, None), MicKind::Speaker, "nothing could be read");
        assert_eq!(kind(None, Some("HDAUDIO")), MicKind::Speaker);
        assert_eq!(kind(speakers, None), MicKind::Speaker);
    }

    #[test]
    fn the_reference_is_always_an_output_the_microphone_hears() {
        for form in 0..=10 {
            for bus in ["HDAUDIO", "INTELAUDIO", "USB", "BTHENUM", "ROOT", "SWD", ""] {
                if output_kind(Some(form), bus, false) == OutputKind::BuiltIn {
                    assert_eq!(
                        mic_kind(Some(form), Some(bus)),
                        MicKind::Speaker,
                        "{form} {bus}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_listing_keeps_what_windows_said_about_each_device() {
        let listing = list_endpoints(vec![reading(0, "a", "Headphones", 3, "HDAUDIO")], None);
        assert_eq!(
            listing.endpoints[0].traits,
            DeviceTraits {
                form_factor: Some(3),
                enumerator: Some("HDAUDIO".to_owned()),
            }
        );
        assert_eq!(listing.endpoints[0].traits.mic_kind(), MicKind::Headphones);
    }
}
