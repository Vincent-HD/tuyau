use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use gpui::{
    AnyElement, AnyView, App, AppContext, Context, FocusHandle, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, PathBuilder, Pixels, Point, Render, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, div, prelude::FluentBuilder, px,
};
use tuyau_core::{
    Config, Direction, Graph, Link, LinkRule, MediaKind, Node, NodeState, Port, PortPair, Position,
    Settings, State, Ui, View,
    graph::level_to_linear,
    layout::{self, NODE_WIDTH},
};
use tuyau_pipewire::{Command, CommandSender, Connection, Event, Reconciler};

use crate::{
    Options, Redo, Undo,
    geometry::{
        self, HANDLE_SIZE, HEADER_HEIGHT, MUTE_WIDTH, PADDING, PERCENT_WIDTH, PORT_ROW_HEIGHT,
        SLIDER_GAP, SUBTITLE_HEIGHT, VOLUME_HEIGHT,
    },
    history::{Edit, History, Layout},
    theme::{self, Theme},
};

pub const TOOLBAR_HEIGHT: f32 = 44.0;
const VOLUME_STEP: f32 = 0.05;
/// Two roundtrips: the first lists every object, the second their volumes.
const INITIAL_SYNCS: u64 = 2;
const MIN_ZOOM: f32 = 0.2;
const MAX_ZOOM: f32 = 2.0;
/// Zoom change per pixel of ctrl-scroll; one wheel notch is about 8%.
const ZOOM_SPEED: f32 = 0.002;
const FIT_MARGIN: f32 = 24.0;
/// Card text sizes at 100% zoom: GPUI's `text_sm` and `text_xs`.
const TEXT_TITLE: f32 = 14.0;
const TEXT_SMALL: f32 = 12.0;
/// How close a right-click must be to a wire to pick it, in screen pixels.
const WIRE_REACH: f32 = 8.0;
const INSPECTOR_WIDTH: f32 = 420.0;
const INSPECTOR_HEIGHT: f32 = 460.0;
const INSPECTOR_KEY_WIDTH: f32 = 170.0;

enum Drag {
    Node {
        node_id: u32,
        grab: Position,
        start: Position,
    },
    Pan {
        last: Point<Pixels>,
    },
    Wire {
        from_port: u32,
        pointer: Position,
    },
    Volume {
        node_id: u32,
    },
}

#[derive(Default, PartialEq)]
struct Shape {
    nodes: BTreeSet<u32>,
    edges: BTreeSet<(u32, u32)>,
}

struct WireMenu {
    pair: PortPair,
    /// Where the click happened, in window coordinates.
    at: Point<Pixels>,
}

struct Rename {
    node_id: u32,
    text: String,
}

pub struct Patchbay {
    options: Options,
    _connection: Connection,
    commands: CommandSender,
    graph: Arc<Graph>,
    settings: Settings,
    reconciler: Reconciler,
    history: History,
    /// The node whose properties panel is open.
    inspected: Option<u32>,
    /// The menu opened by right-clicking a wire.
    wire_menu: Option<WireMenu>,
    /// Nothing is placed before PipeWire has listed the whole graph, or every node
    /// would look unlinked.
    settled: bool,
    auto_connect: bool,
    /// The graph shape last laid out by auto-arrange.
    shape: Shape,
    positions: BTreeMap<u32, Position>,
    pan: Point<Pixels>,
    zoom: f32,
    drag: Option<Drag>,
    rename: Option<Rename>,
    media_filter: BTreeSet<MediaKind>,
    status: Option<String>,
    focus: FocusHandle,
}

impl Patchbay {
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn new(options: Options, connection: Connection, cx: &mut Context<Self>) -> Self {
        let (settings, status) = match load_settings(&options) {
            Ok(settings) => (settings, None),
            Err(error) => (Settings::default(), Some(error.to_string())),
        };
        let view = settings.ui.view.unwrap_or(View {
            x: 24.0,
            y: 24.0,
            zoom: 1.0,
        });
        let events = connection.events();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let mut latest = event;
                // Skip stale snapshots when several queued up.
                while let Ok(next) = events.try_recv() {
                    if matches!((&latest, &next), (Event::Graph(_), Event::Graph(_))) {
                        latest = next;
                        continue;
                    }
                    if this
                        .update(cx, |this, cx| this.on_event(latest, cx))
                        .is_err()
                    {
                        return;
                    }
                    latest = next;
                }
                if this
                    .update(cx, |this, cx| this.on_event(latest, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();

        connection.send(Command::Sync(1));

        Self {
            commands: connection.commands(),
            _connection: connection,
            options,
            graph: Arc::default(),
            settings,
            reconciler: Reconciler::default(),
            history: History::default(),
            inspected: None,
            wire_menu: None,
            auto_connect: true,
            shape: Shape::default(),
            settled: false,
            positions: BTreeMap::new(),
            pan: Point::new(px(view.x), px(view.y)),
            zoom: view.zoom.clamp(MIN_ZOOM, MAX_ZOOM),
            drag: None,
            rename: None,
            media_filter: [
                MediaKind::Audio,
                MediaKind::Midi,
                MediaKind::Video,
                MediaKind::Other,
            ]
            .into_iter()
            .collect(),
            status,
            focus: cx.focus_handle(),
        }
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Graph(graph) => {
                self.graph = graph;
                self.place_new_nodes();
                self.reconcile();
            }
            Event::Error(message) => self.status = Some(message),
            Event::Disconnected => self.status = Some("disconnected from PipeWire".into()),
            Event::Synced(token) if !self.settled => {
                if token < INITIAL_SYNCS {
                    self.commands.send(Command::Sync(token + 1));
                } else {
                    self.settled = true;
                    self.place_new_nodes();
                    self.reconcile();
                }
            }
            Event::Synced(_) => {}
        }
        cx.notify();
    }

    /// Links are only planned against the complete graph, so existing ones are not
    /// requested again.
    fn reconcile(&mut self) {
        if self.auto_connect && self.settled {
            self.reconciler
                .reconcile(&self.graph, &self.settings, &self.commands);
        }
    }

    fn place_new_nodes(&mut self) {
        if !self.settled {
            return;
        }
        if self.settings.ui.auto_arrange {
            let shape = self.shape();
            if shape != self.shape {
                let graph = &self.graph;
                self.positions =
                    layout::arrange(graph, &self.settings, &geometry::Cards(graph), |_| None);
                self.shape = shape;
            }
            return;
        }
        let current = &self.positions;
        let settings = &self.settings;
        let graph = &self.graph;
        self.positions = layout::arrange(graph, settings, &geometry::Cards(graph), |node| {
            current
                .get(&node.id)
                .copied()
                .or_else(|| settings.position(node))
        });
    }

    /// What the auto layout depends on: which nodes are shown and which feed which.
    fn shape(&self) -> Shape {
        let nodes = self
            .graph
            .ports
            .values()
            .map(|port| port.node_id)
            .filter(|id| {
                self.graph
                    .nodes
                    .get(id)
                    .is_some_and(|node| !self.settings.is_hidden(node))
            })
            .collect();
        let edges = self
            .graph
            .links
            .values()
            .map(|link| (link.ports.output_node, link.ports.input_node))
            .collect();
        Shape { nodes, edges }
    }

    fn toggle_auto_arrange(&mut self, window: &Window, cx: &mut Context<Self>) {
        let ui = &mut self.settings.ui;
        ui.auto_arrange = !ui.auto_arrange;
        if ui.auto_arrange {
            // Starts from a fresh layout, which Ctrl+Z can take back.
            self.arrange(window, cx);
            self.shape = self.shape();
        } else {
            self.save_ui();
        }
        cx.notify();
    }

    fn arrange(&mut self, window: &Window, cx: &mut Context<Self>) {
        let before = self.layout();
        let graph = &self.graph;
        let shown = layout::arrange(graph, &self.settings, &geometry::Cards(graph), |_| None);
        self.edit(Edit::Layout {
            before,
            after: Layout {
                saved: BTreeMap::new(),
                shown,
            },
        });
        self.fit(window, cx);
    }

    fn layout(&self) -> Layout {
        Layout {
            saved: self.settings.ui.positions.clone(),
            shown: self.positions.clone(),
        }
    }

    /// Records where the view is, so the next launch opens on it.
    pub fn save_view(&mut self) {
        self.settings.ui.view = Some(View {
            x: f32::from(self.pan.x),
            y: f32::from(self.pan.y),
            zoom: self.zoom,
        });
        self.save_ui();
    }

    /// Zooms out (never in past 100%) and pans so every visible node is on screen.
    fn fit(&mut self, window: &Window, cx: &mut Context<Self>) {
        let bounds = self
            .graph
            .nodes
            .values()
            .filter(|node| self.visible(node))
            .filter_map(|node| {
                let position = self.positions.get(&node.id)?;
                let height = geometry::node_height(&self.graph, node);
                Some((
                    position.x,
                    position.y,
                    position.x + NODE_WIDTH,
                    position.y + height,
                ))
            })
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)));
        let Some((left, top, right, bottom)) = bounds else {
            return;
        };
        let viewport = window.viewport_size();
        let width = f32::from(viewport.width) - 2.0 * FIT_MARGIN;
        let height = f32::from(viewport.height) - TOOLBAR_HEIGHT - 2.0 * FIT_MARGIN;
        self.zoom = (width / (right - left))
            .min(height / (bottom - top))
            .clamp(MIN_ZOOM, 1.0);
        self.pan = Point::new(
            px(FIT_MARGIN + (width - (right - left) * self.zoom).max(0.0) / 2.0 - left * self.zoom),
            px(FIT_MARGIN - top * self.zoom),
        );
        cx.notify();
    }

    /// Sets the zoom while keeping the world point under `anchor` (window
    /// coordinates) in place.
    fn zoom_at(&mut self, zoom: f32, anchor: Point<Pixels>) {
        let world = self.to_world(anchor);
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.pan = Point::new(
            anchor.x - px(world.x * self.zoom),
            anchor.y - px(TOOLBAR_HEIGHT + world.y * self.zoom),
        );
    }

    /// A length in world units, scaled to the screen.
    fn scaled(&self, length: f32) -> Pixels {
        px(length * self.zoom)
    }

    fn save_state(&mut self) {
        if let Err(error) = self.settings.state.save(&self.options.state_path) {
            self.status = Some(error.to_string());
        }
    }

    fn save_ui(&mut self) {
        let path = Ui::path_for(&self.options.state_path);
        if let Err(error) = self.settings.ui.save(&path) {
            self.status = Some(error.to_string());
        }
    }

    fn reload_config(&mut self, cx: &mut Context<Self>) {
        match Config::load(&self.options.config_path) {
            Ok(config) => {
                self.settings.config = config;
                self.reconciler.reset();
                self.status = Some(format!("reloaded {}", self.options.config_path.display()));
                self.place_new_nodes();
                if self.auto_connect {
                    self.reconciler
                        .reconcile(&self.graph, &self.settings, &self.commands);
                }
            }
            Err(error) => self.status = Some(error.to_string()),
        }
        cx.notify();
    }

    fn to_world(&self, point: Point<Pixels>) -> Position {
        Position {
            x: f32::from(point.x - self.pan.x) / self.zoom,
            y: (f32::from(point.y - self.pan.y) - TOOLBAR_HEIGHT) / self.zoom,
        }
    }

    fn to_screen(&self, position: Position) -> Point<Pixels> {
        Point::new(
            self.scaled(position.x) + self.pan.x,
            self.scaled(position.y) + self.pan.y,
        )
    }

    fn visible(&self, node: &Node) -> bool {
        self.positions.contains_key(&node.id)
            && self.media_filter.contains(&self.graph.node_media(node.id))
    }

    fn volume(&self, node_id: u32) -> Option<&tuyau_core::Volume> {
        self.graph.nodes.get(&node_id)?.volume.as_ref()
    }

    fn set_level(&mut self, node_id: u32, level: f32) {
        let Some(volume) = self.volume(node_id) else {
            return;
        };
        let before = volume.channels.clone();
        let after = vec![level_to_linear(level); before.len().max(1)];
        self.edit(Edit::Volume {
            node_id,
            before,
            after,
        });
    }

    fn toggle_mute(&mut self, node_id: u32) {
        let Some(volume) = self.volume(node_id) else {
            return;
        };
        let mute = !volume.mute;
        self.edit(Edit::Mute { node_id, mute });
    }

    /// Links the two ports, or unlinks them when they already are.
    fn toggle_link(&mut self, first: u32, second: u32) {
        let (Some(a), Some(b)) = (self.graph.ports.get(&first), self.graph.ports.get(&second))
        else {
            return;
        };
        let (output, input) = match (a.direction, b.direction) {
            (Direction::Output, Direction::Input) => (a.id, b.id),
            (Direction::Input, Direction::Output) => (b.id, a.id),
            _ => return,
        };
        if let Some(link) = self.graph.find_link(output, input) {
            self.edit(Edit::Unlink(link.ports));
            return;
        }
        if let Some(pair) = self.graph.pair(output, input) {
            self.edit(Edit::Link(pair));
        }
    }

    /// Applies an edit made by the user and records it for undo.
    fn edit(&mut self, edit: Edit) {
        self.apply(&edit);
        self.history.record(edit);
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.rename.is_some() {
            return;
        }
        match self.history.undo() {
            Some(edit) => self.apply(&edit),
            None => self.status = Some("nothing to undo".into()),
        }
        cx.notify();
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.rename.is_some() {
            return;
        }
        match self.history.redo() {
            Some(edit) => self.apply(&edit),
            None => self.status = Some("nothing to redo".into()),
        }
        cx.notify();
    }

    /// Carries out an edit without recording it. Edits on nodes or ports that are
    /// gone since are skipped.
    fn apply(&mut self, edit: &Edit) {
        match edit {
            Edit::Link(pair) => {
                if let Some(rule) = self.link_rule(pair)
                    && !self.settings.state.links.contains(&rule)
                {
                    self.settings.state.links.push(rule);
                    self.save_state();
                }
                if self.auto_connect {
                    // The reconciler creates it from the saved rule, once.
                    self.reconcile();
                } else if self
                    .graph
                    .find_link(pair.output_port, pair.input_port)
                    .is_none()
                {
                    self.commands.send(Command::Link(*pair));
                }
            }
            Edit::Unlink(pair) => {
                if let Some(rule) = self.link_rule(pair) {
                    let before = self.settings.state.links.len();
                    self.settings.state.links.retain(|saved| saved != &rule);
                    if self.settings.state.links.len() != before {
                        self.save_state();
                    }
                }
                if let Some(link) = self.graph.find_link(pair.output_port, pair.input_port) {
                    self.commands.send(Command::Unlink(link.id));
                }
            }
            Edit::Volume { node_id, after, .. } => {
                let graph = Arc::make_mut(&mut self.graph);
                if let Some(volume) = graph
                    .nodes
                    .get_mut(node_id)
                    .and_then(|node| node.volume.as_mut())
                {
                    volume.channels = after.clone();
                    self.commands.send(Command::SetVolume {
                        node_id: *node_id,
                        channels: after.clone(),
                    });
                }
            }
            Edit::Mute { node_id, mute } => {
                let graph = Arc::make_mut(&mut self.graph);
                if let Some(volume) = graph
                    .nodes
                    .get_mut(node_id)
                    .and_then(|node| node.volume.as_mut())
                {
                    volume.mute = *mute;
                    self.commands.send(Command::SetMute {
                        node_id: *node_id,
                        mute: *mute,
                    });
                }
            }
            Edit::Move { name, after, .. } => {
                let ids: Vec<u32> = self
                    .graph
                    .nodes
                    .values()
                    .filter(|node| &node.name == name)
                    .map(|node| node.id)
                    .collect();
                for id in ids {
                    self.positions.insert(id, *after);
                }
                self.settings.ui.positions.insert(name.clone(), *after);
                self.save_ui();
            }
            Edit::Alias { name, after, .. } => {
                match after {
                    Some(alias) => self
                        .settings
                        .state
                        .aliases
                        .insert(name.clone(), alias.clone()),
                    None => self.settings.state.aliases.remove(name),
                };
                self.save_state();
            }
            Edit::Hidden { names, hidden } => {
                for name in names {
                    if *hidden {
                        self.settings.state.hidden.insert(name.clone());
                    } else {
                        self.settings.state.hidden.remove(name);
                    }
                }
                if *hidden {
                    let graph = &self.graph;
                    self.positions.retain(|id, _| {
                        graph
                            .nodes
                            .get(id)
                            .is_none_or(|node| !names.contains(&node.name))
                    });
                }
                self.save_state();
                self.place_new_nodes();
            }
            Edit::Layout { after, .. } => {
                self.settings.ui.positions = after.saved.clone();
                self.positions = after.shown.clone();
                self.save_ui();
                // Nodes that appeared since get a place next to their neighbours.
                self.place_new_nodes();
            }
        }
    }

    /// The saved rule that keeps these two ports linked, by node and port name.
    fn link_rule(&self, pair: &PortPair) -> Option<LinkRule> {
        let graph = &self.graph;
        Some(LinkRule::exact(
            graph.nodes.get(&pair.output_node)?,
            graph.ports.get(&pair.output_port)?,
            graph.nodes.get(&pair.input_node)?,
            graph.ports.get(&pair.input_port)?,
        ))
    }

    /// Dropping a wire on a card picks the port on the same channel.
    fn link_to_node(&mut self, from_port: u32, node_id: u32) {
        let Some(from) = self.graph.ports.get(&from_port) else {
            return;
        };
        let direction = match from.direction {
            Direction::Output => Direction::Input,
            Direction::Input => Direction::Output,
        };
        let candidates: Vec<&Port> = self
            .graph
            .ports_of(node_id, direction)
            .filter(|port| port.media == from.media && !port.monitor)
            .collect();
        let row = self
            .graph
            .ports_of(from.node_id, from.direction)
            .position(|port| port.id == from_port)
            .unwrap_or(0);
        let target = candidates
            .iter()
            .find(|port| port.channel.is_some() && port.channel == from.channel)
            .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))
            .or_else(|| candidates.get(row))
            .map(|port| port.id);
        if let Some(target) = target {
            self.toggle_link(from_port, target);
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag.is_some() && event.pressed_button.is_none() {
            self.end_drag();
            cx.notify();
            return;
        }
        let pointer = self.to_world(event.position);
        match &mut self.drag {
            None => return,
            Some(Drag::Node { node_id, grab, .. }) => {
                self.positions.insert(
                    *node_id,
                    Position {
                        x: pointer.x - grab.x,
                        y: pointer.y - grab.y,
                    },
                );
            }
            Some(Drag::Pan { last }) => {
                self.pan += event.position - *last;
                *last = event.position;
            }
            Some(Drag::Wire {
                pointer: wire_end, ..
            }) => *wire_end = pointer,
            Some(Drag::Volume { node_id }) => {
                let node_id = *node_id;
                if let Some(position) = self.positions.get(&node_id).copied() {
                    self.set_level(node_id, geometry::slider_level(position, pointer.x));
                }
            }
        }
        cx.notify();
    }

    fn end_drag(&mut self) {
        let Some(Drag::Node { node_id, start, .. }) = self.drag.take() else {
            return;
        };
        let (Some(node), Some(position)) =
            (self.graph.nodes.get(&node_id), self.positions.get(&node_id))
        else {
            return;
        };
        if *position != start {
            self.edit(Edit::Move {
                name: node.name.clone(),
                before: start,
                after: *position,
            });
        }
    }

    fn start_rename(&mut self, node_id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.graph.nodes.get(&node_id) else {
            return;
        };
        self.rename = Some(Rename {
            node_id,
            text: self.settings.display_name(node).to_owned(),
        });
        self.focus.focus(window, cx);
    }

    fn commit_rename(&mut self) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let Some(node) = self.graph.nodes.get(&rename.node_id) else {
            return;
        };
        let text = rename.text.trim();
        let after = (!text.is_empty() && text != node.label()).then(|| text.to_owned());
        let before = self.settings.state.aliases.get(&node.name).cloned();
        if before != after {
            self.edit(Edit::Alias {
                name: node.name.clone(),
                before,
                after,
            });
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = &mut self.rename else {
            if event.keystroke.key == "escape"
                && (self.wire_menu.take().is_some() || self.inspected.take().is_some())
            {
                cx.notify();
            }
            return;
        };
        match event.keystroke.key.as_str() {
            "enter" => self.commit_rename(),
            "escape" => self.rename = None,
            "backspace" => {
                rename.text.pop();
            }
            _ => {
                if let Some(text) = &event.keystroke.key_char {
                    rename.text.push_str(text);
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn hide(&mut self, node_id: u32, cx: &mut Context<Self>) {
        if let Some(node) = self.graph.nodes.get(&node_id) {
            self.edit(Edit::Hidden {
                names: BTreeSet::from([node.name.clone()]),
                hidden: true,
            });
        }
        cx.notify();
    }

    fn show_hidden(&mut self, cx: &mut Context<Self>) {
        let names = self.settings.state.hidden.clone();
        if !names.is_empty() {
            self.edit(Edit::Hidden {
                names,
                hidden: false,
            });
        }
        cx.notify();
    }

    fn hidden_count(&self) -> usize {
        self.graph
            .nodes
            .values()
            .filter(|node| self.settings.is_hidden(node))
            .count()
    }

    fn render_toolbar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let filters = [
            (MediaKind::Audio, "Audio"),
            (MediaKind::Midi, "MIDI"),
            (MediaKind::Video, "Video"),
        ];
        let hidden = self.hidden_count();
        div()
            .h(px(TOOLBAR_HEIGHT))
            .flex_shrink_0()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme.toolbar)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .font_weight(gpui::FontWeight::BOLD)
                    .mr_2()
                    .child("tuyau"),
            )
            .children(filters.into_iter().map(|(media, label)| {
                let active = self.media_filter.contains(&media);
                button(
                    theme,
                    SharedString::from(format!("filter-{label}")),
                    label,
                    active,
                )
                .border_color(if active {
                    theme.media(media)
                } else {
                    theme.border
                })
                .tooltip(tip(theme, "Show or hide the nodes of this media type."))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.media_filter.remove(&media) {
                        this.media_filter.insert(media);
                    }
                    cx.notify();
                }))
            }))
            .child(div().w(px(1.0)).h(px(20.0)).bg(theme.border).mx_1())
            .child(
                button(theme, "arrange", "Arrange", false)
                    .tooltip(tip(
                        theme,
                        "Lay every node out again along the signal flow, forgetting \
                         manual positions. Ctrl+Z takes it back.",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.arrange(window, cx))),
            )
            .child(
                button(theme, "fit", "Fit", false)
                    .tooltip(tip(theme, "Zoom and pan so the whole graph is on screen."))
                    .on_click(cx.listener(|this, _, window, cx| this.fit(window, cx))),
            )
            .child(
                button(
                    theme,
                    "zoom",
                    SharedString::from(format!("{:.0}%", self.zoom * 100.0)),
                    false,
                )
                .tooltip(tip(
                    theme,
                    "Zoom level. Click to reset to 100%; Ctrl+scroll to zoom.",
                ))
                .on_click(cx.listener(|this, _, window, cx| {
                    let viewport = window.viewport_size();
                    let center = Point::new(
                        viewport.width / 2.0,
                        (viewport.height + px(TOOLBAR_HEIGHT)) / 2.0,
                    );
                    this.zoom_at(1.0, center);
                    cx.notify();
                })),
            )
            .child(
                button(
                    theme,
                    "auto-arrange",
                    "Auto-arrange",
                    self.settings.ui.auto_arrange,
                )
                .tooltip(tip(
                    theme,
                    "Lay the graph out again whenever nodes or links come and go, so \
                     short-lived streams land in place.",
                ))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_auto_arrange(window, cx))),
            )
            .child(
                button(theme, "auto-connect", "Auto-connect", self.auto_connect)
                    .tooltip(tip(
                        theme,
                        "Keep the [[link]] rules of your config in place: recreate them \
                         when their nodes appear, apply exclusive rules and initial \
                         volumes. Like `tuyau watch`. Off: tuyau changes nothing by itself.",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.auto_connect = !this.auto_connect;
                        if this.auto_connect {
                            this.reconciler.reset();
                            this.reconciler
                                .reconcile(&this.graph, &this.settings, &this.commands);
                        }
                        cx.notify();
                    })),
            )
            .child(
                button(theme, "reload", "Reload config", false)
                    .tooltip(tip(
                        theme,
                        "Read config.toml again after editing it (or after a Home Manager switch).",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.reload_config(cx))),
            )
            .when(hidden > 0, |toolbar| {
                toolbar.child(
                    button(
                        theme,
                        "show-hidden",
                        SharedString::from(format!("Show hidden ({hidden})")),
                        false,
                    )
                    .tooltip(tip(
                        theme,
                        "Bring back the nodes hidden from the GUI. Nodes hidden by the \
                         config stay hidden.",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.show_hidden(cx))),
                )
            })
            .child(div().flex_1())
            .when_some(self.status.clone(), |toolbar, status| {
                toolbar.child(
                    div()
                        .id("status")
                        .max_w(px(420.0))
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_text)
                        .cursor_pointer()
                        .child(status)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.status = None;
                            cx.notify();
                        })),
                )
            })
    }

    fn render_wires(&self, theme: &Theme) -> impl IntoElement {
        let slots = theme::wire_slots(&self.graph);
        let targeted = self.wire_menu.as_ref().map(|menu| menu.pair);
        // (from, to, color, highlighted)
        let mut ends: Vec<(Position, Position, Hsla, bool)> = self
            .graph
            .links
            .values()
            .filter_map(|link| {
                let (from, to) = self.link_ends(&link.ports)?;
                Some(if targeted == Some(link.ports) {
                    (from, to, gpui::rgb_to_hsla(theme.danger), true)
                } else {
                    (from, to, self.wire_color(link, &slots, theme)?, false)
                })
            })
            .collect();
        // The wire a menu is open on is drawn last, over the others.
        ends.sort_by_key(|(_, _, _, highlighted)| *highlighted);
        if let Some(Drag::Wire { from_port, pointer }) = &self.drag
            && let Some(anchor) = self.port_anchor(*from_port)
        {
            let port = &self.graph.ports[from_port];
            let (from, to) = match port.direction {
                Direction::Output => (anchor, *pointer),
                Direction::Input => (*pointer, anchor),
            };
            ends.push((from, to, gpui::rgb_to_hsla(theme.accent), false));
        }
        // Curves are shaped in world space so remove handles stay on them at any zoom.
        let stroke = self.scaled(2.0).max(px(1.0));
        let wires: Vec<_> = ends
            .into_iter()
            .map(|(from, to, color, highlighted)| {
                let (a, b) = geometry::wire_controls(from, to);
                let [from, a, b, to] = [from, a, b, to].map(|point| self.to_screen(point));
                let width = if highlighted { stroke * 2.0 } else { stroke };
                (from, a, b, to, color, width)
            })
            .collect();
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                for (from, a, b, to, color, width) in wires {
                    let mut path = PathBuilder::stroke(width);
                    path.move_to(bounds.origin + from);
                    path.cubic_bezier_to(bounds.origin + to, bounds.origin + a, bounds.origin + b);
                    if let Ok(path) = path.build() {
                        window.paint_path(path, color);
                    }
                }
            },
        )
        .absolute()
        .size_full()
    }

    fn port_anchor(&self, port_id: u32) -> Option<Position> {
        let port = self.graph.ports.get(&port_id)?;
        let node = self.graph.nodes.get(&port.node_id)?;
        if !self.visible(node) {
            return None;
        }
        geometry::port_anchor(&self.graph, node, *self.positions.get(&node.id)?, port)
    }

    fn wire_color(&self, link: &Link, slots: &BTreeMap<u32, usize>, theme: &Theme) -> Option<Hsla> {
        let output = self.graph.ports.get(&link.ports.output_port)?;
        let channel = output.channel.as_deref().or_else(|| {
            self.graph
                .ports
                .get(&link.ports.input_port)?
                .channel
                .as_deref()
        });
        let slot = slots.get(&link.ports.output_node).copied().unwrap_or(0);
        Some(theme.wire(output.media, slot, channel))
    }

    fn link_ends(&self, pair: &PortPair) -> Option<(Position, Position)> {
        Some((
            self.port_anchor(pair.output_port)?,
            self.port_anchor(pair.input_port)?,
        ))
    }

    fn render_wire_handles(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let slots = theme::wire_slots(&self.graph);
        let wires: Vec<_> = self
            .graph
            .links
            .values()
            .filter_map(|link| {
                let ends = self.link_ends(&link.ports)?;
                Some((link.clone(), self.wire_color(link, &slots, theme)?, ends))
            })
            .collect();
        let ends: Vec<_> = wires.iter().map(|(_, _, ends)| *ends).collect();
        wires
            .iter()
            .zip(geometry::wire_handles(&ends))
            .map(|((link, color, _), handle)| {
                let (link_id, pair) = (link.id, link.ports);
                let color = *color;
                let center = self.to_screen(handle);
                let radius = self.scaled(HANDLE_SIZE / 2.0);
                div()
                    .id(("wire", link_id as usize))
                    .absolute()
                    .left(center.x - radius)
                    .top(center.y - radius)
                    .size(self.scaled(HANDLE_SIZE))
                    .rounded_full()
                    .bg(theme.background)
                    .border_2()
                    .border_color(color)
                    .cursor_pointer()
                    .hover_style(|style| style.bg(theme.danger).border_color(theme.danger))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.edit(Edit::Unlink(pair));
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_node(
        &self,
        node: &Node,
        position: Position,
        slots: &BTreeMap<u32, usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let node_id = node.id;
        let screen = self.to_screen(position);
        let media = self.graph.node_media(node_id);
        let renaming = self
            .rename
            .as_ref()
            .filter(|rename| rename.node_id == node_id);
        let title: SharedString = match renaming {
            Some(rename) => format!("{}▏", rename.text).into(),
            None => self.settings.display_name(node).to_owned().into(),
        };
        let wiring = matches!(self.drag, Some(Drag::Wire { .. }));

        let header = div()
            .id(("header", node_id as usize))
            .h(self.scaled(HEADER_HEIGHT))
            .px(self.scaled(PADDING))
            .flex()
            .items_center()
            .gap(self.scaled(8.0))
            .bg(theme.header)
            .rounded_t_md()
            .border_b_1()
            .border_color(theme.border)
            .cursor_grab()
            // The media bar is lit while the node processes data, e.g. a playing stream.
            .child(
                div()
                    .w(self.scaled(3.0))
                    .h(self.scaled(14.0))
                    .rounded_sm()
                    .bg(theme.media(media))
                    .when(node.state != NodeState::Running, |bar| bar.opacity(0.3)),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_size(self.scaled(TEXT_TITLE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .when(renaming.is_some(), |title| title.text_color(theme.accent))
                    .child(title),
            )
            .child(
                div()
                    .id(("info", node_id as usize))
                    .px(self.scaled(4.0))
                    .rounded_sm()
                    .text_size(self.scaled(TEXT_SMALL))
                    .text_color(if self.inspected == Some(node_id) {
                        theme.accent
                    } else {
                        theme.muted_text
                    })
                    .cursor_pointer()
                    .hover_style(|style| style.bg(theme.button_hover))
                    .font_weight(gpui::FontWeight::BOLD)
                    .tooltip(tip(theme, "Show every PipeWire property of this node."))
                    .child("i")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.inspected = (this.inspected != Some(node_id)).then_some(node_id);
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .id(("hide", node_id as usize))
                    .px(self.scaled(4.0))
                    .rounded_sm()
                    .text_size(self.scaled(TEXT_SMALL))
                    .text_color(theme.muted_text)
                    .cursor_pointer()
                    .hover_style(|style| style.bg(theme.button_hover))
                    .tooltip(tip(
                        theme,
                        "Hide this node (and every node with the same name). \
                         \"Show hidden\" brings it back.",
                    ))
                    .child("hide")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.hide(node_id, cx);
                        }),
                    ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if event.click_count >= 2 {
                        this.drag = None;
                        this.start_rename(node_id, window, cx);
                    } else {
                        this.start_node_drag(node_id, event.position);
                    }
                    cx.notify();
                }),
            );

        let inputs = geometry::inputs(&self.graph, node_id);
        let outputs = geometry::outputs(&self.graph, node_id);
        let port_column = |ports: Vec<&Port>, cx: &mut Context<Self>| {
            div().flex().flex_col().flex_1().min_w_0().children(
                ports
                    .into_iter()
                    .map(|port| self.render_port(port, wiring, slots, theme, cx)),
            )
        };

        div()
            .id(("node", node_id as usize))
            .absolute()
            .left(screen.x)
            .top(screen.y)
            .w(self.scaled(NODE_WIDTH))
            .h(self.scaled(geometry::node_height(&self.graph, node)))
            .flex()
            .flex_col()
            .bg(theme.card)
            .rounded_md()
            .shadow_md()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.start_node_drag(node_id, event.position);
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    if let Some(Drag::Wire { from_port, .. }) = this.drag {
                        this.drag = None;
                        this.link_to_node(from_port, node_id);
                        cx.notify();
                    }
                }),
            )
            // GPUI paints an element's border over its children, which would cut
            // through the port dots, so the outline is a layer below them.
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border),
            )
            .child(header)
            .when_some(node.media_detail(), |card, detail| {
                card.child(
                    div()
                        .h(self.scaled(SUBTITLE_HEIGHT))
                        .px(self.scaled(PADDING))
                        .flex()
                        .items_center()
                        .text_size(self.scaled(TEXT_SMALL))
                        .text_color(theme.muted_text)
                        .child(
                            div()
                                .truncate()
                                .child(SharedString::from(detail.to_owned())),
                        ),
                )
            })
            .when_some(node.volume.clone(), |card, volume| {
                card.child(self.render_volume(node_id, &volume, theme, cx))
            })
            .child(
                div()
                    .flex()
                    .child(port_column(inputs, cx))
                    .child(port_column(outputs, cx)),
            )
            .into_any_element()
    }

    /// The wire closest to `position` (window coordinates), if one is within reach.
    fn wire_at(&self, position: Point<Pixels>) -> Option<PortPair> {
        let point = self.to_world(position);
        let reach = WIRE_REACH / self.zoom;
        self.graph
            .links
            .values()
            .filter_map(|link| {
                let (from, to) = self.link_ends(&link.ports)?;
                Some((geometry::wire_distance(from, to, point), link.ports))
            })
            .filter(|(distance, _)| *distance <= reach)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, pair)| pair)
    }

    fn port_description(&self, node_id: u32, port_id: u32) -> String {
        let node = self
            .graph
            .nodes
            .get(&node_id)
            .map_or("?", |node| self.settings.display_name(node));
        let port = self
            .graph
            .ports
            .get(&port_id)
            .map(port_label)
            .unwrap_or_default();
        format!("{node} {port}")
    }

    fn render_wire_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.wire_menu.as_ref()?;
        let pair = menu.pair;
        let title = format!(
            "{} → {}",
            self.port_description(pair.output_node, pair.output_port),
            self.port_description(pair.input_node, pair.input_port),
        );
        Some(
            div()
                .absolute()
                .left(menu.at.x)
                .top(menu.at.y - px(TOOLBAR_HEIGHT))
                .min_w(px(200.0))
                .max_w(px(420.0))
                .p_1()
                .flex()
                .flex_col()
                .bg(theme.card)
                .border_1()
                .border_color(theme.border)
                .rounded_md()
                .shadow_lg()
                .text_sm()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_text)
                        .child(title),
                )
                .child(
                    div()
                        .id("disconnect")
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_color(theme.danger)
                        .hover_style(|style| style.bg(theme.button_hover))
                        .child("Disconnect")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.wire_menu = None;
                                this.edit(Edit::Unlink(pair));
                                cx.notify();
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Every PipeWire property of the inspected node, next to its card.
    fn render_inspector(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let node_id = self.inspected?;
        let node = self.graph.nodes.get(&node_id)?;
        let position = *self.positions.get(&node_id)?;
        let screen = self.to_screen(Position {
            x: position.x + NODE_WIDTH,
            y: position.y,
        });
        let state = format!("{:?}", node.state).to_lowercase();
        let rows = node.properties.iter().map(|(key, value)| {
            div()
                .flex()
                .gap_2()
                .py(px(1.0))
                .child(
                    div()
                        .w(px(INSPECTOR_KEY_WIDTH))
                        .flex_shrink_0()
                        .truncate()
                        .text_color(theme.muted_text)
                        .child(SharedString::from(key.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(SharedString::from(value.clone())),
                )
        });
        Some(
            div()
                .id("inspector")
                .absolute()
                .left(screen.x + px(12.0))
                .top(screen.y)
                .w(px(INSPECTOR_WIDTH))
                .max_h(px(INSPECTOR_HEIGHT))
                .flex()
                .flex_col()
                .bg(theme.card)
                .border_1()
                .border_color(theme.border)
                .rounded_md()
                .shadow_lg()
                .text_xs()
                .cursor_default()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .bg(theme.header)
                        .rounded_t_md()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(SharedString::from(
                                    self.settings.display_name(node).to_owned(),
                                )),
                        )
                        .child(
                            div()
                                .text_color(theme.muted_text)
                                .child(format!("id {node_id} · {state}")),
                        )
                        .child(
                            div()
                                .id("inspector-close")
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .hover_style(|style| style.bg(theme.button_hover))
                                .child("×")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.inspected = None;
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
                .child(
                    div()
                        .id("inspector-properties")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_3()
                        .py_2()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    fn start_node_drag(&mut self, node_id: u32, pointer: Point<Pixels>) {
        let Some(position) = self.positions.get(&node_id) else {
            return;
        };
        let pointer = self.to_world(pointer);
        self.drag = Some(Drag::Node {
            node_id,
            grab: Position {
                x: pointer.x - position.x,
                y: pointer.y - position.y,
            },
            start: *position,
        });
    }

    fn render_volume(
        &self,
        node_id: u32,
        volume: &tuyau_core::Volume,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let level = volume.level();
        let (_, track_width) = geometry::slider_track(Position { x: 0.0, y: 0.0 });
        let fill = (level / geometry::SLIDER_MAX_LEVEL).clamp(0.0, 1.0) * track_width;
        let fill_color = if volume.mute {
            theme.muted_text
        } else {
            theme.accent
        };
        div()
            .h(self.scaled(VOLUME_HEIGHT))
            .px(self.scaled(PADDING))
            .flex()
            .items_center()
            .gap(self.scaled(SLIDER_GAP))
            .child(
                div()
                    .id(("mute", node_id as usize))
                    .w(self.scaled(MUTE_WIDTH))
                    .h(self.scaled(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .text_size(self.scaled(TEXT_SMALL))
                    .font_weight(gpui::FontWeight::BOLD)
                    .cursor_pointer()
                    .bg(if volume.mute {
                        theme.danger
                    } else {
                        theme.button
                    })
                    .text_color(if volume.mute {
                        theme.card
                    } else {
                        theme.muted_text
                    })
                    .hover_style(|style| style.opacity(0.85))
                    .tooltip(tip(theme, "Mute or unmute."))
                    .child("M")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.toggle_mute(node_id);
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .id(("slider", node_id as usize))
                    .w(self.scaled(track_width))
                    .h(self.scaled(20.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .w_full()
                            .h(self.scaled(6.0))
                            .rounded_full()
                            .bg(theme.track)
                            .child(
                                div()
                                    .w(self.scaled(fill))
                                    .h_full()
                                    .rounded_full()
                                    .bg(fill_color),
                            ),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            let pointer = this.to_world(event.position);
                            if let Some(position) = this.positions.get(&node_id).copied() {
                                this.set_level(
                                    node_id,
                                    geometry::slider_level(position, pointer.x),
                                );
                            }
                            this.drag = Some(Drag::Volume { node_id });
                            cx.notify();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                        // Ctrl-scroll zooms the graph, even over a slider.
                        if event.modifiers.control {
                            return;
                        }
                        cx.stop_propagation();
                        let delta = f32::from(event.delta.pixel_delta(px(20.0)).y);
                        let current = this
                            .graph
                            .nodes
                            .get(&node_id)
                            .and_then(|node| node.volume.as_ref())
                            .map(|volume| volume.level())
                            .unwrap_or(0.0);
                        let step = if delta > 0.0 {
                            VOLUME_STEP
                        } else {
                            -VOLUME_STEP
                        };
                        this.set_level(
                            node_id,
                            (current + step).clamp(0.0, geometry::SLIDER_MAX_LEVEL),
                        );
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w(self.scaled(PERCENT_WIDTH))
                    .text_size(self.scaled(TEXT_SMALL))
                    .text_right()
                    .text_color(theme.muted_text)
                    .child(format!("{:.0}%", level * 100.0)),
            )
    }

    fn render_port(
        &self,
        port: &Port,
        wiring: bool,
        slots: &BTreeMap<u32, usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let port_id = port.id;
        let input = port.direction == Direction::Input;
        let connected = self
            .graph
            .links
            .values()
            .any(|link| link.ports.output_port == port_id || link.ports.input_port == port_id);
        let label = port_label(port);
        let compatible = match &self.drag {
            Some(Drag::Wire { from_port, .. }) => {
                self.graph.ports.get(from_port).is_some_and(|from| {
                    from.direction != port.direction && from.media == port.media
                })
            }
            _ => false,
        };
        // A linked output wears its wires' color; inputs mix several sources.
        let color = match slots.get(&port.node_id) {
            Some(slot) if connected && !input => {
                theme.wire(port.media, *slot, port.channel.as_deref())
            }
            _ => gpui::rgb_to_hsla(theme.media(port.media)),
        };
        let dot = div()
            .absolute()
            .top(self.scaled(PORT_ROW_HEIGHT / 2.0 - 5.0))
            .when(input, |dot| dot.left(self.scaled(-5.0)))
            .when(!input, |dot| dot.right(self.scaled(-5.0)))
            .size(self.scaled(10.0))
            .rounded_full()
            .border_2()
            .border_color(color)
            .bg(if connected {
                color
            } else {
                gpui::rgb_to_hsla(theme.card)
            });

        div()
            .id(("port", port_id as usize))
            .relative()
            .h(self.scaled(PORT_ROW_HEIGHT))
            .px(self.scaled(PADDING))
            .flex()
            .items_center()
            .when(!input, |row| row.justify_end())
            .text_size(self.scaled(TEXT_SMALL))
            .text_color(if port.monitor {
                theme.muted_text
            } else {
                theme.text
            })
            .cursor_crosshair()
            .when(compatible, |row| row.bg(theme.port_hover))
            .when(!wiring, |row| {
                row.hover_style(|style| style.bg(theme.port_hover))
            })
            .child(div().truncate().child(label))
            .child(dot)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.drag = Some(Drag::Wire {
                        from_port: port_id,
                        pointer: this.to_world(event.position),
                    });
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    if let Some(Drag::Wire { from_port, .. }) = this.drag {
                        this.drag = None;
                        if from_port != port_id {
                            this.toggle_link(from_port, port_id);
                        }
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
    }
}

impl Render for Patchbay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let slots = theme::wire_slots(&self.graph);
        let nodes: Vec<AnyElement> = self
            .graph
            .nodes
            .values()
            .filter(|node| self.visible(node))
            .filter_map(|node| {
                let position = *self.positions.get(&node.id)?;
                Some(self.render_node(node, position, &slots, &theme, cx))
            })
            .collect();
        let handles = self.render_wire_handles(&theme, cx);
        let inspector = self.render_inspector(&theme, cx);
        let wire_menu = self.render_wire_menu(&theme, cx);
        let empty = self.graph.nodes.is_empty();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.text)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            // Keep the shortcuts working after clicks anywhere in the window.
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.focus.focus(window, cx);
                // Any click closes the wire menu; a click on its item was bound
                // with the wire already.
                if this.wire_menu.take().is_some() {
                    cx.notify();
                }
            }))
            .child(self.render_toolbar(&theme, cx))
            .child(
                div()
                    .id("graph")
                    .relative()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .when(matches!(self.drag, Some(Drag::Pan { .. })), |graph| {
                        graph.cursor_grabbing()
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            if this.rename.is_some() {
                                this.commit_rename();
                            }
                            this.drag = Some(Drag::Pan {
                                last: event.position,
                            });
                            cx.notify();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.wire_menu = this.wire_at(event.position).map(|pair| WireMenu {
                                pair,
                                at: event.position,
                            });
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.end_drag();
                            cx.notify();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        let delta = event.delta.pixel_delta(px(40.0));
                        if event.modifiers.control {
                            let factor = (f32::from(delta.y) * ZOOM_SPEED).exp();
                            this.zoom_at(this.zoom * factor, event.position);
                        } else {
                            this.pan += delta;
                        }
                        cx.notify();
                    }))
                    .child(self.render_wires(&theme))
                    .children(handles)
                    .children(nodes)
                    .children(inspector)
                    .children(wire_menu)
                    .when(empty, |graph| {
                        graph.child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(theme.muted_text)
                                .child("Waiting for PipeWire…"),
                        )
                    }),
            )
    }
}

fn load_settings(options: &Options) -> Result<Settings, tuyau_core::ConfigError> {
    let ui_path = Ui::path_for(&options.state_path);
    let ui = Ui::load(&ui_path, &options.state_path)?;
    // First run after the split: keep what was migrated from the state file.
    if !ui_path.exists() && ui != Ui::default() {
        ui.save(&ui_path)?;
    }
    Ok(Settings {
        config: Config::load(&options.config_path)?,
        state: State::load(&options.state_path)?,
        ui,
    })
}

fn port_label(port: &Port) -> String {
    let name = port.channel.clone().unwrap_or_else(|| port.name.clone());
    if port.monitor {
        return format!("monitor {name}");
    }
    name
}

/// The text shown when hovering a control.
struct Tip {
    text: SharedString,
    background: gpui::Rgba,
    border: gpui::Rgba,
    color: gpui::Rgba,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(340.0))
            .px_2()
            .py_1()
            .bg(self.background)
            .border_1()
            .border_color(self.border)
            .rounded_md()
            .shadow_md()
            .text_xs()
            .text_color(self.color)
            .child(self.text.clone())
    }
}

fn tip(theme: &Theme, text: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let (background, border, color) = (theme.toolbar, theme.border, theme.text);
    move |_, cx| {
        cx.new(|_| Tip {
            text: text.into(),
            background,
            border,
            color,
        })
        .into()
    }
}

fn button(
    theme: &Theme,
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .text_sm()
        .border_1()
        .border_color(if active { theme.accent } else { theme.border })
        .bg(theme.button)
        .text_color(if active { theme.text } else { theme.muted_text })
        .cursor_pointer()
        .hover_style(|style| style.bg(theme.button_hover))
        .child(label.into())
}
