//! Automatic node placement with the Sugiyama method: nodes go into layers
//! following the signal flow, layers are reordered to remove wire crossings, then
//! nodes slide vertically so wires run as straight as the space allows.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    config::{Position, Settings},
    graph::{Direction, Graph, Node, Port},
};

pub const NODE_WIDTH: f32 = 240.0;
pub const LAYER_GAP: f32 = 140.0;
pub const NODE_GAP: f32 = 24.0;
const COMPONENT_GAP: f32 = 72.0;
/// Room kept for a wire passing through a layer it does not stop in.
const LANE_HEIGHT: f32 = 10.0;
const LANE_GAP: f32 = 6.0;
const ORDER_PASSES: usize = 24;
const PLACEMENT_PASSES: usize = 16;
/// Unlinked nodes are packed in rows at most this many cards wide.
const LOOSE_ROW_CARDS: usize = 4;

/// Card sizes, which only the GUI knows.
pub trait Metrics {
    fn height(&self, node: &Node) -> f32;
    /// Distance from the top of the card to where wires attach to `port`.
    fn port_offset(&self, node: &Node, port: &Port) -> f32;
}

/// Positions for every visible node with ports. Nodes with a `fixed` position keep
/// it and the others are placed next to what they are linked to. With nothing
/// fixed, the whole graph is laid out from scratch.
pub fn arrange(
    graph: &Graph,
    settings: &Settings,
    metrics: &impl Metrics,
    fixed: impl Fn(&Node) -> Option<Position>,
) -> BTreeMap<u32, Position> {
    let nodes: BTreeMap<u32, &Node> = graph
        .nodes
        .values()
        .filter(|node| {
            !settings.is_hidden(node) && graph.ports.values().any(|port| port.node_id == node.id)
        })
        .map(|node| (node.id, node))
        .collect();
    let heights: BTreeMap<u32, f32> = nodes
        .iter()
        .map(|(id, node)| (*id, metrics.height(node)))
        .collect();
    let edges = edges(graph, &nodes, metrics);
    let layout = layout_all(graph, &nodes, &heights, &edges);

    let pinned: BTreeMap<u32, Position> = nodes
        .values()
        .filter_map(|node| Some((node.id, fixed(node)?)))
        .collect();
    if pinned.is_empty() {
        return layout;
    }
    place_around(pinned, &layout, &heights, &edges)
}

/// A link between two visible nodes and where it attaches on each card.
#[derive(Debug, Clone, Copy)]
struct Edge {
    from: u32,
    to: u32,
    from_offset: f32,
    to_offset: f32,
}

fn edges(graph: &Graph, nodes: &BTreeMap<u32, &Node>, metrics: &impl Metrics) -> Vec<Edge> {
    graph
        .links
        .values()
        .filter(|link| link.ports.output_node != link.ports.input_node)
        .filter_map(|link| {
            let from = nodes.get(&link.ports.output_node)?;
            let to = nodes.get(&link.ports.input_node)?;
            let output = graph.ports.get(&link.ports.output_port)?;
            let input = graph.ports.get(&link.ports.input_port)?;
            Some(Edge {
                from: from.id,
                to: to.id,
                from_offset: metrics.port_offset(from, output),
                to_offset: metrics.port_offset(to, input),
            })
        })
        .collect()
}

/// Lays out each group of linked nodes on its own, stacks the groups, and packs
/// unlinked nodes in rows underneath.
fn layout_all(
    graph: &Graph,
    nodes: &BTreeMap<u32, &Node>,
    heights: &BTreeMap<u32, f32>,
    edges: &[Edge],
) -> BTreeMap<u32, Position> {
    let mut components = components(nodes.keys().copied(), edges);
    components.sort_by_key(|component| (std::cmp::Reverse(component.len()), component[0]));
    let (linked, loose): (Vec<_>, Vec<_>) = components
        .into_iter()
        .partition(|component| component.len() > 1);

    let mut positions = BTreeMap::new();
    let mut top = 0.0;
    for component in linked {
        let members: BTreeSet<u32> = component.iter().copied().collect();
        let component_edges: Vec<Edge> = edges
            .iter()
            .filter(|edge| members.contains(&edge.from))
            .copied()
            .collect();
        let placed = layered(&component, heights, &component_edges);
        let bottom = placed
            .iter()
            .map(|(id, position)| position.y + heights[id])
            .fold(0.0, f32::max);
        for (id, position) in placed {
            positions.insert(
                id,
                Position {
                    x: position.x,
                    y: position.y + top,
                },
            );
        }
        top += bottom + COMPONENT_GAP;
    }

    let mut loose: Vec<u32> = loose.into_iter().flatten().collect();
    loose.sort_by_key(|id| {
        let node = nodes[id];
        let inputs = graph.has_ports(node.id, Direction::Input);
        let outputs = graph.has_ports(node.id, Direction::Output);
        (
            graph.node_media(node.id),
            inputs && !outputs,
            inputs,
            node.label().to_lowercase(),
            node.id,
        )
    });
    for row in loose.chunks(LOOSE_ROW_CARDS) {
        for (column, id) in row.iter().enumerate() {
            positions.insert(
                *id,
                Position {
                    x: column as f32 * (NODE_WIDTH + LAYER_GAP),
                    y: top,
                },
            );
        }
        top += row.iter().map(|id| heights[id]).fold(0.0, f32::max) + NODE_GAP;
    }
    positions
}

fn components(nodes: impl Iterator<Item = u32>, edges: &[Edge]) -> Vec<Vec<u32>> {
    let mut parent: BTreeMap<u32, u32> = nodes.map(|id| (id, id)).collect();
    fn root(parent: &mut BTreeMap<u32, u32>, id: u32) -> u32 {
        let up = parent[&id];
        if up == id {
            return id;
        }
        let top = root(parent, up);
        parent.insert(id, top);
        top
    }
    for edge in edges {
        let (a, b) = (root(&mut parent, edge.from), root(&mut parent, edge.to));
        parent.insert(a.max(b), a.min(b));
    }
    let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let ids: Vec<u32> = parent.keys().copied().collect();
    for id in ids {
        let top = root(&mut parent, id);
        groups.entry(top).or_default().push(id);
    }
    groups.into_values().collect()
}

/// One card, or a waypoint of a wire crossing a layer.
#[derive(Debug, Clone, Copy)]
struct Vertex {
    node: Option<u32>,
    height: f32,
}

/// A wire between two adjacent layers: vertex indices and attach offsets.
#[derive(Debug, Clone, Copy)]
struct Segment {
    upper: usize,
    upper_offset: f32,
    lower: usize,
    lower_offset: f32,
}

struct Layered {
    vertices: Vec<Vertex>,
    /// Vertex indices per layer, in their vertical order.
    layers: Vec<Vec<usize>>,
    /// `segments[l]` joins layer `l` to layer `l + 1`.
    segments: Vec<Vec<Segment>>,
}

fn layered(nodes: &[u32], heights: &BTreeMap<u32, f32>, edges: &[Edge]) -> BTreeMap<u32, Position> {
    let edges = break_cycles(nodes, edges);
    let ranks = assign_layers(nodes, &edges);
    let mut graph = build(nodes, heights, &edges, &ranks);
    order(&mut graph);
    let ys = place(&graph);
    let top = ys.iter().copied().fold(f32::MAX, f32::min);
    graph
        .layers
        .iter()
        .enumerate()
        .flat_map(|(layer, vertices)| vertices.iter().map(move |vertex| (layer, *vertex)))
        .filter_map(|(layer, vertex)| {
            let node = graph.vertices[vertex].node?;
            Some((
                node,
                Position {
                    x: layer as f32 * (NODE_WIDTH + LAYER_GAP),
                    y: ys[vertex] - top,
                },
            ))
        })
        .collect()
}

/// Turns back edges of a depth-first search around so the graph flows one way.
/// Feedback loops are rare in audio graphs; when present, the search starts from
/// nodes nothing feeds so the loop's way back is the edge that gets flipped.
fn break_cycles(nodes: &[u32], edges: &[Edge]) -> Vec<Edge> {
    let mut outgoing: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    let mut fed = BTreeSet::new();
    for (index, edge) in edges.iter().enumerate() {
        outgoing.entry(edge.from).or_default().push(index);
        fed.insert(edge.to);
    }
    let starts = nodes
        .iter()
        .filter(|id| !fed.contains(id))
        .chain(nodes.iter().filter(|id| fed.contains(id)));

    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Active,
        Done,
    }
    let mut marks: BTreeMap<u32, Mark> = BTreeMap::new();
    let mut reversed = BTreeSet::new();
    for start in starts {
        if marks.contains_key(start) {
            continue;
        }
        marks.insert(*start, Mark::Active);
        let mut stack = vec![(*start, 0)];
        while let Some((node, next)) = stack.last_mut() {
            let out = outgoing.get(node).map(Vec::as_slice).unwrap_or_default();
            let Some(&edge) = out.get(*next) else {
                marks.insert(*node, Mark::Done);
                stack.pop();
                continue;
            };
            *next += 1;
            let target = edges[edge].to;
            match marks.get(&target) {
                Some(Mark::Active) => {
                    reversed.insert(edge);
                }
                Some(Mark::Done) => {}
                None => {
                    marks.insert(target, Mark::Active);
                    stack.push((target, 0));
                }
            }
        }
    }
    edges
        .iter()
        .enumerate()
        .map(|(index, edge)| {
            if reversed.contains(&index) {
                Edge {
                    from: edge.to,
                    to: edge.from,
                    from_offset: edge.to_offset,
                    to_offset: edge.from_offset,
                }
            } else {
                *edge
            }
        })
        .collect()
}

/// Longest path from the sources, then sources move right next to what they feed
/// so they do not trail long wires across the graph.
fn assign_layers(nodes: &[u32], edges: &[Edge]) -> BTreeMap<u32, usize> {
    let mut incoming: BTreeMap<u32, usize> = nodes.iter().map(|id| (*id, 0)).collect();
    for edge in edges {
        *incoming.get_mut(&edge.to).expect("edge within component") += 1;
    }
    let mut ready: Vec<u32> = nodes
        .iter()
        .filter(|id| incoming[id] == 0)
        .copied()
        .collect();
    let mut ranks: BTreeMap<u32, usize> = ready.iter().map(|id| (*id, 0)).collect();
    while let Some(node) = ready.pop() {
        let rank = ranks[&node];
        for edge in edges.iter().filter(|edge| edge.from == node) {
            let target = ranks.entry(edge.to).or_default();
            *target = (*target).max(rank + 1);
            let waiting = incoming.get_mut(&edge.to).expect("edge within component");
            *waiting -= 1;
            if *waiting == 0 {
                ready.push(edge.to);
            }
        }
    }
    for node in nodes {
        let fed = edges.iter().any(|edge| edge.to == *node);
        let closest = edges
            .iter()
            .filter(|edge| edge.from == *node)
            .map(|edge| ranks[&edge.to])
            .min();
        if let (false, Some(closest)) = (fed, closest) {
            ranks.insert(*node, closest - 1);
        }
    }
    ranks
}

fn build(
    nodes: &[u32],
    heights: &BTreeMap<u32, f32>,
    edges: &[Edge],
    ranks: &BTreeMap<u32, usize>,
) -> Layered {
    let depth = ranks.values().max().map_or(1, |rank| rank + 1);
    let mut graph = Layered {
        vertices: Vec::new(),
        layers: vec![Vec::new(); depth],
        segments: vec![Vec::new(); depth.saturating_sub(1)],
    };
    let mut index_of = BTreeMap::new();
    for node in nodes {
        index_of.insert(*node, graph.vertices.len());
        graph.layers[ranks[node]].push(graph.vertices.len());
        graph.vertices.push(Vertex {
            node: Some(*node),
            height: heights[node],
        });
    }
    for edge in edges {
        let (start, end) = (ranks[&edge.from], ranks[&edge.to]);
        let mut upper = index_of[&edge.from];
        let mut upper_offset = edge.from_offset;
        for layer in start + 1..end {
            let lane = graph.vertices.len();
            graph.vertices.push(Vertex {
                node: None,
                height: LANE_HEIGHT,
            });
            graph.layers[layer].push(lane);
            graph.segments[layer - 1].push(Segment {
                upper,
                upper_offset,
                lower: lane,
                lower_offset: LANE_HEIGHT / 2.0,
            });
            upper = lane;
            upper_offset = LANE_HEIGHT / 2.0;
        }
        graph.segments[end - 1].push(Segment {
            upper,
            upper_offset,
            lower: index_of[&edge.to],
            lower_offset: edge.to_offset,
        });
    }
    graph
}

/// Barycenter sweeps followed by swapping neighbours, keeping the order with the
/// fewest crossings.
fn order(graph: &mut Layered) {
    let mut best = graph.layers.clone();
    let mut fewest = crossings(graph);
    for pass in 0..ORDER_PASSES {
        if fewest == 0 {
            break;
        }
        if pass % 2 == 0 {
            for layer in 1..graph.layers.len() {
                sort_by_neighbours(graph, layer, true);
            }
        } else {
            for layer in (0..graph.layers.len().saturating_sub(1)).rev() {
                sort_by_neighbours(graph, layer, false);
            }
        }
        transpose(graph);
        let count = crossings(graph);
        if count < fewest {
            fewest = count;
            best = graph.layers.clone();
        }
    }
    graph.layers = best;
}

/// Where each vertex sits within its layer, as an index.
fn slots(graph: &Layered) -> Vec<f32> {
    let mut slots = vec![0.0; graph.vertices.len()];
    for layer in &graph.layers {
        for (slot, vertex) in layer.iter().enumerate() {
            slots[*vertex] = slot as f32;
        }
    }
    slots
}

/// A port's place in its layer: the card's slot plus how far down the card it is.
fn port_key(graph: &Layered, slots: &[f32], vertex: usize, offset: f32) -> f32 {
    slots[vertex] + offset / graph.vertices[vertex].height.max(1.0)
}

fn sort_by_neighbours(graph: &mut Layered, layer: usize, from_above: bool) {
    let slots = slots(graph);
    let mut sums: BTreeMap<usize, (f32, f32)> = BTreeMap::new();
    let segments = if from_above {
        &graph.segments[layer - 1]
    } else {
        &graph.segments[layer]
    };
    for segment in segments {
        let (own, other, other_offset) = if from_above {
            (segment.lower, segment.upper, segment.upper_offset)
        } else {
            (segment.upper, segment.lower, segment.lower_offset)
        };
        let entry = sums.entry(own).or_default();
        entry.0 += port_key(graph, &slots, other, other_offset);
        entry.1 += 1.0;
    }
    let mut keyed: Vec<(f32, usize)> = graph.layers[layer]
        .iter()
        .map(|vertex| {
            let key = sums
                .get(vertex)
                .map_or(slots[*vertex], |(sum, count)| sum / count);
            (key, *vertex)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
    graph.layers[layer] = keyed.into_iter().map(|(_, vertex)| vertex).collect();
}

fn crossings_between(graph: &Layered, slots: &[f32], layer: usize) -> usize {
    let ends: Vec<(f32, f32)> = graph.segments[layer]
        .iter()
        .map(|segment| {
            (
                port_key(graph, slots, segment.upper, segment.upper_offset),
                port_key(graph, slots, segment.lower, segment.lower_offset),
            )
        })
        .collect();
    let mut count = 0;
    for (index, a) in ends.iter().enumerate() {
        for b in &ends[index + 1..] {
            if (a.0 - b.0) * (a.1 - b.1) < 0.0 {
                count += 1;
            }
        }
    }
    count
}

fn crossings(graph: &Layered) -> usize {
    let slots = slots(graph);
    (0..graph.segments.len())
        .map(|layer| crossings_between(graph, &slots, layer))
        .sum()
}

fn crossings_around(graph: &Layered, layer: usize) -> usize {
    let slots = slots(graph);
    let above = layer
        .checked_sub(1)
        .map_or(0, |above| crossings_between(graph, &slots, above));
    let below = if layer < graph.segments.len() {
        crossings_between(graph, &slots, layer)
    } else {
        0
    };
    above + below
}

fn transpose(graph: &mut Layered) {
    let mut improved = true;
    let mut rounds = 0;
    while improved && rounds < 8 {
        improved = false;
        rounds += 1;
        for layer in 0..graph.layers.len() {
            for slot in 1..graph.layers[layer].len() {
                let before = crossings_around(graph, layer);
                graph.layers[layer].swap(slot - 1, slot);
                if crossings_around(graph, layer) < before {
                    improved = true;
                } else {
                    graph.layers[layer].swap(slot - 1, slot);
                }
            }
        }
    }
}

/// Vertical positions: each vertex is pulled to line its ports up with the ports
/// it is wired to, while keeping the layer's order and gaps. Each layer is an
/// isotonic regression, solved exactly by pooling adjacent violators.
fn place(graph: &Layered) -> Vec<f32> {
    let mut ys = vec![0.0; graph.vertices.len()];
    for layer in &graph.layers {
        let mut y = 0.0;
        for (index, vertex) in layer.iter().enumerate() {
            if index > 0 {
                y += gap(graph, layer[index - 1], *vertex);
            }
            ys[*vertex] = y;
            y += graph.vertices[*vertex].height;
        }
    }
    let count = graph.layers.len();
    for pass in 0..PLACEMENT_PASSES {
        let layers: Vec<usize> = match pass % 3 {
            0 => (1..count).collect(),
            1 => (0..count.saturating_sub(1)).rev().collect(),
            _ => (0..count).collect(),
        };
        for layer in layers {
            let above = pass % 3 != 1;
            let below = pass % 3 != 0;
            align_layer(graph, &mut ys, layer, above, below);
        }
    }
    ys
}

fn gap(graph: &Layered, a: usize, b: usize) -> f32 {
    match (graph.vertices[a].node, graph.vertices[b].node) {
        (None, None) => LANE_GAP,
        _ => NODE_GAP,
    }
}

fn align_layer(graph: &Layered, ys: &mut [f32], layer: usize, above: bool, below: bool) {
    let mut pulls: BTreeMap<usize, (f32, f32)> = BTreeMap::new();
    let mut pull = |own: usize, target: f32, weight: f32| {
        let entry = pulls.entry(own).or_default();
        entry.0 += target * weight;
        entry.1 += weight;
    };
    let lane_weight = |segment: &Segment| {
        if graph.vertices[segment.upper].node.is_none()
            || graph.vertices[segment.lower].node.is_none()
        {
            4.0
        } else {
            1.0
        }
    };
    if above && layer > 0 {
        for segment in &graph.segments[layer - 1] {
            let target = ys[segment.upper] + segment.upper_offset - segment.lower_offset;
            pull(segment.lower, target, lane_weight(segment));
        }
    }
    if below && layer < graph.segments.len() {
        for segment in &graph.segments[layer] {
            let target = ys[segment.lower] + segment.lower_offset - segment.upper_offset;
            pull(segment.upper, target, lane_weight(segment));
        }
    }

    let vertices = &graph.layers[layer];
    let mut stacked = 0.0;
    let mut targets = Vec::with_capacity(vertices.len());
    let mut weights = Vec::with_capacity(vertices.len());
    let mut offsets = Vec::with_capacity(vertices.len());
    for (index, vertex) in vertices.iter().enumerate() {
        if index > 0 {
            stacked += graph.vertices[vertices[index - 1]].height
                + gap(graph, vertices[index - 1], *vertex);
        }
        let (target, weight) = match pulls.get(vertex) {
            Some((sum, weight)) => (sum / weight, *weight),
            None => (ys[*vertex], 0.01),
        };
        offsets.push(stacked);
        targets.push(target - stacked);
        weights.push(weight);
    }
    for ((vertex, value), offset) in vertices
        .iter()
        .zip(isotonic(&targets, &weights))
        .zip(offsets)
    {
        ys[*vertex] = value + offset;
    }
}

/// The non-decreasing sequence closest to `targets` in weighted least squares.
fn isotonic(targets: &[f32], weights: &[f32]) -> Vec<f32> {
    // (weighted sum, total weight, length)
    let mut blocks: Vec<(f32, f32, usize)> = Vec::new();
    for (target, weight) in targets.iter().zip(weights) {
        blocks.push((target * weight, *weight, 1));
        while blocks.len() > 1 {
            let last = blocks[blocks.len() - 1];
            let previous = blocks[blocks.len() - 2];
            if previous.0 / previous.1 <= last.0 / last.1 {
                break;
            }
            blocks.pop();
            *blocks.last_mut().expect("two blocks") = (
                previous.0 + last.0,
                previous.1 + last.1,
                previous.2 + last.2,
            );
        }
    }
    blocks
        .into_iter()
        .flat_map(|(sum, weight, length)| std::iter::repeat_n(sum / weight, length))
        .collect()
}

/// Keeps `pinned` nodes where they are and puts each other node beside what it is
/// wired to, or below everything when it is not wired to anything placed, moving
/// it down until it overlaps nothing.
fn place_around(
    mut placed: BTreeMap<u32, Position>,
    layout: &BTreeMap<u32, Position>,
    heights: &BTreeMap<u32, f32>,
    edges: &[Edge],
) -> BTreeMap<u32, Position> {
    let mut pending: Vec<(u32, Position)> = layout
        .iter()
        .filter(|(id, _)| !placed.contains_key(id))
        .map(|(id, position)| (*id, *position))
        .collect();
    pending.sort_by(|a, b| a.1.x.total_cmp(&b.1.x).then(a.1.y.total_cmp(&b.1.y)));

    for (node, _) in pending {
        let feeding: Vec<(Position, f32)> = edges
            .iter()
            .filter(|edge| edge.to == node)
            .filter_map(|edge| Some((*placed.get(&edge.from)?, edge.from_offset - edge.to_offset)))
            .collect();
        let fed: Vec<(Position, f32)> = edges
            .iter()
            .filter(|edge| edge.from == node)
            .filter_map(|edge| Some((*placed.get(&edge.to)?, edge.to_offset - edge.from_offset)))
            .collect();
        let mean_y = |links: &[(Position, f32)]| {
            links
                .iter()
                .map(|(position, shift)| position.y + shift)
                .sum::<f32>()
                / links.len() as f32
        };
        let start = if !feeding.is_empty() {
            Position {
                x: feeding
                    .iter()
                    .map(|(position, _)| position.x)
                    .fold(f32::MIN, f32::max)
                    + NODE_WIDTH
                    + LAYER_GAP,
                y: mean_y(&feeding),
            }
        } else if !fed.is_empty() {
            Position {
                x: fed
                    .iter()
                    .map(|(position, _)| position.x)
                    .fold(f32::MAX, f32::min)
                    - NODE_WIDTH
                    - LAYER_GAP,
                y: mean_y(&fed),
            }
        } else {
            Position {
                x: placed
                    .values()
                    .map(|position| position.x)
                    .fold(f32::MAX, f32::min)
                    .min(0.0),
                y: placed
                    .iter()
                    .map(|(id, position)| position.y + heights[id] + NODE_GAP)
                    .fold(0.0, f32::max),
            }
        };
        let position = free_spot(start, heights[&node], &placed, heights);
        placed.insert(node, position);
    }
    placed
}

fn free_spot(
    mut position: Position,
    height: f32,
    placed: &BTreeMap<u32, Position>,
    heights: &BTreeMap<u32, f32>,
) -> Position {
    while let Some(bottom) = placed
        .iter()
        .filter(|(id, other)| {
            position.x < other.x + NODE_WIDTH + NODE_GAP
                && other.x < position.x + NODE_WIDTH + NODE_GAP
                && position.y < other.y + heights[id] + NODE_GAP
                && other.y < position.y + height + NODE_GAP
        })
        .map(|(id, other)| other.y + heights[id] + NODE_GAP)
        .reduce(f32::max)
    {
        position.y = bottom;
    }
    position
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::fixtures::Builder;

    const HEADER: f32 = 30.0;
    const ROW: f32 = 20.0;

    struct Cards<'a>(&'a Graph);

    impl Metrics for Cards<'_> {
        fn height(&self, node: &Node) -> f32 {
            let rows = self
                .0
                .ports_of(node.id, Direction::Input)
                .count()
                .max(self.0.ports_of(node.id, Direction::Output).count());
            HEADER + rows as f32 * ROW
        }

        fn port_offset(&self, node: &Node, port: &Port) -> f32 {
            let row = self
                .0
                .ports_of(node.id, port.direction)
                .position(|sibling| sibling.id == port.id)
                .unwrap_or(0);
            HEADER + (row as f32 + 0.5) * ROW
        }
    }

    fn layout(builder: &Builder, settings: &Settings) -> BTreeMap<u32, Position> {
        arrange(&builder.graph, settings, &Cards(&builder.graph), |_| None)
    }

    fn connect(builder: &mut Builder, from: u32, to: u32, channel: &str) {
        let output = builder.port(from, &format!("output_{channel}"));
        let input = builder.port(to, &format!("playback_{channel}"));
        builder.link(output, input);
    }

    fn assert_no_overlap(builder: &Builder, positions: &BTreeMap<u32, Position>) {
        let cards = Cards(&builder.graph);
        let rects: Vec<(u32, Position, f32)> = positions
            .iter()
            .map(|(id, position)| (*id, *position, cards.height(&builder.graph.nodes[id])))
            .collect();
        for (index, (a, pa, ha)) in rects.iter().enumerate() {
            for (b, pb, hb) in &rects[index + 1..] {
                let apart = pa.x + NODE_WIDTH <= pb.x
                    || pb.x + NODE_WIDTH <= pa.x
                    || pa.y + ha <= pb.y
                    || pb.y + hb <= pa.y;
                assert!(apart, "{a} at {pa:?} overlaps {b} at {pb:?}");
            }
        }
    }

    #[test]
    fn follows_the_signal_left_to_right() {
        let mut builder = Builder::new();
        let app = builder.node("spotify", "Stream/Output/Audio", &[], &["FL", "FR"]);
        let filter = builder.node("eq", "Audio/Sink", &["FL", "FR"], &["FL", "FR"]);
        let sink = builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        for channel in ["FL", "FR"] {
            connect(&mut builder, app, filter, channel);
            connect(&mut builder, filter, sink, channel);
        }
        let positions = layout(&builder, &Settings::default());
        assert!(positions[&app].x < positions[&filter].x);
        assert!(positions[&filter].x < positions[&sink].x);
    }

    #[test]
    fn chains_line_up_their_ports() {
        let mut builder = Builder::new();
        let app = builder.node("app", "Stream/Output/Audio", &[], &["FL", "FR"]);
        let sink = builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        connect(&mut builder, app, sink, "FL");
        connect(&mut builder, app, sink, "FR");
        let positions = layout(&builder, &Settings::default());
        assert_eq!(positions[&app].y, positions[&sink].y);
    }

    #[test]
    fn untangles_crossed_wires() {
        let mut builder = Builder::new();
        let first = builder.node("a", "Stream/Output/Audio", &[], &["MONO"]);
        let second = builder.node("b", "Stream/Output/Audio", &[], &["MONO"]);
        let late = builder.node("x", "Audio/Sink", &["MONO"], &[]);
        let early = builder.node("y", "Audio/Sink", &["MONO"], &[]);
        connect(&mut builder, first, early, "MONO");
        connect(&mut builder, second, late, "MONO");
        connect(&mut builder, first, late, "MONO");
        let positions = layout(&builder, &Settings::default());
        let sources_down = positions[&first].y < positions[&second].y;
        let sinks_down = positions[&early].y < positions[&late].y;
        assert_eq!(sources_down, sinks_down);
        assert_no_overlap(&builder, &positions);
    }

    #[test]
    fn busy_graphs_do_not_overlap() {
        let mut builder = Builder::new();
        let apps: Vec<u32> = (0..6)
            .map(|index| {
                builder.node(
                    &format!("app{index}"),
                    "Stream/Output/Audio",
                    &[],
                    &["FL", "FR"],
                )
            })
            .collect();
        let filters: Vec<u32> = (0..3)
            .map(|index| {
                builder.node(
                    &format!("fx{index}"),
                    "Audio/Sink",
                    &["FL", "FR"],
                    &["FL", "FR"],
                )
            })
            .collect();
        let sinks: Vec<u32> = (0..2)
            .map(|index| builder.node(&format!("out{index}"), "Audio/Sink", &["FL", "FR"], &[]))
            .collect();
        builder.node("unused", "Audio/Sink", &["FL", "FR"], &[]);
        for (index, app) in apps.iter().enumerate() {
            for channel in ["FL", "FR"] {
                connect(&mut builder, *app, filters[index % 3], channel);
                connect(&mut builder, *app, sinks[index % 2], channel);
            }
        }
        for (index, filter) in filters.iter().enumerate() {
            for channel in ["FL", "FR"] {
                connect(&mut builder, *filter, sinks[index % 2], channel);
            }
        }
        connect(&mut builder, filters[0], filters[1], "FL");
        assert_no_overlap(&builder, &layout(&builder, &Settings::default()));
    }

    #[test]
    fn feedback_loops_still_lay_out() {
        let mut builder = Builder::new();
        let a = builder.node("a", "Audio/Sink", &["MONO"], &["MONO"]);
        let b = builder.node("b", "Audio/Sink", &["MONO"], &["MONO"]);
        connect(&mut builder, a, b, "MONO");
        connect(&mut builder, b, a, "MONO");
        let positions = layout(&builder, &Settings::default());
        assert_ne!(positions[&a].x, positions[&b].x);
    }

    #[test]
    fn new_nodes_go_next_to_their_source_without_overlap() {
        let mut builder = Builder::new();
        let app = builder.node("app", "Stream/Output/Audio", &[], &["MONO"]);
        let blocker = builder.node("blocker", "Audio/Sink", &["MONO"], &[]);
        let sink = builder.node("speakers", "Audio/Sink", &["MONO"], &[]);
        connect(&mut builder, app, sink, "MONO");
        let fixed = BTreeMap::from([
            (app, Position { x: 0.0, y: 0.0 }),
            (
                blocker,
                Position {
                    x: NODE_WIDTH + LAYER_GAP,
                    y: 0.0,
                },
            ),
        ]);
        let positions = arrange(
            &builder.graph,
            &Settings::default(),
            &Cards(&builder.graph),
            |node| fixed.get(&node.id).copied(),
        );
        assert_eq!(positions[&app], fixed[&app]);
        assert_eq!(positions[&sink].x, NODE_WIDTH + LAYER_GAP);
        assert_no_overlap(&builder, &positions);
    }

    #[test]
    fn hidden_and_portless_nodes_are_skipped() {
        let mut builder = Builder::new();
        let hidden = builder.node("noise", "Audio/Sink", &["FL"], &[]);
        let driver = builder.node("Dummy-Driver", "", &[], &[]);
        let mut settings = Settings::default();
        settings.state.hidden.insert("noise".into());
        let positions = layout(&builder, &settings);
        assert!(!positions.contains_key(&hidden));
        assert!(!positions.contains_key(&driver));
    }

    #[test]
    fn isotonic_pools_violators() {
        assert_eq!(
            isotonic(&[1.0, 3.0, 2.0], &[1.0, 1.0, 1.0]),
            [1.0, 2.5, 2.5]
        );
    }
}
