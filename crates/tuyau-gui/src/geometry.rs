//! Card metrics. Everything is laid out from these constants, so port anchors
//! and slider hit areas can be computed without measuring rendered elements.

use tuyau_core::{
    Direction, Graph, Node, Port, Position,
    layout::{Metrics, NODE_WIDTH},
};

pub const HEADER_HEIGHT: f32 = 32.0;
pub const VOLUME_HEIGHT: f32 = 30.0;
/// The stream's `media.name`, when it says something.
pub const SUBTITLE_HEIGHT: f32 = 20.0;
pub const PORT_ROW_HEIGHT: f32 = 22.0;
pub const BOTTOM_PADDING: f32 = 8.0;
pub const PADDING: f32 = 10.0;
pub const MUTE_WIDTH: f32 = 26.0;
pub const PERCENT_WIDTH: f32 = 42.0;
pub const SLIDER_GAP: f32 = 8.0;
/// The slider spans 0% to this level; louder values pin it to the end.
pub const SLIDER_MAX_LEVEL: f32 = 1.0;

pub fn inputs(graph: &Graph, node_id: u32) -> Vec<&Port> {
    graph.ports_of(node_id, Direction::Input).collect()
}

pub fn outputs(graph: &Graph, node_id: u32) -> Vec<&Port> {
    graph.ports_of(node_id, Direction::Output).collect()
}

fn ports_top(node: &Node) -> f32 {
    let subtitle = if node.media_detail().is_some() {
        SUBTITLE_HEIGHT
    } else {
        0.0
    };
    let volume = if node.volume.is_some() {
        VOLUME_HEIGHT
    } else {
        0.0
    };
    HEADER_HEIGHT + subtitle + volume
}

pub fn node_height(graph: &Graph, node: &Node) -> f32 {
    let rows = inputs(graph, node.id)
        .len()
        .max(outputs(graph, node.id).len());
    ports_top(node) + rows as f32 * PORT_ROW_HEIGHT + BOTTOM_PADDING
}

/// Card sizes for the auto layout.
pub struct Cards<'a>(pub &'a Graph);

impl Metrics for Cards<'_> {
    fn height(&self, node: &Node) -> f32 {
        node_height(self.0, node)
    }

    fn port_offset(&self, node: &Node, port: &Port) -> f32 {
        port_anchor(self.0, node, Position { x: 0.0, y: 0.0 }, port).map_or(0.0, |anchor| anchor.y)
    }
}

/// Where wires attach to a port, in world coordinates.
pub fn port_anchor(
    graph: &Graph,
    node: &Node,
    position: Position,
    port: &Port,
) -> Option<Position> {
    let siblings = match port.direction {
        Direction::Input => inputs(graph, node.id),
        Direction::Output => outputs(graph, node.id),
    };
    let row = siblings.iter().position(|sibling| sibling.id == port.id)?;
    let x = match port.direction {
        Direction::Input => position.x,
        Direction::Output => position.x + NODE_WIDTH,
    };
    let y = position.y + ports_top(node) + (row as f32 + 0.5) * PORT_ROW_HEIGHT;
    Some(Position { x, y })
}

/// Left edge and width of the volume slider track, in world coordinates.
pub fn slider_track(position: Position) -> (f32, f32) {
    let left = position.x + PADDING + MUTE_WIDTH + SLIDER_GAP;
    let width = NODE_WIDTH - 2.0 * PADDING - MUTE_WIDTH - PERCENT_WIDTH - 2.0 * SLIDER_GAP;
    (left, width)
}

pub fn slider_level(position: Position, pointer_x: f32) -> f32 {
    let (left, width) = slider_track(position);
    ((pointer_x - left) / width).clamp(0.0, 1.0) * SLIDER_MAX_LEVEL
}

/// Control points for a wire leaving `from` rightwards and entering `to` from the left.
pub fn wire_controls(from: Position, to: Position) -> (Position, Position) {
    let bend = ((to.x - from.x).abs() * 0.5).max(60.0);
    (
        Position {
            x: from.x + bend,
            y: from.y,
        },
        Position {
            x: to.x - bend,
            y: to.y,
        },
    )
}

pub const HANDLE_SIZE: f32 = 12.0;
/// Where along a wire its remove handle may sit, best first.
const HANDLE_STOPS: [f32; 7] = [0.5, 0.4, 0.6, 0.3, 0.7, 0.2, 0.8];

/// Point at `t` (0 at `from`, 1 at `to`) along the wire.
pub fn wire_point(from: Position, to: Position, t: f32) -> Position {
    let (a, b) = wire_controls(from, to);
    let u = 1.0 - t;
    let weights = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
    let points = [from, a, b, to];
    Position {
        x: weights.iter().zip(&points).map(|(w, p)| w * p.x).sum(),
        y: weights.iter().zip(&points).map(|(w, p)| w * p.y).sum(),
    }
}

/// How far `point` is from the wire, measured along a polyline through the curve.
pub fn wire_distance(from: Position, to: Position, point: Position) -> f32 {
    const SAMPLES: usize = 48;
    let samples: Vec<Position> = (0..=SAMPLES)
        .map(|step| wire_point(from, to, step as f32 / SAMPLES as f32))
        .collect();
    samples
        .windows(2)
        .map(|segment| segment_distance(segment[0], segment[1], point))
        .fold(f32::MAX, f32::min)
}

fn segment_distance(a: Position, b: Position, point: Position) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        (((point.x - a.x) * dx + (point.y - a.y) * dy) / length).clamp(0.0, 1.0)
    };
    (a.x + t * dx - point.x).hypot(a.y + t * dy - point.y)
}

pub fn wire_midpoint(from: Position, to: Position) -> Position {
    wire_point(from, to, 0.5)
}

/// Remove handles for these wires: each at the middle of its wire, or slid along
/// it when that spot would overlap a handle already placed.
pub fn wire_handles(wires: &[(Position, Position)]) -> Vec<Position> {
    let mut handles: Vec<Position> = Vec::with_capacity(wires.len());
    for (from, to) in wires {
        let free = |point: &Position| {
            handles
                .iter()
                .all(|other| (other.x - point.x).hypot(other.y - point.y) >= HANDLE_SIZE + 2.0)
        };
        let handle = HANDLE_STOPS
            .iter()
            .map(|t| wire_point(*from, *to, *t))
            .find(free)
            .unwrap_or_else(|| wire_midpoint(*from, *to));
        handles.push(handle);
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_to_a_wire() {
        let from = Position { x: 0.0, y: 0.0 };
        let to = Position { x: 200.0, y: 100.0 };
        let middle = wire_midpoint(from, to);
        assert!(wire_distance(from, to, middle) < 0.5);
        let beside = Position {
            x: middle.x,
            y: middle.y + 30.0,
        };
        assert!(wire_distance(from, to, beside) > 10.0);
    }

    #[test]
    fn handles_of_wires_from_one_port_do_not_overlap() {
        let from = Position { x: 0.0, y: 0.0 };
        let wires = [
            (from, Position { x: 200.0, y: 4.0 }),
            (from, Position { x: 200.0, y: 26.0 }),
        ];
        let [a, b] = wire_handles(&wires)[..] else {
            panic!("two handles");
        };
        assert!((a.x - b.x).hypot(a.y - b.y) >= HANDLE_SIZE);
    }

    #[test]
    fn slider_maps_track_to_level() {
        let origin = Position { x: 0.0, y: 0.0 };
        let (left, width) = slider_track(origin);
        assert_eq!(slider_level(origin, left), 0.0);
        assert_eq!(slider_level(origin, left + width), SLIDER_MAX_LEVEL);
        assert_eq!(slider_level(origin, left - 50.0), 0.0);
    }

    #[test]
    fn midpoint_of_straight_wire_is_centered() {
        let from = Position { x: 0.0, y: 10.0 };
        let to = Position { x: 400.0, y: 10.0 };
        assert_eq!(wire_midpoint(from, to), Position { x: 200.0, y: 10.0 });
    }
}
