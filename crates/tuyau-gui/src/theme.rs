use std::collections::{BTreeMap, BTreeSet};

use gpui::{Hsla, Rgba, Window, WindowAppearance, hsla, rgb, rgb_to_hsla, rgba};
use tuyau_core::{Graph, MediaKind};

pub struct Theme {
    pub background: Rgba,
    pub toolbar: Rgba,
    pub card: Rgba,
    pub header: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub muted_text: Rgba,
    pub button: Rgba,
    pub button_hover: Rgba,
    pub accent: Rgba,
    pub danger: Rgba,
    pub track: Rgba,
    pub port_hover: Rgba,
    wire_lightness: f32,
}

impl Theme {
    pub fn for_window(window: &Window) -> Self {
        match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    fn dark() -> Self {
        Self {
            background: rgb(0x16181d),
            toolbar: rgb(0x1e2128),
            card: rgb(0x23262e),
            header: rgb(0x2b2f38),
            border: rgb(0x383d48),
            text: rgb(0xe6e8ee),
            muted_text: rgb(0x8b92a1),
            button: rgb(0x2b2f38),
            button_hover: rgb(0x363b46),
            accent: rgb(0x5b9cf5),
            danger: rgb(0xe5534b),
            track: rgb(0x3a3f4b),
            port_hover: rgba(0x5b9cf533),
            wire_lightness: 0.58,
        }
    }

    fn light() -> Self {
        Self {
            background: rgb(0xeef0f4),
            toolbar: rgb(0xfafbfc),
            card: rgb(0xffffff),
            header: rgb(0xf3f4f7),
            border: rgb(0xd3d7df),
            text: rgb(0x1c1f26),
            muted_text: rgb(0x687083),
            button: rgb(0xeceef2),
            button_hover: rgb(0xdfe2e8),
            accent: rgb(0x2f74d0),
            danger: rgb(0xcf3a33),
            track: rgb(0xd9dce3),
            port_hover: rgba(0x2f74d026),
            wire_lightness: 0.45,
        }
    }

    pub fn media(&self, media: MediaKind) -> Rgba {
        match media {
            MediaKind::Audio => rgb(0x3fb37f),
            MediaKind::Midi => rgb(0xe0823d),
            MediaKind::Video => rgb(0xa371f7),
            MediaKind::Other => self.muted_text,
        }
    }

    /// Audio wires take their source's hue from [`AUDIO_HUES`]; other media keep
    /// their color. Left channels are a bit lighter and right ones a bit darker.
    pub fn wire(&self, media: MediaKind, slot: usize, channel: Option<&str>) -> Hsla {
        let mut color = match media {
            MediaKind::Audio => hsla(
                AUDIO_HUES[slot % AUDIO_HUES.len()] / 360.0,
                0.55,
                self.wire_lightness,
                1.0,
            ),
            _ => rgb_to_hsla(self.media(media)),
        };
        let shift = match channel_side(channel) {
            Some(Side::Left) => CHANNEL_SHIFT,
            Some(Side::Right) => -CHANNEL_SHIFT,
            None => 0.0,
        };
        color.color.lightness = (color.color.lightness + shift).clamp(0.0, 1.0);
        color
    }
}

/// Distinct hues, green first to match the audio color; red is left to
/// destructive actions.
const AUDIO_HUES: [f32; 8] = [152.0, 200.0, 40.0, 275.0, 95.0, 325.0, 178.0, 225.0];
const CHANNEL_SHIFT: f32 = 0.09;

enum Side {
    Left,
    Right,
}

/// FL, RL, SL, … are left; FR, RR, SR, … are right.
fn channel_side(channel: Option<&str>) -> Option<Side> {
    match channel?.chars().last()? {
        'L' => Some(Side::Left),
        'R' => Some(Side::Right),
        _ => None,
    }
}

/// The slot a node gets unless a neighbour already has it: derived from its name,
/// so a node keeps its color across restarts.
fn preferred_slot(name: &str) -> usize {
    // FNV-1a: tiny and stable across Rust versions, unlike `DefaultHasher`.
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    (hash % AUDIO_HUES.len() as u64) as usize
}

/// A palette slot per source node. A node that shares no input with another
/// keeps the default green; nodes feeding the same node get different colors
/// whenever the palette has enough of them.
pub fn wire_slots(graph: &Graph) -> BTreeMap<u32, usize> {
    let mut targets: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for link in graph.links.values() {
        targets
            .entry(link.ports.output_node)
            .or_default()
            .insert(link.ports.input_node);
    }
    let name = |id: &u32| {
        graph
            .nodes
            .get(id)
            .map_or_else(|| id.to_string(), |node| node.name.clone())
    };
    // Ids change across restarts and names do not, so names decide who keeps
    // their preferred color when two collide.
    let mut sources: Vec<(String, u32)> = targets.keys().map(|id| (name(id), *id)).collect();
    sources.sort();
    let mut slots = BTreeMap::new();
    for (name, source) in sources {
        let fed = &targets[&source];
        let rivals: Vec<u32> = targets
            .iter()
            .filter(|(other, other_fed)| **other != source && !other_fed.is_disjoint(fed))
            .map(|(other, _)| *other)
            .collect();
        if rivals.is_empty() {
            slots.insert(source, 0);
            continue;
        }
        let taken: BTreeSet<usize> = rivals
            .iter()
            .filter_map(|other| slots.get(other).copied())
            .collect();
        let preferred = preferred_slot(&name);
        let slot = (0..AUDIO_HUES.len())
            .map(|offset| (preferred + offset) % AUDIO_HUES.len())
            .find(|slot| !taken.contains(slot))
            .unwrap_or(preferred);
        slots.insert(source, slot);
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuyau_core::{Link, PortPair};

    fn link(graph: &mut Graph, id: u32, output_node: u32, input_node: u32) {
        let ports = PortPair {
            output_node,
            output_port: id * 10,
            input_node,
            input_port: id * 10 + 1,
        };
        graph.links.insert(id, Link { id, ports });
    }

    #[test]
    fn sources_of_one_node_get_different_slots() {
        let mut graph = Graph::default();
        link(&mut graph, 1, 1, 9);
        link(&mut graph, 2, 2, 9);
        link(&mut graph, 3, 3, 9);
        link(&mut graph, 4, 4, 8);
        let slots = wire_slots(&graph);
        let feeding: BTreeSet<usize> = [1, 2, 3].iter().map(|node| slots[node]).collect();
        assert_eq!(feeding.len(), 3);
        assert_eq!(slots[&4], 0, "a lone source keeps the default color");
    }

    #[test]
    fn channels_shade_the_same_hue() {
        let theme = Theme::dark();
        let left = theme.wire(MediaKind::Audio, 1, Some("FL"));
        let right = theme.wire(MediaKind::Audio, 1, Some("FR"));
        assert_eq!(left.color.hue, right.color.hue);
        assert!(left.color.lightness > right.color.lightness);
    }
}
