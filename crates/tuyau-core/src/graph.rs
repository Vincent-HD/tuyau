use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Audio,
    Midi,
    Video,
    Other,
}

impl MediaKind {
    /// Classifies a port from its `format.dsp` property, e.g. "32 bit float mono audio".
    pub fn from_format_dsp(format: Option<&str>) -> Self {
        let Some(format) = format else {
            return Self::Other;
        };
        if format.contains("midi") {
            return Self::Midi;
        }
        if format.contains("audio") {
            return Self::Audio;
        }
        if format.contains("video") {
            return Self::Video;
        }
        Self::Other
    }
}

/// Channel volumes are linear amplitudes, as PipeWire stores them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Volume {
    pub channels: Vec<f32>,
    pub mute: bool,
}

impl Volume {
    /// Perceptual level (cubic scale, like pavucontrol and wpctl) averaged over channels.
    pub fn level(&self) -> f32 {
        if self.channels.is_empty() {
            return 0.0;
        }
        let average = self.channels.iter().sum::<f32>() / self.channels.len() as f32;
        linear_to_level(average)
    }
}

pub fn level_to_linear(level: f32) -> f32 {
    level.max(0.0).powi(3)
}

pub fn linear_to_level(linear: f32) -> f32 {
    linear.max(0.0).cbrt()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    #[default]
    Unknown,
    /// Processing data: a stream that is playing or recording.
    Running,
    Idle,
    Suspended,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Node {
    pub id: u32,
    /// `node.name`: the stable identifier used to match nodes across restarts.
    pub name: String,
    pub description: Option<String>,
    pub nick: Option<String>,
    pub media_class: Option<String>,
    pub application: Option<String>,
    /// `media.name`: what a stream carries, e.g. a tab title or a song.
    pub media_name: Option<String>,
    pub state: NodeState,
    pub volume: Option<Volume>,
    /// Every property PipeWire reports for the node.
    pub properties: BTreeMap<String, String>,
}

/// `media.name` values apps use for any stream, which say nothing about it.
const GENERIC_MEDIA_NAMES: [&str; 8] = [
    "playback",
    "playback stream",
    "recordstream",
    "record stream",
    "audio stream",
    "audio-stream",
    "output",
    "input",
];

impl Node {
    /// `media.name` when it tells this stream apart from the app's others. Devices
    /// and filters put internal names there, so only streams have one.
    pub fn media_detail(&self) -> Option<&str> {
        if !self.is_stream() {
            return None;
        }
        let media = self.media_name.as_deref()?.trim();
        let generic = GENERIC_MEDIA_NAMES.contains(&media.to_lowercase().as_str());
        (!media.is_empty() && !generic && media != self.label()).then_some(media)
    }

    pub fn label(&self) -> &str {
        self.description
            .as_deref()
            .or(self.nick.as_deref())
            .or(self.application.as_deref())
            .unwrap_or(&self.name)
    }

    pub fn is_stream(&self) -> bool {
        self.media_class
            .as_deref()
            .is_some_and(|class| class.starts_with("Stream/"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Port {
    pub id: u32,
    pub node_id: u32,
    pub name: String,
    pub alias: Option<String>,
    pub direction: Direction,
    /// `audio.channel`, e.g. "FL", "FR", "MONO".
    pub channel: Option<String>,
    pub media: MediaKind,
    pub physical: bool,
    pub monitor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct PortPair {
    pub output_node: u32,
    pub output_port: u32,
    pub input_node: u32,
    pub input_port: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Link {
    pub id: u32,
    #[serde(flatten)]
    pub ports: PortPair,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Graph {
    pub nodes: BTreeMap<u32, Node>,
    pub ports: BTreeMap<u32, Port>,
    pub links: BTreeMap<u32, Link>,
}

impl Graph {
    /// A node's ports in display order: regular before monitor, then by speaker position.
    pub fn ports_of(&self, node_id: u32, direction: Direction) -> impl Iterator<Item = &Port> {
        let mut ports: Vec<&Port> = self
            .ports
            .values()
            .filter(|port| port.node_id == node_id && port.direction == direction)
            .collect();
        ports.sort_by_key(|port| {
            (
                port.monitor,
                channel_rank(port.channel.as_deref()),
                &port.name,
                port.id,
            )
        });
        ports.into_iter()
    }

    pub fn has_ports(&self, node_id: u32, direction: Direction) -> bool {
        self.ports_of(node_id, direction).next().is_some()
    }

    pub fn find_link(&self, output_port: u32, input_port: u32) -> Option<&Link> {
        self.links.values().find(|link| {
            link.ports.output_port == output_port && link.ports.input_port == input_port
        })
    }

    pub fn links_of_node(&self, node_id: u32) -> impl Iterator<Item = &Link> {
        self.links.values().filter(move |link| {
            link.ports.output_node == node_id || link.ports.input_node == node_id
        })
    }

    /// The dominant media kind of a node, from its ports.
    pub fn node_media(&self, node_id: u32) -> MediaKind {
        self.ports
            .values()
            .filter(|port| port.node_id == node_id)
            .map(|port| port.media)
            .min()
            .unwrap_or(MediaKind::Other)
    }

    pub fn pair(&self, output_port: u32, input_port: u32) -> Option<PortPair> {
        let output = self.ports.get(&output_port)?;
        let input = self.ports.get(&input_port)?;
        if output.direction != Direction::Output || input.direction != Direction::Input {
            return None;
        }
        Some(PortPair {
            output_node: output.node_id,
            output_port,
            input_node: input.node_id,
            input_port,
        })
    }
}

const CHANNEL_ORDER: [&str; 12] = [
    "MONO", "FL", "FR", "FC", "LFE", "SL", "SR", "RL", "RR", "FLC", "FRC", "RC",
];

fn channel_rank(channel: Option<&str>) -> usize {
    channel
        .and_then(|channel| CHANNEL_ORDER.iter().position(|known| *known == channel))
        .unwrap_or(CHANNEL_ORDER.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(application: &str, media_name: &str) -> Node {
        Node {
            application: Some(application.into()),
            media_name: Some(media_name.into()),
            media_class: Some("Stream/Output/Audio".into()),
            ..Node::default()
        }
    }

    #[test]
    fn media_detail_is_for_streams_only() {
        let filter = Node {
            media_class: Some("Audio/Sink".into()),
            ..stream("easyeffects", "ee_soe_compressor")
        };
        assert_eq!(filter.media_detail(), None);
    }

    #[test]
    fn media_detail_skips_generic_names() {
        assert_eq!(stream("Brave", "Playback").media_detail(), None);
        assert_eq!(stream("Firefox", "Firefox").media_detail(), None);
        assert_eq!(
            stream("Firefox", "Lo-fi beats - YouTube").media_detail(),
            Some("Lo-fi beats - YouTube")
        );
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub struct Builder {
        pub graph: Graph,
        next_id: u32,
    }

    impl Builder {
        pub fn new() -> Self {
            Self {
                graph: Graph::default(),
                next_id: 100,
            }
        }

        fn id(&mut self) -> u32 {
            self.next_id += 1;
            self.next_id
        }

        pub fn node(
            &mut self,
            name: &str,
            media_class: &str,
            inputs: &[&str],
            outputs: &[&str],
        ) -> u32 {
            let node_id = self.id();
            self.graph.nodes.insert(
                node_id,
                Node {
                    id: node_id,
                    name: name.to_owned(),
                    media_class: Some(media_class.to_owned()),
                    ..Node::default()
                },
            );
            for (direction, channels) in [(Direction::Input, inputs), (Direction::Output, outputs)]
            {
                for channel in channels {
                    let port_id = self.id();
                    let prefix = match direction {
                        Direction::Input => "playback",
                        Direction::Output => "output",
                    };
                    self.graph.ports.insert(
                        port_id,
                        Port {
                            id: port_id,
                            node_id,
                            name: format!("{prefix}_{channel}"),
                            alias: None,
                            direction,
                            channel: Some((*channel).to_owned()),
                            media: MediaKind::Audio,
                            physical: false,
                            monitor: false,
                        },
                    );
                }
            }
            node_id
        }

        pub fn port(&self, node_id: u32, name: &str) -> u32 {
            self.graph
                .ports
                .values()
                .find(|port| port.node_id == node_id && port.name == name)
                .map(|port| port.id)
                .expect("fixture port exists")
        }

        pub fn link(&mut self, output_port: u32, input_port: u32) -> u32 {
            let link_id = self.id();
            let ports = self
                .graph
                .pair(output_port, input_port)
                .expect("fixture ports pair up");
            self.graph
                .links
                .insert(link_id, Link { id: link_id, ports });
            link_id
        }
    }
}
