use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use pipewire::{
    self as pw,
    properties::properties,
    spa::{param::ParamType, pod::Pod, utils::dict::DictRef},
    types::ObjectType,
};
use tuyau_core::{Direction, Graph, Link, MediaKind, Node, NodeState, Port, PortPair, Volume};

use crate::{
    Command, Event,
    params::{Props, parse_props, serialize_props},
};

/// The core reports a broken connection with `-EPIPE`.
const EPIPE: i32 = 32;
const ENOENT: i32 = 2;
const PUBLISH_INTERVAL: Duration = Duration::from_millis(30);

struct Shared {
    graph: RefCell<Graph>,
    nodes: RefCell<HashMap<u32, (pw::node::Node, pw::node::NodeListener)>>,
    /// Link proxies kept until the server confirmed them with a sync.
    pending_links: RefCell<Vec<pw::link::Link>>,
    sync_tokens: RefCell<HashMap<i32, u64>>,
    dirty: Cell<bool>,
    events: async_channel::Sender<Event>,
}

impl Shared {
    fn emit(&self, event: Event) {
        let _ = self.events.try_send(event);
    }

    fn publish(&self, force: bool) {
        if self.dirty.replace(false) || force {
            self.emit(Event::Graph(Arc::new(self.graph.borrow().clone())));
        }
    }

    fn update(&self, change: impl FnOnce(&mut Graph)) {
        change(&mut self.graph.borrow_mut());
        self.dirty.set(true);
    }
}

pub fn run(
    commands: pw::channel::Receiver<Command>,
    events: async_channel::Sender<Event>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let connected = (|| -> Result<_, pw::Error> {
        let main_loop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&main_loop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;
        Ok((main_loop, context, core, registry))
    })();
    let (main_loop, _context, core, registry) = match connected {
        Ok(handles) => handles,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };

    let shared = Rc::new(Shared {
        graph: RefCell::default(),
        nodes: RefCell::default(),
        pending_links: RefCell::default(),
        sync_tokens: RefCell::default(),
        dirty: Cell::new(true),
        events,
    });

    let _core_listener = core
        .add_listener_local()
        .done({
            let shared = shared.clone();
            move |id, seq| {
                if id != pw::core::PW_ID_CORE {
                    return;
                }
                let Some(token) = shared.sync_tokens.borrow_mut().remove(&seq.seq()) else {
                    return;
                };
                shared.pending_links.borrow_mut().clear();
                shared.publish(true);
                shared.emit(Event::Synced(token));
            }
        })
        .error({
            let shared = shared.clone();
            let main_loop = main_loop.downgrade();
            move |id, _seq, result, message| {
                // Nodes without volume controls answer the Props subscription with ENOENT.
                if id != pw::core::PW_ID_CORE && result == -ENOENT {
                    log::debug!("object {id}: {message}");
                    return;
                }
                shared.emit(Event::Error(format!("object {id}: {message} ({result})")));
                if id == pw::core::PW_ID_CORE && result == -EPIPE {
                    shared.emit(Event::Disconnected);
                    if let Some(main_loop) = main_loop.upgrade() {
                        main_loop.quit();
                    }
                }
            }
        })
        .register();

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let shared = shared.clone();
            let registry = registry.downgrade();
            move |global| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    ObjectType::Node => {
                        let node = parse_node(global.id, props);
                        shared.update(|graph| {
                            graph.nodes.insert(node.id, node);
                        });
                        if let Some(registry) = registry.upgrade() {
                            bind_node(&shared, &registry, global);
                        }
                    }
                    ObjectType::Port => {
                        if let Some(port) = parse_port(global.id, props) {
                            shared.update(|graph| {
                                graph.ports.insert(port.id, port);
                            });
                        }
                    }
                    ObjectType::Link => {
                        if let Some(link) = parse_link(global.id, props) {
                            shared.update(|graph| {
                                graph.links.insert(link.id, link);
                            });
                        }
                    }
                    _ => {}
                }
            }
        })
        .global_remove({
            let shared = shared.clone();
            move |id| {
                shared.nodes.borrow_mut().remove(&id);
                shared.update(|graph| {
                    graph.nodes.remove(&id);
                    graph.ports.remove(&id);
                    graph.links.remove(&id);
                });
            }
        })
        .register();

    let timer = main_loop.loop_().add_timer({
        let shared = shared.clone();
        move |_| shared.publish(false)
    });
    timer.update_timer(Some(PUBLISH_INTERVAL), Some(PUBLISH_INTERVAL));

    let _commands = commands.attach(main_loop.loop_(), {
        let shared = shared.clone();
        let main_loop = main_loop.clone();
        let core = core.clone();
        let registry = registry.clone();
        move |command| handle_command(&shared, &main_loop, &core, &registry, command)
    });

    let _ = ready.send(Ok(()));
    main_loop.run();
    shared.emit(Event::Disconnected);
}

fn handle_command(
    shared: &Shared,
    main_loop: &pw::main_loop::MainLoopRc,
    core: &pw::core::CoreRc,
    registry: &pw::registry::RegistryRc,
    command: Command,
) {
    match command {
        Command::Link(pair) => match create_link(core, pair) {
            Ok(link) => shared.pending_links.borrow_mut().push(link),
            Err(error) => shared.emit(Event::Error(format!("cannot create link: {error}"))),
        },
        Command::Unlink(link_id) => {
            registry.destroy_global(link_id);
        }
        Command::SetVolume { node_id, channels } => set_props(
            shared,
            node_id,
            &Props {
                channel_volumes: Some(channels),
                mute: None,
            },
        ),
        Command::SetMute { node_id, mute } => set_props(
            shared,
            node_id,
            &Props {
                channel_volumes: None,
                mute: Some(mute),
            },
        ),
        Command::Sync(token) => match core.sync(0) {
            Ok(seq) => {
                shared.sync_tokens.borrow_mut().insert(seq.seq(), token);
            }
            Err(error) => shared.emit(Event::Error(format!("sync failed: {error}"))),
        },
        Command::Quit => main_loop.quit(),
    }
}

fn create_link(core: &pw::core::CoreRc, pair: PortPair) -> Result<pw::link::Link, pw::Error> {
    core.create_object::<pw::link::Link>(
        "link-factory",
        &properties! {
            "link.output.node" => pair.output_node.to_string(),
            "link.output.port" => pair.output_port.to_string(),
            "link.input.node" => pair.input_node.to_string(),
            "link.input.port" => pair.input_port.to_string(),
            "object.linger" => "true",
        },
    )
}

fn set_props(shared: &Shared, node_id: u32, props: &Props) {
    let nodes = shared.nodes.borrow();
    let Some((node, _)) = nodes.get(&node_id) else {
        shared.emit(Event::Error(format!("node {node_id} is not available")));
        return;
    };
    let bytes = serialize_props(props);
    let pod = Pod::from_bytes(&bytes).expect("serialized props are a pod");
    node.set_param(ParamType::Props, 0, pod);
}

fn bind_node(
    shared: &Rc<Shared>,
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&DictRef>,
) {
    let node: pw::node::Node = match registry.bind(global) {
        Ok(node) => node,
        Err(error) => {
            shared.emit(Event::Error(format!(
                "cannot bind node {}: {error}",
                global.id
            )));
            return;
        }
    };
    let node_id = global.id;
    let weak = Rc::downgrade(shared);
    let listener = node
        .add_listener_local()
        .info({
            let weak = weak.clone();
            move |info| {
                let changes = info.change_mask();
                let props_changed = changes.contains(pw::node::NodeChangeMask::PROPS);
                let state_changed = changes.contains(pw::node::NodeChangeMask::STATE);
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                if !props_changed && !state_changed {
                    return;
                }
                let state = node_state(info.state());
                shared.update(|graph| {
                    let Some(node) = graph.nodes.get_mut(&node_id) else {
                        return;
                    };
                    if state_changed {
                        node.state = state;
                    }
                    if let (true, Some(props)) = (props_changed, info.props()) {
                        merge_node(node, props);
                    }
                });
            }
        })
        .param(move |_seq, id, _index, _next, pod| {
            let (Some(shared), Some(pod), ParamType::Props) = (weak.upgrade(), pod, id) else {
                return;
            };
            let props = parse_props(pod);
            if props == Props::default() {
                return;
            }
            shared.update(|graph| {
                let Some(node) = graph.nodes.get_mut(&node_id) else {
                    return;
                };
                let volume = node.volume.get_or_insert_with(|| Volume {
                    channels: Vec::new(),
                    mute: false,
                });
                if let Some(channels) = props.channel_volumes {
                    volume.channels = channels;
                }
                if let Some(mute) = props.mute {
                    volume.mute = mute;
                }
            });
        })
        .register();
    node.subscribe_params(&[ParamType::Props]);
    shared.nodes.borrow_mut().insert(node_id, (node, listener));
}

fn text(props: &DictRef, key: &str) -> Option<String> {
    props
        .get(key)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn number(props: &DictRef, key: &str) -> Option<u32> {
    props.get(key)?.parse().ok()
}

fn flag(props: &DictRef, key: &str) -> bool {
    props.get(key) == Some("true")
}

fn parse_node(id: u32, props: &DictRef) -> Node {
    Node {
        id,
        name: text(props, "node.name").unwrap_or_else(|| format!("node-{id}")),
        description: text(props, "node.description"),
        nick: text(props, "node.nick"),
        media_class: text(props, "media.class"),
        application: text(props, "application.name"),
        media_name: text(props, "media.name"),
        properties: properties(props),
        ..Node::default()
    }
}

fn properties(props: &DictRef) -> BTreeMap<String, String> {
    props
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn node_state(state: pw::node::NodeState<'_>) -> NodeState {
    match state {
        pw::node::NodeState::Running => NodeState::Running,
        pw::node::NodeState::Idle => NodeState::Idle,
        pw::node::NodeState::Suspended => NodeState::Suspended,
        pw::node::NodeState::Error(_) => NodeState::Error,
        pw::node::NodeState::Creating => NodeState::Unknown,
    }
}

/// Applies a props update. Updates may carry only some keys, so missing ones keep
/// what the registry announced instead of falling back to `node-<id>`.
fn merge_node(node: &mut Node, props: &DictRef) {
    if let Some(name) = text(props, "node.name") {
        node.name = name;
    }
    for (field, key) in [
        (&mut node.description, "node.description"),
        (&mut node.nick, "node.nick"),
        (&mut node.media_class, "media.class"),
        (&mut node.application, "application.name"),
        (&mut node.media_name, "media.name"),
    ] {
        if let Some(value) = text(props, key) {
            *field = Some(value);
        }
    }
    node.properties.extend(properties(props));
}

fn parse_port(id: u32, props: &DictRef) -> Option<Port> {
    let direction = match props.get("port.direction")? {
        "in" => Direction::Input,
        "out" => Direction::Output,
        _ => return None,
    };
    Some(Port {
        id,
        node_id: number(props, "node.id")?,
        name: text(props, "port.name").unwrap_or_else(|| format!("port-{id}")),
        alias: text(props, "port.alias"),
        direction,
        channel: text(props, "audio.channel"),
        media: MediaKind::from_format_dsp(props.get("format.dsp")),
        physical: flag(props, "port.physical"),
        monitor: flag(props, "port.monitor"),
    })
}

fn parse_link(id: u32, props: &DictRef) -> Option<Link> {
    Some(Link {
        id,
        ports: PortPair {
            output_node: number(props, "link.output.node")?,
            output_port: number(props, "link.output.port")?,
            input_node: number(props, "link.input.node")?,
            input_port: number(props, "link.input.port")?,
        },
    })
}
