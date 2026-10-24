//! The graph thread, `audiosync-pw-graph`: one connection that follows the
//! registry for the outputs and inputs, the default devices, each output's
//! own mute and volume, and the apps' streams and links. The controller
//! reads a copy of the [`Graph`] and never waits for the daemon; volume
//! fixes the user clicks reach the thread through a PipeWire channel.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::node::{Node as NodeProxy, NodeListener, NodeState};
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::audio::AudioInfoRaw;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Property, Value, ValueArray};
use pw::spa::utils::SpaTypes;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;

use super::graph::{self, Graph, Kind, SINK_CLASS, STREAM_CLASS};
use crate::audio::backend::BackendError;
use crate::audio::routing::EndpointVolume;

/// How long the first full listing may take.
const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

/// The node name in a `default.audio.*` metadata value, `{ "name": "..." }`.
pub fn default_name(value: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    value.get("name")?.as_str().map(str::to_owned)
}

/// A dictionary's entries, owned.
pub fn owned(props: &DictRef) -> BTreeMap<String, String> {
    props
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// The graph kind of a registry object type.
pub fn kind(type_: &ObjectType) -> Option<Kind> {
    match type_ {
        ObjectType::Node => Some(Kind::Node),
        ObjectType::Port => Some(Kind::Port),
        ObjectType::Link => Some(Kind::Link),
        ObjectType::Client => Some(Kind::Client),
        _ => None,
    }
}

/// What the controller shares with the graph thread.
#[derive(Default)]
struct Shared {
    graph: Mutex<Graph>,
    synced: AtomicBool,
    outputs_changed: AtomicBool,
    failed: AtomicBool,
    watched: Mutex<BTreeSet<String>>,
    volume_changes: Mutex<BTreeSet<String>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A change to an output's own volume.
enum Change {
    Unmute,
    Level(f32),
}

enum Command {
    SetVolume {
        node: String,
        change: Change,
        reply: mpsc::Sender<Result<(), BackendError>>,
    },
    Stop,
}

/// The running graph thread. Dropping it stops the thread.
pub struct Registry {
    shared: Arc<Shared>,
    commands: pw::channel::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Registry {
    /// Starts the graph thread and waits, up to a few seconds, for the
    /// first full listing.
    pub fn start() -> Result<Registry, BackendError> {
        let shared = Arc::new(Shared::default());
        let (commands, receiver) = pw::channel::channel();
        let (ready, ready_receiver) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("audiosync-pw-graph".to_owned())
            .spawn(move || {
                if let Err(error) = serve(&thread_shared, receiver, ready.clone()) {
                    thread_shared.failed.store(true, Ordering::Release);
                    let _ = ready.send(Err(error));
                }
            })
            .map_err(|error| BackendError::System {
                call: "std::thread::spawn",
                code: error.raw_os_error().unwrap_or(0),
            })?;
        let registry = Registry {
            shared,
            commands,
            thread: Some(thread),
        };
        match ready_receiver.recv_timeout(SYNC_TIMEOUT) {
            Ok(Ok(())) => Ok(registry),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(BackendError::NotResponding),
        }
    }

    /// A copy of the graph; [`BackendError::NotResponding`] before the
    /// first full listing came, and [`BackendError::ServiceNotRunning`]
    /// once the daemon went away.
    pub fn graph(&self) -> Result<Graph, BackendError> {
        if self.shared.failed.load(Ordering::Acquire) {
            return Err(BackendError::ServiceNotRunning);
        }
        if !self.shared.synced.load(Ordering::Acquire) {
            return Err(BackendError::NotResponding);
        }
        Ok(locked(&self.shared.graph).clone())
    }

    pub fn take_outputs_changed(&self) -> bool {
        self.shared.outputs_changed.swap(false, Ordering::AcqRel)
    }

    pub fn watch(&self, id: &str) {
        locked(&self.shared.watched).insert(id.to_owned());
    }

    pub fn unwatch(&self, id: &str) {
        locked(&self.shared.watched).remove(id);
        locked(&self.shared.volume_changes).remove(id);
    }

    pub fn volume_changes(&self) -> Vec<String> {
        std::mem::take(&mut *locked(&self.shared.volume_changes))
            .into_iter()
            .collect()
    }

    pub fn volume(&self, id: &str) -> Result<EndpointVolume, BackendError> {
        let graph = self.graph()?;
        let (_, node) = graph.named(id).ok_or(BackendError::DeviceGone)?;
        node.volume.ok_or(BackendError::NotResponding)
    }

    fn set_volume(&self, id: &str, change: Change) -> Result<(), BackendError> {
        let (reply, answer) = mpsc::channel();
        self.commands
            .send(Command::SetVolume {
                node: id.to_owned(),
                change,
                reply,
            })
            .map_err(|_| BackendError::ServiceNotRunning)?;
        answer
            .recv_timeout(SYNC_TIMEOUT)
            .map_err(|_| BackendError::NotResponding)?
    }

    pub fn set_level(&self, id: &str, level: f32) -> Result<(), BackendError> {
        self.set_volume(id, Change::Level(level))
    }

    pub fn unmute(&self, id: &str) -> Result<(), BackendError> {
        self.set_volume(id, Change::Unmute)
    }
}

/// A node the thread bound: its proxy, its listener, and the channel count
/// of its last `Props` param.
struct Bound {
    proxy: NodeProxy,
    _listener: NodeListener,
    channels: Rc<RefCell<usize>>,
}

/// The mute and channel volumes in a `Props` param, when it has them.
fn read_props(param: &Pod) -> Option<(Option<bool>, Option<Vec<f32>>)> {
    let (_, Value::Object(object)) =
        PodDeserializer::deserialize_any_from(param.as_bytes()).ok()?
    else {
        return None;
    };
    let mut mute = None;
    let mut volumes = None;
    for property in object.properties {
        match (property.key, property.value) {
            (spa::sys::SPA_PROP_mute, Value::Bool(value)) => mute = Some(value),
            (spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(values))) => {
                volumes = Some(values);
            }
            _ => {}
        }
    }
    (mute.is_some() || volumes.is_some()).then_some((mute, volumes))
}

/// A `Props` param that sets `property`, serialized.
fn props_param(property: Property) -> Option<Vec<u8>> {
    let object = Value::Object(Object {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: ParamType::Props.as_raw(),
        properties: vec![property],
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .ok()
        .map(|(cursor, _)| cursor.into_inner())
}

fn serve(
    shared: &Arc<Shared>,
    commands: pw::channel::Receiver<Command>,
    ready: mpsc::Sender<Result<(), BackendError>>,
) -> Result<(), BackendError> {
    pw::init();
    let main_loop =
        pw::main_loop::MainLoopRc::new(None).map_err(|_| BackendError::ServiceNotRunning)?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|_| BackendError::ServiceNotRunning)?;
    let core = context
        .connect_rc(None)
        .map_err(|_| BackendError::ServiceNotRunning)?;
    let registry = core
        .get_registry_rc()
        .map_err(|_| BackendError::ServiceNotRunning)?;

    let bound: Rc<RefCell<HashMap<u32, Bound>>> = Rc::default();
    let defaults: Rc<RefCell<Option<(Metadata, MetadataListener)>>> = Rc::default();

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let registry = registry.clone();
            let shared = Arc::clone(shared);
            let bound = Rc::clone(&bound);
            let defaults = Rc::clone(&defaults);
            move |global| {
                let Some(props) = global.props else { return };
                if global.type_ == ObjectType::Metadata {
                    if props.get("metadata.name") == Some("default") && defaults.borrow().is_none()
                    {
                        *defaults.borrow_mut() = follow_defaults(&registry, global, &shared);
                    }
                    return;
                }
                let Some(kind) = kind(&global.type_) else {
                    return;
                };
                let props = owned(props);
                let class = props.get("media.class").cloned().unwrap_or_default();
                if locked(&shared.graph).add(global.id, kind, props) {
                    shared.outputs_changed.store(true, Ordering::Release);
                }
                let followed = class == SINK_CLASS || class == STREAM_CLASS;
                if kind == Kind::Node
                    && followed
                    && let Some(node) = bind_node(&registry, global, &shared, class == SINK_CLASS)
                {
                    bound.borrow_mut().insert(global.id, node);
                }
            }
        })
        .global_remove({
            let shared = Arc::clone(shared);
            let bound = Rc::clone(&bound);
            move |id| {
                bound.borrow_mut().remove(&id);
                if locked(&shared.graph).remove(id) {
                    shared.outputs_changed.store(true, Ordering::Release);
                }
            }
        })
        .register();

    // The first `done` after the registry's globals marks the listing whole.
    let pending = core.sync(0).map_err(|_| BackendError::ServiceNotRunning)?;
    let _core_listener = core
        .add_listener_local()
        .done({
            let shared = Arc::clone(shared);
            let ready = ready.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    shared.synced.store(true, Ordering::Release);
                    let _ = ready.send(Ok(()));
                }
            }
        })
        .error({
            let shared = Arc::clone(shared);
            let main_loop = main_loop.clone();
            move |id, _seq, _res, _message| {
                if id == pw::core::PW_ID_CORE {
                    shared.failed.store(true, Ordering::Release);
                    main_loop.quit();
                }
            }
        })
        .register();

    let _commands = commands.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        let shared = Arc::clone(shared);
        let bound = Rc::clone(&bound);
        move |command| match command {
            Command::Stop => main_loop.quit(),
            Command::SetVolume {
                node,
                change,
                reply,
            } => {
                let _ = reply.send(set_volume(&shared, &bound.borrow(), &node, &change));
            }
        }
    });
    main_loop.run();
    bound.borrow_mut().clear();
    defaults.borrow_mut().take();
    Ok(())
}

/// Writes `change` to the output named `name` as its node's `Props`.
fn set_volume(
    shared: &Shared,
    bound: &HashMap<u32, Bound>,
    name: &str,
    change: &Change,
) -> Result<(), BackendError> {
    let id = locked(&shared.graph)
        .named(name)
        .map(|(id, _)| id)
        .ok_or(BackendError::DeviceGone)?;
    let node = bound.get(&id).ok_or(BackendError::DeviceGone)?;
    let property = match *change {
        Change::Unmute => Property::new(spa::sys::SPA_PROP_mute, Value::Bool(false)),
        Change::Level(level) => {
            let channels = (*node.channels.borrow()).max(1);
            Property::new(
                spa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(vec![
                    graph::linear_volume(level);
                    channels
                ])),
            )
        }
    };
    let bytes = props_param(property).ok_or(BackendError::UnsupportedFormat)?;
    let pod = Pod::from_bytes(&bytes).ok_or(BackendError::UnsupportedFormat)?;
    node.proxy.set_param(ParamType::Props, 0, pod);
    Ok(())
}

/// Binds the node of `global` to follow its state, and for an output its
/// format and its own volume.
fn bind_node(
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&DictRef>,
    shared: &Arc<Shared>,
    output: bool,
) -> Option<Bound> {
    let proxy: NodeProxy = registry.bind(global).ok()?;
    let id = global.id;
    let channels = Rc::new(RefCell::new(0));
    let listener = proxy
        .add_listener_local()
        .info({
            let shared = Arc::clone(shared);
            move |info| {
                let running = matches!(info.state(), NodeState::Running);
                let mut graph = locked(&shared.graph);
                if let Some(node) = graph.nodes.get_mut(&id) {
                    node.running = running;
                    if output && let Some(props) = info.props() {
                        let props = owned(props);
                        if node.props != props {
                            node.props = props;
                            shared.outputs_changed.store(true, Ordering::Release);
                        }
                    }
                }
            }
        })
        .param({
            let shared = Arc::clone(shared);
            let channels = Rc::clone(&channels);
            move |_seq, kind, _index, _next, param| {
                let Some(param) = param else { return };
                let mut graph = locked(&shared.graph);
                let Some(node) = graph.nodes.get_mut(&id) else {
                    return;
                };
                match kind {
                    ParamType::Format => {
                        let mut info = AudioInfoRaw::new();
                        if info.parse(param).is_ok() && info.rate() > 0 {
                            let count = u16::try_from(info.channels()).unwrap_or(2);
                            node.format = Some((info.rate(), count));
                        }
                    }
                    ParamType::Props => {
                        let Some((mute, volumes)) = read_props(param) else {
                            return;
                        };
                        let previous = node.volume;
                        let mut muted = previous.is_some_and(|volume| volume.muted);
                        if let Some(mute) = mute {
                            muted = mute;
                        }
                        let volume = match volumes {
                            Some(volumes) if !volumes.is_empty() => {
                                *channels.borrow_mut() = volumes.len();
                                graph::endpoint_volume(muted, &volumes)
                            }
                            _ => EndpointVolume {
                                muted,
                                level: previous.map_or(1.0, |volume| volume.level),
                            },
                        };
                        node.volume = Some(volume);
                        if previous != Some(volume) {
                            let name = node.name().to_owned();
                            drop(graph);
                            if locked(&shared.watched).contains(&name) {
                                locked(&shared.volume_changes).insert(name);
                            }
                        }
                    }
                    _ => {}
                }
            }
        })
        .register();
    if output {
        proxy.subscribe_params(&[ParamType::Props, ParamType::Format]);
    }
    Some(Bound {
        proxy,
        _listener: listener,
        channels,
    })
}

/// Binds the `default` metadata and keeps the default output and input in
/// the graph.
fn follow_defaults(
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&DictRef>,
    shared: &Arc<Shared>,
) -> Option<(Metadata, MetadataListener)> {
    let metadata: Metadata = registry.bind(global).ok()?;
    let shared = Arc::clone(shared);
    let listener = metadata
        .add_listener_local()
        .property(move |subject, key, _type, value| {
            if subject != pw::core::PW_ID_CORE {
                return 0;
            }
            let name = value.and_then(default_name);
            let mut graph = locked(&shared.graph);
            match key {
                Some("default.audio.sink") => {
                    if graph.default_sink != name {
                        graph.default_sink = name;
                        shared.outputs_changed.store(true, Ordering::Release);
                    }
                }
                Some("default.audio.source") => graph.default_source = name,
                // Every key cleared at once.
                None => {
                    graph.default_sink = None;
                    graph.default_source = None;
                    shared.outputs_changed.store(true, Ordering::Release);
                }
                _ => {}
            }
            0
        })
        .register();
    Some((metadata, listener))
}
