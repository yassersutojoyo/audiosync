//! The part of the PipeWire graph AudioSync reads: outputs (sinks), inputs
//! (sources), the apps' playback streams, their ports and links, and the
//! clients that own them. The registry threads keep a [`Graph`] up to date;
//! everything decided from it (the listing, which app owns a stream, which
//! links a capture needs) is a plain function of it, so it is tested
//! without a daemon.

use std::collections::{BTreeMap, BTreeSet};

use crate::audio::backend::{BackendError, OutputKind};
use crate::audio::outputs::{
    DeviceTraits, Endpoint, EndpointProperties, EndpointReading, Listing, ListingNote, form_factor,
};
use crate::audio::process::ProcessTable;
use crate::audio::routing::{EndpointAudio, EndpointVolume};

pub const SINK_CLASS: &str = "Audio/Sink";
/// The node name of AudioSync's own null sink (S477).
pub const OWN_SINK: &str = "audiosync-silent";
pub const STREAM_CLASS: &str = "Stream/Output/Audio";
const INPUT_STREAM_CLASS: &str = "Stream/Input/Audio";
const SOURCE_CLASSES: [&str; 2] = ["Audio/Source", "Audio/Source/Virtual"];

/// The factories of Bluetooth hands-free nodes, for calls only: opening one
/// switches the headset out of its stereo profile.
const HANDS_FREE_FACTORIES: [&str; 2] = ["api.bluez5.sco.sink", "api.bluez5.sco.source"];

/// The executables whose client PID stands for another process: the daemon
/// itself, and its PulseAudio server, which every PulseAudio-API app talks
/// through.
const DAEMON_EXECUTABLES: [&str; 2] = ["pipewire", "pipewire-pulse"];

/// What kind of global an object is, as far as the graph keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Node,
    Port,
    Link,
    Client,
}

/// A node and what was learned about it beyond its global properties.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub props: BTreeMap<String, String>,
    /// Whether its last info said it runs.
    pub running: bool,
    /// An output's own mute and volume, once its `Props` param came.
    pub volume: Option<EndpointVolume>,
    /// The rate and channels its `Format` param last said.
    pub format: Option<(u32, u16)>,
}

impl Node {
    pub fn prop(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(String::as_str)
    }

    pub fn class(&self) -> &str {
        self.prop("media.class").unwrap_or_default()
    }

    pub fn name(&self) -> &str {
        self.prop("node.name").unwrap_or_default()
    }

    /// The name people see: its description, else its nickname, else its
    /// node name.
    pub fn display_name(&self) -> &str {
        ["node.description", "node.nick", "node.name"]
            .into_iter()
            .filter_map(|key| self.prop(key))
            .find(|name| !name.trim().is_empty())
            .unwrap_or_default()
    }

    fn hands_free(&self) -> bool {
        self.prop("factory.name")
            .is_some_and(|factory| HANDS_FREE_FACTORIES.contains(&factory))
    }

    /// An AirPlay speaker PipeWire's own RAOP module publishes.
    fn raop(&self) -> bool {
        self.name().starts_with("raop_sink")
            || self.props.keys().any(|key| key.starts_with("raop."))
    }

    /// Whether it is a software node with no device behind it.
    pub fn virtual_device(&self) -> bool {
        self.prop("device.id").is_none()
    }

    pub fn bluetooth(&self) -> bool {
        self.prop("device.api") == Some("bluez5")
    }

    /// The channel count its properties give, for a node without a format.
    pub fn prop_channels(&self) -> Option<u16> {
        if let Some(channels) = self.prop("audio.channels").and_then(|c| c.parse().ok()) {
            return Some(channels);
        }
        let positions = positions(self.prop("audio.position")?);
        u16::try_from(positions.len()).ok().filter(|&n| n > 0)
    }
}

/// The channel names in an `audio.position` value, `[ FL FR ]` or `FL,FR`.
pub fn positions(value: &str) -> Vec<&str> {
    value
        .split(|c: char| c == ',' || c == '[' || c == ']' || c.is_whitespace())
        .filter(|name| !name.is_empty())
        .collect()
}

/// What calibration needs of a sink, in the Windows terms [`DeviceTraits`]
/// holds: a worn form factor as its Windows value, and a sink [`sink_kind`]
/// calls virtual under the `ROOT` enumerator Windows gives virtual devices.
pub fn sink_traits(node: &Node) -> DeviceTraits {
    let form_factor = match node.prop("device.form-factor") {
        Some("headphone") => Some(form_factor::HEADPHONES),
        Some("headset") => Some(form_factor::HEADSET),
        Some("handset") => Some(form_factor::HANDSET),
        _ => None,
    };
    let enumerator = if sink_kind(node) == OutputKind::Virtual {
        Some("ROOT")
    } else {
        node.prop("device.api")
    };
    DeviceTraits {
        form_factor,
        enumerator: enumerator.map(str::to_owned),
        vb_cable: false,
    }
}

/// The kind of an output, from its node's properties.
pub fn sink_kind(node: &Node) -> OutputKind {
    let form = node.prop("device.form-factor").unwrap_or_default();
    let worn = matches!(form, "headphone" | "headset");
    if node.bluetooth() {
        return OutputKind::Bluetooth;
    }
    if node.prop("device.bus") == Some("usb") {
        return match form {
            _ if worn => OutputKind::UsbHeadphones,
            "speaker" => OutputKind::UsbSpeakers,
            _ => OutputKind::Usb,
        };
    }
    if worn {
        return OutputKind::Headphones;
    }
    if node.virtual_device() {
        return OutputKind::Virtual;
    }
    let name = node.name().to_ascii_lowercase();
    if name.contains("hdmi") {
        return OutputKind::Hdmi;
    }
    if name.contains("displayport") {
        return OutputKind::DisplayPort;
    }
    if matches!(form, "internal" | "speaker") {
        return OutputKind::BuiltIn;
    }
    OutputKind::Other
}

/// A port of a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub node: u32,
    pub output: bool,
    pub monitor: bool,
    pub channel: String,
}

/// A link from an output port to an input port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Link {
    pub output_node: u32,
    pub output_port: u32,
    pub input_node: u32,
    pub input_port: u32,
}

/// A client's process IDs: the one the daemon saw on its socket, and the
/// one the client says it runs as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Client {
    pub sec_pid: Option<u32>,
    pub app_pid: Option<u32>,
}

/// The graph as the registry announced it.
#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: BTreeMap<u32, Node>,
    pub ports: BTreeMap<u32, Port>,
    pub links: BTreeMap<u32, Link>,
    pub clients: BTreeMap<u32, Client>,
    /// The node name `default.audio.sink` holds.
    pub default_sink: Option<String>,
    /// The node name `default.audio.source` holds.
    pub default_source: Option<String>,
}

fn number(props: &BTreeMap<String, String>, key: &str) -> Option<u32> {
    props.get(key)?.parse().ok()
}

impl Graph {
    /// Records a global; true when it is an output or input, whose listing
    /// may have changed.
    pub fn add(&mut self, id: u32, kind: Kind, props: BTreeMap<String, String>) -> bool {
        match kind {
            Kind::Node => {
                let class = props.get("media.class").cloned().unwrap_or_default();
                let node = self.nodes.entry(id).or_default();
                node.props = props;
                class == SINK_CLASS || SOURCE_CLASSES.contains(&class.as_str())
            }
            Kind::Port => {
                if let Some(node) = number(&props, "node.id") {
                    self.ports.insert(
                        id,
                        Port {
                            node,
                            output: props.get("port.direction").map(String::as_str) == Some("out"),
                            monitor: props.get("port.monitor").map(String::as_str) == Some("true"),
                            channel: props.get("audio.channel").cloned().unwrap_or_default(),
                        },
                    );
                }
                false
            }
            Kind::Link => {
                let ends = (
                    number(&props, "link.output.node"),
                    number(&props, "link.output.port"),
                    number(&props, "link.input.node"),
                    number(&props, "link.input.port"),
                );
                if let (Some(output_node), Some(output_port), Some(input_node), Some(input_port)) =
                    ends
                {
                    self.links.insert(
                        id,
                        Link {
                            output_node,
                            output_port,
                            input_node,
                            input_port,
                        },
                    );
                }
                false
            }
            Kind::Client => {
                self.clients.insert(
                    id,
                    Client {
                        sec_pid: number(&props, "pipewire.sec.pid"),
                        app_pid: number(&props, "application.process.id"),
                    },
                );
                false
            }
        }
    }

    /// Forgets a global; true when it was an output or input.
    pub fn remove(&mut self, id: u32) -> bool {
        self.ports.remove(&id);
        self.links.remove(&id);
        self.clients.remove(&id);
        self.nodes.remove(&id).is_some_and(|node| {
            node.class() == SINK_CLASS || SOURCE_CLASSES.contains(&node.class())
        })
    }

    pub fn sinks(&self) -> impl Iterator<Item = (u32, &Node)> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.class() == SINK_CLASS)
            .map(|(&id, node)| (id, node))
    }

    /// The node named `name`, of any class.
    pub fn named(&self, name: &str) -> Option<(u32, &Node)> {
        self.nodes
            .iter()
            .find(|(_, node)| node.name() == name)
            .map(|(&id, node)| (id, node))
    }

    /// The input node named `name`, to capture as the source.
    pub fn capture_input(&self, name: &str) -> Result<&Node, BackendError> {
        let (_, node) = self.named(name).ok_or(BackendError::InputGone)?;
        // The listing leaves a hands-free node out, so one saved anyway counts as not connected.
        (!node.hands_free())
            .then_some(node)
            .ok_or(BackendError::InputGone)
    }

    /// The rate and channels to play to output `id` at: its own format,
    /// else what its properties say, else 48 kHz stereo; at most its first
    /// [`MAX_CHANNELS`](super::format::MAX_CHANNELS) channels.
    pub fn sink_format(&self, id: u32) -> (u32, u16) {
        let Some(node) = self.nodes.get(&id) else {
            return (48_000, 2);
        };
        let (rate, channels) = node.format.unwrap_or_else(|| {
            let rate = node.prop("audio.rate").and_then(|rate| rate.parse().ok());
            (rate.unwrap_or(48_000), node.prop_channels().unwrap_or(2))
        });
        (rate, channels.min(super::format::MAX_CHANNELS))
    }

    /// The default input device, when one is set and present.
    pub fn default_input(&self) -> Option<(u32, &Node)> {
        self.named(self.default_source.as_deref()?)
            .filter(|(_, node)| SOURCE_CLASSES.contains(&node.class()))
    }

    /// Whether the default input is a Bluetooth hands-free one.
    pub fn default_input_hands_free(&self) -> bool {
        self.default_input()
            .is_some_and(|(_, node)| node.hands_free())
    }

    /// The outputs to list, by node name: every sink but a Bluetooth
    /// hands-free one, PipeWire's own AirPlay sinks, which AudioSync
    /// reaches through its own sender, and AudioSync's own sink (S477).
    pub fn listing(&self) -> Listing {
        let mut listing = Listing::default();
        for (_, node) in self.sinks() {
            let id = node.name().to_owned();
            if id.is_empty() || id == OWN_SINK {
                continue;
            }
            let name = Some(node.display_name().to_owned()).filter(|name| !name.is_empty());
            if node.hands_free() {
                listing.notes.push(ListingNote::HandsFree { id, name });
                continue;
            }
            if node.raop() {
                listing.notes.push(ListingNote::NetworkSpeaker { id, name });
                continue;
            }
            listing.endpoints.push(Endpoint {
                is_default: self.default_sink.as_deref() == Some(id.as_str()),
                name_read: name.is_some(),
                name: name.unwrap_or_else(|| id.clone()),
                kind: sink_kind(node),
                traits: sink_traits(node),
                id,
            });
        }
        listing.endpoints.sort_by(|a, b| a.id.cmp(&b.id));
        listing
    }

    /// The input devices, hands-free ones left out.
    pub fn inputs(&self) -> Vec<EndpointReading> {
        self.nodes
            .values()
            .filter(|node| SOURCE_CLASSES.contains(&node.class()) && !node.hands_free())
            .filter(|node| !node.name().is_empty())
            .enumerate()
            .map(|(index, node)| EndpointReading {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                id: Ok(node.name().to_owned()),
                properties: Ok(EndpointProperties {
                    name: Some(node.display_name().to_owned()),
                    form_factor: None,
                    enumerator: node.prop("device.api").map(str::to_owned),
                    display_port: false,
                    channels: node
                        .prop_channels()
                        .map(|channels| channels.min(super::format::MAX_CHANNELS)),
                    vb_cable: false,
                }),
            })
            .collect()
    }

    /// The process that owns node `id`: its client's socket PID, unless
    /// that is the daemon or its PulseAudio server, which speak for the app
    /// that says which process it is. A PID that doesn't run is no app.
    pub fn owner_pid(&self, id: u32, processes: &ProcessTable) -> Option<u32> {
        let node = self.nodes.get(&id)?;
        let client = self.clients.get(&number(&node.props, "client.id")?)?;
        let sec_pid = client.sec_pid?;
        let entry = processes.get(sec_pid)?;
        if DAEMON_EXECUTABLES.contains(&entry.executable.as_str()) {
            return client.app_pid.filter(|&pid| processes.contains(pid));
        }
        Some(sec_pid)
    }

    /// The apps' playback streams.
    pub fn streams(&self) -> impl Iterator<Item = u32> + '_ {
        self.nodes
            .iter()
            .filter(|(_, node)| node.class() == STREAM_CLASS)
            .map(|(&id, _)| id)
    }

    /// The owners of every running capture stream linked from a source:
    /// the apps listening to a microphone (S487).
    pub fn listening_pids(&self, processes: &ProcessTable) -> Vec<u32> {
        let sources: BTreeSet<u32> = self
            .nodes
            .iter()
            .filter(|(_, node)| SOURCE_CLASSES.contains(&node.class()))
            .map(|(&id, _)| id)
            .collect();
        let pids: BTreeSet<u32> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.class() == INPUT_STREAM_CLASS && node.running)
            .map(|(&id, _)| id)
            .filter(|&id| sources.iter().any(|&source| self.linked_into(source, id)))
            .filter_map(|id| self.owner_pid(id, processes))
            .collect();
        pids.into_iter().collect()
    }

    /// The playback streams owned by one of `pids`.
    pub fn streams_of(&self, pids: &[u32], processes: &ProcessTable) -> Vec<u32> {
        self.streams()
            .filter(|&id| {
                self.owner_pid(id, processes)
                    .is_some_and(|pid| pids.contains(&pid))
            })
            .collect()
    }

    /// The playback streams of `pids` whose `target.object` must be set
    /// (`pin`) or cleared to match, `targeted` holding those set (S487).
    pub fn to_target(
        &self,
        targeted: &mut BTreeSet<String>,
        pids: &[u32],
        processes: &ProcessTable,
        pin: bool,
    ) -> Vec<u32> {
        // Keyed by `object.serial`: PipeWire gives a freed node ID to the
        // next node, never a serial.
        let serial = |id: &u32| self.nodes.get(id)?.prop("object.serial");
        let live: BTreeSet<&str> = self.streams().filter_map(|id| serial(&id)).collect();
        targeted.retain(|key| live.contains(key.as_str()));
        self.streams_of(pids, processes)
            .into_iter()
            .filter(|id| match serial(id) {
                Some(key) if pin => targeted.insert(key.to_owned()),
                Some(key) => targeted.remove(key),
                None => false,
            })
            .collect()
    }

    /// Whether stream `id` is linked into node `sink`.
    fn linked_into(&self, id: u32, sink: u32) -> bool {
        self.links
            .values()
            .any(|link| link.output_node == id && link.input_node == sink)
    }

    /// The owners of every stream linked into an output.
    pub fn session_pids(&self, processes: &ProcessTable) -> Vec<u32> {
        let sinks: BTreeSet<u32> = self.sinks().map(|(id, _)| id).collect();
        let pids: BTreeSet<u32> = self
            .streams()
            .filter(|&id| sinks.iter().any(|&sink| self.linked_into(id, sink)))
            .filter_map(|id| self.owner_pid(id, processes))
            .collect();
        pids.into_iter().collect()
    }

    /// Each listed output as the double-audio check reads it.
    pub fn endpoint_audio(&self, processes: &ProcessTable) -> Vec<EndpointAudio> {
        let listing = self.listing();
        listing
            .endpoints
            .iter()
            .filter_map(|endpoint| {
                let (sink, node) = self.named(&endpoint.id)?;
                let pids: BTreeSet<u32> = self
                    .streams()
                    .filter(|&id| self.nodes.get(&id).is_some_and(|stream| stream.running))
                    .filter(|&id| self.linked_into(id, sink))
                    .filter_map(|id| self.owner_pid(id, processes))
                    .collect();
                Some(EndpointAudio {
                    id: endpoint.id.clone(),
                    name: endpoint.name.clone(),
                    is_default: endpoint.is_default,
                    virtual_device: node.virtual_device(),
                    volume: node.volume,
                    active_pids: pids.into_iter().collect(),
                    silent_pids: Vec::new(),
                    mix_pids: Vec::new(),
                })
            })
            .collect()
    }

    /// The links a capture keeps, `takes` saying which streams it records:
    /// for the whole system (`whole_system`) the default output's monitor
    /// while it holds none of AudioSync's streams, else each stream's own
    /// links.
    pub fn capture_links(
        &self,
        capture: u32,
        whole_system: bool,
        takes: impl Fn(u32) -> bool,
    ) -> BTreeSet<Link> {
        if whole_system && let Some(links) = self.monitor_links(capture, &takes) {
            return links;
        }
        self.wanted_links(capture, takes)
    }

    /// The default output's monitor into `capture`, while it holds none of
    /// AudioSync's streams: each of those plays into another output, or
    /// names one by `target.object` before it is linked. A taken stream
    /// that plays only into other outputs is linked beside it. A stream in
    /// the default output reaches the capture from its first buffer.
    fn monitor_links(&self, capture: u32, takes: &impl Fn(u32) -> bool) -> Option<BTreeSet<Link>> {
        let (default, node) = self.named(self.default_sink.as_deref()?)?;
        let serial = node.prop("object.serial");
        let sinks: BTreeSet<u32> = self.sinks().map(|(id, _)| id).collect();
        let mut links = self.channel_links(default, true, capture);
        if links.is_empty() {
            return None;
        }
        for stream in self.streams().filter(|&id| id != capture) {
            let outputs: BTreeSet<u32> = self
                .links
                .values()
                .filter(|link| link.output_node == stream && sinks.contains(&link.input_node))
                .map(|link| link.input_node)
                .collect();
            if takes(stream) {
                if !outputs.is_empty() && !outputs.contains(&default) {
                    links.extend(self.channel_links(stream, false, capture));
                }
                continue;
            }
            let target = self
                .nodes
                .get(&stream)
                .and_then(|node| node.prop("target.object"));
            let elsewhere = if outputs.is_empty() {
                target.is_some() && target != serial
            } else {
                !outputs.contains(&default)
            };
            if !elsewhere {
                return None;
            }
        }
        Some(links)
    }

    /// The links that bring every stream `takes` accepts into `capture`'s
    /// input ports, channel to channel ([`input_channels`]).
    pub fn wanted_links(&self, capture: u32, takes: impl Fn(u32) -> bool) -> BTreeSet<Link> {
        self.streams()
            .filter(|&id| id != capture && takes(id))
            .flat_map(|stream| self.channel_links(stream, false, capture))
            .collect()
    }

    /// The links from `source`'s output ports, its monitor ports for
    /// `monitor`, into `capture`'s input ports, channel to channel.
    fn channel_links(&self, source: u32, monitor: bool, capture: u32) -> BTreeSet<Link> {
        let inputs: Vec<(u32, &Port)> = self
            .ports
            .iter()
            .filter(|(_, port)| port.node == capture && !port.output)
            .map(|(&id, port)| (id, port))
            .collect();
        let mut links = BTreeSet::new();
        let outputs = self
            .ports
            .iter()
            .filter(|(_, port)| port.node == source && port.output && port.monitor == monitor);
        for (&output_port, port) in outputs {
            for channel in input_channels(&port.channel) {
                for &(input_port, _) in inputs.iter().filter(|(_, input)| input.channel == *channel)
                {
                    links.insert(Link {
                        output_node: source,
                        output_port,
                        input_node: capture,
                        input_port,
                    });
                }
            }
        }
        links
    }
}

/// The capture's stereo channels a stream's channel goes to: front and
/// centre or mono to both, sides and rears to their own side; LFE and the
/// rest are left out.
pub fn input_channels(channel: &str) -> &'static [&'static str] {
    match channel {
        "FL" | "SL" | "RL" => &["FL"],
        "FR" | "SR" | "RR" => &["FR"],
        "MONO" | "FC" => &["FL", "FR"],
        _ => &[],
    }
}

/// Which streams a capture takes, by owner process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Every stream except those of this process's tree (AudioSync's own).
    Except(u32),
    /// Only the streams of these processes' trees.
    Trees(Vec<u32>),
}

impl Filter {
    /// Whether a stream owned by `pid` (`None`: unknown) is captured.
    pub fn takes(&self, pid: Option<u32>, processes: &ProcessTable) -> bool {
        match (self, pid) {
            (Filter::Except(own), Some(pid)) => !processes.in_tree(pid, *own),
            (Filter::Except(_), None) => true,
            (Filter::Trees(roots), Some(pid)) => {
                roots.iter().any(|&root| processes.in_tree(pid, root))
            }
            (Filter::Trees(_), None) => false,
        }
    }
}

/// An output's mute and volume from its `Props` param: the loudest channel,
/// on the cubic scale desktop sliders show.
pub fn endpoint_volume(muted: bool, channel_volumes: &[f32]) -> EndpointVolume {
    let linear = channel_volumes.iter().copied().fold(0.0_f32, f32::max);
    EndpointVolume {
        muted,
        level: linear.max(0.0).cbrt().min(1.0),
    }
}

/// The linear channel volume for a slider `level`.
pub fn linear_volume(level: f32) -> f32 {
    let level = level.clamp(0.0, 1.0);
    level * level * level
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::outputs::MicKind;
    use crate::audio::process::ProcessEntry;

    fn props(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn process(pid: u32, parent_pid: u32, executable: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent_pid,
            created: Some(u64::from(pid)),
            executable: executable.to_owned(),
        }
    }

    fn sink(graph: &mut Graph, id: u32, extra: &[(&str, &str)]) {
        let mut all = props(&[("media.class", SINK_CLASS)]);
        all.extend(props(extra));
        graph.add(id, Kind::Node, all);
    }

    // covers: S381
    #[test]
    fn sinks_are_listed_by_node_name_with_the_default_marked() {
        let mut graph = Graph::default();
        sink(
            &mut graph,
            40,
            &[("node.name", "as-b"), ("node.description", "Second")],
        );
        sink(
            &mut graph,
            41,
            &[("node.name", "as-a"), ("node.nick", "First")],
        );
        graph.default_sink = Some("as-a".to_owned());
        let listing = graph.listing();
        let ids: Vec<&str> = listing.endpoints.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["as-a", "as-b"]);
        assert!(listing.endpoints[0].is_default);
        assert_eq!(listing.endpoints[0].name, "First");
        assert_eq!(listing.endpoints[1].name, "Second");
        assert_eq!(listing.endpoints[1].kind, OutputKind::Virtual);
        assert!(listing.notes.is_empty());
    }

    // covers: S381
    #[test]
    fn hands_free_and_airplay_sinks_are_left_out_with_a_note() {
        let mut graph = Graph::default();
        sink(
            &mut graph,
            1,
            &[
                ("node.name", "bluez_output.sco"),
                ("factory.name", "api.bluez5.sco.sink"),
            ],
        );
        sink(&mut graph, 2, &[("node.name", "raop_sink.kitchen")]);
        sink(
            &mut graph,
            3,
            &[
                ("node.name", "bluez_output.a2dp"),
                ("factory.name", "api.bluez5.a2dp.sink"),
            ],
        );
        let listing = graph.listing();
        assert_eq!(listing.endpoints.len(), 1);
        assert!(matches!(listing.notes[0], ListingNote::HandsFree { .. }));
        assert!(matches!(
            listing.notes[1],
            ListingNote::NetworkSpeaker { .. }
        ));
    }

    #[test]
    fn audiosyncs_own_sink_is_not_an_output() {
        let mut graph = Graph::default();
        sink(&mut graph, 1, &[("node.name", "audiosync-silent")]);
        sink(&mut graph, 2, &[("node.name", "alsa_output.speakers")]);
        graph.default_sink = Some("audiosync-silent".to_owned());
        let ids: Vec<String> = graph
            .listing()
            .endpoints
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["alsa_output.speakers"]);
    }

    // covers: S381
    #[test]
    fn kinds_come_from_bus_form_factor_and_name() {
        let node = |pairs: &[(&str, &str)]| Node {
            props: props(pairs),
            ..Node::default()
        };
        let device = ("device.id", "7");
        assert_eq!(
            sink_kind(&node(&[device, ("device.api", "bluez5")])),
            OutputKind::Bluetooth
        );
        assert_eq!(
            sink_kind(&node(&[
                device,
                ("device.bus", "usb"),
                ("device.form-factor", "headset")
            ])),
            OutputKind::UsbHeadphones
        );
        assert_eq!(
            sink_kind(&node(&[device, ("device.bus", "usb")])),
            OutputKind::Usb
        );
        assert_eq!(
            sink_kind(&node(&[device, ("device.form-factor", "headphone")])),
            OutputKind::Headphones
        );
        assert_eq!(
            sink_kind(&node(&[
                device,
                ("node.name", "alsa_output.pci.hdmi-stereo")
            ])),
            OutputKind::Hdmi
        );
        assert_eq!(
            sink_kind(&node(&[device, ("device.form-factor", "internal")])),
            OutputKind::BuiltIn
        );
        assert_eq!(sink_kind(&node(&[device])), OutputKind::Other);
        assert_eq!(sink_kind(&node(&[])), OutputKind::Virtual);
    }

    // covers: S381
    #[test]
    fn calibration_leaves_out_worn_and_virtual_sinks() {
        let mut graph = Graph::default();
        let device = ("device.id", "7");
        sink(&mut graph, 1, &[("node.name", "a-room"), device]);
        sink(
            &mut graph,
            2,
            &[
                ("node.name", "b-phones"),
                device,
                ("device.form-factor", "headphone"),
            ],
        );
        sink(
            &mut graph,
            3,
            &[
                ("node.name", "c-headset"),
                device,
                ("device.api", "bluez5"),
                ("device.form-factor", "headset"),
            ],
        );
        sink(&mut graph, 4, &[("node.name", "d-null-sink")]);
        sink(
            &mut graph,
            5,
            &[("node.name", "e-bt-speaker"), ("device.api", "bluez5")],
        );
        sink(
            &mut graph,
            6,
            &[("node.name", "f-usb-speaker"), ("device.bus", "usb")],
        );
        let kinds: Vec<MicKind> = graph
            .listing()
            .endpoints
            .iter()
            .map(|output| output.traits.mic_kind())
            .collect();
        assert_eq!(
            kinds,
            [
                MicKind::Speaker,
                MicKind::Headphones,
                MicKind::Headphones,
                MicKind::Virtual,
                MicKind::Speaker,
                MicKind::Speaker
            ]
        );
    }

    // covers: S380
    #[test]
    fn pulse_apps_are_owned_by_the_process_they_name() {
        let processes = ProcessTable::new([
            process(100, 1, "pipewire-pulse"),
            process(200, 1, "firefox"),
            process(300, 1, "mpv"),
        ]);
        let mut graph = Graph::default();
        graph.add(
            5,
            Kind::Client,
            props(&[
                ("pipewire.sec.pid", "100"),
                ("application.process.id", "200"),
            ]),
        );
        graph.add(6, Kind::Client, props(&[("pipewire.sec.pid", "300")]));
        graph.add(
            7,
            Kind::Client,
            props(&[
                ("pipewire.sec.pid", "100"),
                ("application.process.id", "999"),
            ]),
        );
        for (node, client) in [(10, "5"), (11, "6"), (12, "7")] {
            graph.add(
                node,
                Kind::Node,
                props(&[("media.class", STREAM_CLASS), ("client.id", client)]),
            );
        }
        assert_eq!(graph.owner_pid(10, &processes), Some(200));
        assert_eq!(graph.owner_pid(11, &processes), Some(300));
        // The app it names doesn't run.
        assert_eq!(graph.owner_pid(12, &processes), None);
    }

    /// AudioSync (pid 50) with a child (51), an app (60) with a child (61),
    /// each owning one stereo stream into sink 1; capture node 90.
    fn playing_graph() -> (Graph, ProcessTable) {
        let processes = ProcessTable::new([
            process(50, 1, "audiosync"),
            process(51, 50, "pw-play"),
            process(60, 1, "app"),
            process(61, 60, "app"),
        ]);
        let mut graph = Graph::default();
        sink(&mut graph, 1, &[("node.name", "as-a")]);
        graph.add(
            90,
            Kind::Node,
            props(&[
                ("media.class", "Stream/Input/Audio"),
                ("node.name", "audiosync-capture"),
            ]),
        );
        for (index, channel) in ["FL", "FR"].into_iter().enumerate() {
            graph.add(
                900 + index as u32,
                Kind::Port,
                props(&[
                    ("node.id", "90"),
                    ("port.direction", "in"),
                    ("audio.channel", channel),
                ]),
            );
        }
        for (stream, pid) in [(20_u32, 50_u32), (21, 51), (22, 60), (23, 61)] {
            let client = stream + 100;
            graph.add(
                client,
                Kind::Client,
                props(&[("pipewire.sec.pid", &pid.to_string())]),
            );
            graph.add(
                stream,
                Kind::Node,
                props(&[
                    ("media.class", STREAM_CLASS),
                    ("client.id", &client.to_string()),
                ]),
            );
            graph.nodes.get_mut(&stream).unwrap().running = true;
            for (index, channel) in ["FL", "FR"].into_iter().enumerate() {
                let port = stream * 10 + index as u32;
                graph.add(
                    port,
                    Kind::Port,
                    props(&[
                        ("node.id", &stream.to_string()),
                        ("port.direction", "out"),
                        ("audio.channel", channel),
                    ]),
                );
                graph.add(
                    port + 5000,
                    Kind::Link,
                    props(&[
                        ("link.output.node", &stream.to_string()),
                        ("link.output.port", &port.to_string()),
                        ("link.input.node", "1"),
                        ("link.input.port", "77"),
                    ]),
                );
            }
        }
        (graph, processes)
    }

    fn captured(graph: &Graph, processes: &ProcessTable, filter: &Filter) -> BTreeSet<u32> {
        graph
            .wanted_links(90, |id| {
                filter.takes(graph.owner_pid(id, processes), processes)
            })
            .into_iter()
            .map(|link| link.output_node)
            .collect()
    }

    /// [`playing_graph`] with `as-a` the default output and its monitor
    /// ports, AudioSync's streams moved to a second output, `as-b`.
    fn monitored_graph() -> (Graph, ProcessTable) {
        let (mut graph, processes) = playing_graph();
        graph.default_sink = Some("as-a".to_owned());
        sink(&mut graph, 2, &[("node.name", "as-b")]);
        for (index, channel) in ["FL", "FR"].into_iter().enumerate() {
            graph.add(
                10 + index as u32,
                Kind::Port,
                props(&[
                    ("node.id", "1"),
                    ("port.direction", "out"),
                    ("port.monitor", "true"),
                    ("audio.channel", channel),
                ]),
            );
        }
        for link in graph.links.values_mut() {
            if link.output_node == 20 || link.output_node == 21 {
                link.input_node = 2;
            }
        }
        (graph, processes)
    }

    fn routed(graph: &Graph, processes: &ProcessTable, filter: &Filter) -> BTreeSet<u32> {
        graph
            .capture_links(90, matches!(filter, Filter::Except(_)), |id| {
                filter.takes(graph.owner_pid(id, processes), processes)
            })
            .into_iter()
            .map(|link| link.output_node)
            .collect()
    }

    // covers: S479
    #[test]
    fn the_whole_system_takes_the_default_outputs_monitor_while_audiosync_plays_elsewhere() {
        let (graph, processes) = monitored_graph();
        let links = graph.capture_links(90, true, |id| {
            Filter::Except(50).takes(graph.owner_pid(id, &processes), &processes)
        });
        let pairs: BTreeSet<(u32, u32)> = links
            .iter()
            .map(|link| (link.output_port, link.input_port))
            .collect();
        assert_eq!(pairs, BTreeSet::from([(10, 900), (11, 901)]));
        assert!(links.iter().all(|link| link.output_node == 1));
    }

    // covers: S479
    #[test]
    fn the_capture_keeps_stream_links_where_the_monitor_would_take_too_much_or_too_little() {
        let filter = Filter::Except(50);
        // AudioSync plays into the default output.
        let (graph, processes) = playing_graph();
        let mut with_default = graph.clone();
        with_default.default_sink = Some("as-a".to_owned());
        assert_eq!(
            routed(&with_default, &processes, &filter),
            BTreeSet::from([22, 23])
        );
        // An output of AudioSync's that is not linked yet and names no
        // output may land on the default one.
        let (mut graph, processes) = monitored_graph();
        graph.links.retain(|_, link| link.output_node != 21);
        assert_eq!(
            routed(&graph, &processes, &filter),
            BTreeSet::from([22, 23])
        );
        // A capture of apps never takes a whole output.
        let (graph, processes) = monitored_graph();
        assert_eq!(
            routed(&graph, &processes, &Filter::Trees(vec![60])),
            BTreeSet::from([22, 23])
        );
    }

    // An app in another output is linked beside the monitor, and an
    // AudioSync output pinned to another output keeps the monitor even
    // before it is linked, so neither change moves the default output's
    // audio to another route.
    // covers: S479
    #[test]
    fn streams_in_other_outputs_are_linked_beside_the_monitor() {
        let filter = Filter::Except(50);
        let (mut graph, processes) = monitored_graph();
        for link in graph.links.values_mut() {
            if link.output_node == 23 {
                link.input_node = 2;
            }
        }
        assert_eq!(routed(&graph, &processes, &filter), BTreeSet::from([1, 23]));
        let (mut graph, processes) = monitored_graph();
        graph
            .nodes
            .get_mut(&2)
            .unwrap()
            .props
            .insert("object.serial".to_owned(), "202".to_owned());
        graph
            .nodes
            .get_mut(&21)
            .unwrap()
            .props
            .insert("target.object".to_owned(), "202".to_owned());
        graph.links.retain(|_, link| link.output_node != 21);
        assert_eq!(routed(&graph, &processes, &filter), BTreeSet::from([1]));
    }

    // covers: S479
    #[test]
    fn links_into_the_capture_itself_keep_the_monitor_route_open() {
        let (mut graph, processes) = monitored_graph();
        let made = graph.wanted_links(90, |id| id == 22 || id == 23);
        for (index, link) in made.into_iter().enumerate() {
            graph.links.insert(7000 + index as u32, link);
        }
        assert_eq!(
            routed(&graph, &processes, &Filter::Except(50)),
            BTreeSet::from([1])
        );
    }

    // covers: S10
    #[test]
    fn the_whole_system_leaves_out_audiosyncs_own_tree() {
        let (graph, processes) = playing_graph();
        let streams = captured(&graph, &processes, &Filter::Except(50));
        assert_eq!(streams, BTreeSet::from([22, 23]));
        let links = graph.wanted_links(90, |id| id == 22);
        assert_eq!(links.len(), 2);
        assert!(links.iter().all(|link| link.input_node == 90));
    }

    // covers: S380
    #[test]
    fn one_app_takes_its_tree_only() {
        let (graph, processes) = playing_graph();
        assert_eq!(
            captured(&graph, &processes, &Filter::Trees(vec![60])),
            BTreeSet::from([22, 23])
        );
        assert_eq!(
            captured(&graph, &processes, &Filter::Trees(vec![61])),
            BTreeSet::from([23])
        );
        assert!(captured(&graph, &processes, &Filter::Trees(vec![70])).is_empty());
    }

    // covers: S16
    #[test]
    fn mono_and_centre_go_to_both_sides_and_lfe_to_none() {
        assert_eq!(input_channels("MONO"), ["FL", "FR"]);
        assert_eq!(input_channels("FC"), ["FL", "FR"]);
        assert_eq!(input_channels("RL"), ["FL"]);
        assert_eq!(input_channels("SR"), ["FR"]);
        assert!(input_channels("LFE").is_empty());
    }

    // covers: S487
    #[test]
    fn a_running_capture_stream_from_a_source_is_an_app_listening() {
        let (mut graph, processes) = playing_graph();
        graph.add(3, Kind::Node, props(&[("media.class", "Audio/Source")]));
        // App 60's capture stream (node 30) from the source, AudioSync's own
        // (node 90) left unlinked.
        graph.add(
            30,
            Kind::Node,
            props(&[("media.class", "Stream/Input/Audio"), ("client.id", "122")]),
        );
        graph.add(
            3001,
            Kind::Link,
            props(&[
                ("link.output.node", "3"),
                ("link.output.port", "31"),
                ("link.input.node", "30"),
                ("link.input.port", "32"),
            ]),
        );
        assert_eq!(graph.listening_pids(&processes), Vec::<u32>::new());
        graph.nodes.get_mut(&30).unwrap().running = true;
        assert_eq!(graph.listening_pids(&processes), vec![60]);
        assert_eq!(graph.streams_of(&[60], &processes), vec![22]);
    }

    // covers: S487
    #[test]
    fn a_new_stream_on_a_reused_node_id_is_pointed_again() {
        let (mut graph, processes) = playing_graph();
        let serial = |graph: &mut Graph, value: &str| {
            let node = graph.nodes.get_mut(&22).unwrap();
            node.props.insert("object.serial".into(), value.into());
        };
        serial(&mut graph, "220");
        let mut targeted = BTreeSet::new();
        assert_eq!(
            graph.to_target(&mut targeted, &[60], &processes, true),
            vec![22]
        );
        assert_eq!(
            graph.to_target(&mut targeted, &[60], &processes, true),
            Vec::<u32>::new()
        );
        // The stream ended and PipeWire gave its ID to the app's next one.
        serial(&mut graph, "231");
        assert_eq!(
            graph.to_target(&mut targeted, &[60], &processes, true),
            vec![22]
        );
        assert_eq!(
            graph.to_target(&mut targeted, &[60], &processes, false),
            vec![22]
        );
        assert!(targeted.is_empty());
    }

    // covers: S382
    #[test]
    fn endpoint_audio_names_the_running_streams_on_each_output() {
        let (mut graph, processes) = playing_graph();
        graph.nodes.get_mut(&23).unwrap().running = false;
        graph.nodes.get_mut(&1).unwrap().volume = Some(endpoint_volume(true, &[0.125, 0.0]));
        let audio = graph.endpoint_audio(&processes);
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].active_pids, [50, 51, 60]);
        assert!(audio[0].virtual_device);
        assert!(!audio[0].audible());
        assert_eq!(graph.session_pids(&processes), [50, 51, 60, 61]);
    }

    #[test]
    fn volume_is_shown_on_the_cubic_scale() {
        let volume = endpoint_volume(false, &[0.125, 0.064]);
        assert!((volume.level - 0.5).abs() < 1e-6);
        assert!((linear_volume(0.5) - 0.125).abs() < 1e-6);
        assert_eq!(linear_volume(2.0), 1.0);
    }

    // covers: S472
    #[test]
    fn a_level_written_reads_back_as_the_same_remembered_level() {
        // A drift of 0.001 would read as a level changed in the system, and
        // the remembered level would never go back on.
        for percent in 0..=100_u8 {
            let level = f32::from(percent) / 100.0;
            let read = endpoint_volume(false, &[linear_volume(level); 2]).level;
            assert!((read - level).abs() < 1e-4, "{level} reads {read}");
        }
    }

    // covers: S381
    #[test]
    fn removing_a_sink_says_the_listing_changed() {
        let mut graph = Graph::default();
        sink(&mut graph, 1, &[("node.name", "as-a")]);
        graph.add(2, Kind::Port, props(&[("node.id", "1")]));
        assert!(!graph.remove(2));
        assert!(graph.remove(1));
        assert!(graph.listing().endpoints.is_empty());
    }

    // covers: S62
    #[test]
    fn an_output_plays_at_its_own_format_or_its_properties() {
        let mut graph = Graph::default();
        sink(
            &mut graph,
            1,
            &[("audio.rate", "44100"), ("audio.position", "FL,FR,FC")],
        );
        assert_eq!(graph.sink_format(1), (44_100, 3));
        graph.nodes.get_mut(&1).unwrap().format = Some((96_000, 2));
        assert_eq!(graph.sink_format(1), (96_000, 2));
        assert_eq!(graph.sink_format(9), (48_000, 2));
    }

    // covers: S62
    #[test]
    fn an_output_past_64_channels_plays_on_its_first_64() {
        let mut graph = Graph::default();
        sink(&mut graph, 1, &[("audio.channels", "128")]);
        assert_eq!(graph.sink_format(1), (48_000, 64));
        graph.nodes.get_mut(&1).unwrap().format = Some((96_000, 128));
        assert_eq!(graph.sink_format(1), (96_000, 64));
    }

    // covers: S232
    #[test]
    fn the_default_input_is_found_by_name() {
        let mut graph = Graph::default();
        graph.add(
            3,
            Kind::Node,
            props(&[
                ("media.class", "Audio/Source"),
                ("node.name", "sco"),
                ("factory.name", "api.bluez5.sco.source"),
            ]),
        );
        assert!(graph.default_input().is_none());
        graph.default_source = Some("sco".to_owned());
        assert_eq!(graph.default_input().map(|(id, _)| id), Some(3));
        assert!(graph.default_input_hands_free());
    }

    // covers: S14
    #[test]
    fn inputs_offer_every_channel_and_skip_hands_free() {
        let mut graph = Graph::default();
        graph.add(
            1,
            Kind::Node,
            props(&[
                ("media.class", "Audio/Source"),
                ("node.name", "mic"),
                ("audio.position", "[ FL FR RL RR ]"),
            ]),
        );
        graph.add(
            2,
            Kind::Node,
            props(&[
                ("media.class", "Audio/Source"),
                ("node.name", "sco"),
                ("factory.name", "api.bluez5.sco.source"),
            ]),
        );
        let inputs = graph.inputs();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].id.as_deref(), Ok("mic"));
        assert_eq!(inputs[0].properties.as_ref().unwrap().channels, Some(4));
    }

    // covers: S417
    #[test]
    fn a_saved_input_that_is_hands_free_or_missing_is_refused_as_not_connected() {
        let mut graph = Graph::default();
        graph.add(
            1,
            Kind::Node,
            props(&[("media.class", "Audio/Source"), ("node.name", "mic")]),
        );
        graph.add(
            2,
            Kind::Node,
            props(&[
                ("media.class", "Audio/Source"),
                ("node.name", "sco"),
                ("factory.name", "api.bluez5.sco.source"),
            ]),
        );
        assert!(graph.capture_input("mic").is_ok());
        assert_eq!(
            graph.capture_input("sco").err(),
            Some(BackendError::InputGone)
        );
        assert_eq!(
            graph.capture_input("gone").err(),
            Some(BackendError::InputGone)
        );
    }

    // covers: S14
    #[test]
    fn inputs_offer_at_most_64_channels() {
        let mut graph = Graph::default();
        graph.add(
            1,
            Kind::Node,
            props(&[
                ("media.class", "Audio/Source"),
                ("node.name", "madi"),
                ("audio.channels", "128"),
            ]),
        );
        let inputs = graph.inputs();
        assert_eq!(inputs[0].properties.as_ref().unwrap().channels, Some(64));
    }
}
