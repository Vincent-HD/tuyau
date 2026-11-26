use std::collections::BTreeSet;

use crate::{
    config::{DeviceRule, LinkRule, NodeMatcher, Settings},
    graph::{Direction, Graph, MediaKind, Node, Port, PortPair, level_to_linear},
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkPlan {
    pub create: Vec<PortPair>,
    /// Link ids to remove because an `exclusive` rule owns their output port.
    pub destroy: Vec<u32>,
}

impl LinkPlan {
    pub fn is_empty(&self) -> bool {
        self.create.is_empty() && self.destroy.is_empty()
    }
}

/// Every port pair the config and the GUI's saved links want in the current graph.
pub fn desired_links(graph: &Graph, settings: &Settings) -> BTreeSet<PortPair> {
    settings
        .link_rules()
        .flat_map(|rule| rule_pairs(graph, settings, rule))
        .collect()
}

pub fn plan_links(graph: &Graph, settings: &Settings) -> LinkPlan {
    let desired = desired_links(graph, settings);
    let create = desired
        .iter()
        .filter(|pair| graph.find_link(pair.output_port, pair.input_port).is_none())
        .copied()
        .collect();

    let exclusive_outputs: BTreeSet<u32> = settings
        .link_rules()
        .filter(|rule| rule.exclusive)
        .flat_map(|rule| rule_pairs(graph, settings, rule))
        .map(|pair| pair.output_port)
        .collect();
    let destroy = graph
        .links
        .values()
        .filter(|link| {
            exclusive_outputs.contains(&link.ports.output_port) && !desired.contains(&link.ports)
        })
        .map(|link| link.id)
        .collect();

    LinkPlan { create, destroy }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DevicePlan {
    /// Declared devices with no node of that name yet.
    pub create: Vec<DeviceRule>,
    /// Nodes tuyau created that are no longer declared, or duplicates.
    pub destroy: Vec<u32>,
}

impl DevicePlan {
    pub fn is_empty(&self) -> bool {
        self.create.is_empty() && self.destroy.is_empty()
    }
}

/// Virtual devices to create or remove so the graph has exactly the declared ones.
/// A node of the same name that tuyau did not create (e.g. from the PipeWire
/// config) counts as present and is never touched.
pub fn plan_devices(graph: &Graph, settings: &Settings) -> DevicePlan {
    let mut declared = BTreeSet::new();
    let create = settings
        .device_rules()
        .filter(|rule| declared.insert(rule.name.as_str()))
        .filter(|rule| !graph.nodes.values().any(|node| node.name == rule.name))
        .cloned()
        .collect();
    let mut kept = BTreeSet::new();
    let destroy = graph
        .nodes
        .values()
        .filter(|node| node.is_tuyau_device())
        .filter(|node| !declared.contains(node.name.as_str()) || !kept.insert(node.name.as_str()))
        .map(|node| node.id)
        .collect();
    DevicePlan { create, destroy }
}

/// Nodes with ports in `direction` that `matcher` selects.
fn matching<'a>(
    graph: &'a Graph,
    settings: &'a Settings,
    matcher: &'a NodeMatcher,
    direction: Direction,
) -> impl Iterator<Item = &'a Node> + 'a {
    graph.nodes.values().filter(move |node| {
        let selected = if matcher.is_default_sink() {
            graph.default_sink.as_deref() == Some(node.name.as_str())
        } else {
            settings.matches(matcher, node)
        };
        selected && graph.has_ports(node.id, direction)
    })
}

fn rule_pairs(graph: &Graph, settings: &Settings, rule: &LinkRule) -> Vec<PortPair> {
    matching(graph, settings, &rule.from, Direction::Output)
        .flat_map(|source| {
            matching(graph, settings, &rule.to, Direction::Input)
                .filter(move |sink| sink.id != source.id)
                .flat_map(move |sink| node_pairs(graph, source, sink, rule))
        })
        .collect()
}

/// Pairs the ports of two nodes, either from explicit mappings or by channel.
pub fn node_pairs(graph: &Graph, source: &Node, sink: &Node, rule: &LinkRule) -> Vec<PortPair> {
    let outputs = source_outputs(graph, source.id);
    let inputs: Vec<&Port> = graph.ports_of(sink.id, Direction::Input).collect();
    let pair = |output: &Port, input: &Port| PortPair {
        output_node: source.id,
        output_port: output.id,
        input_node: sink.id,
        input_port: input.id,
    };

    if !rule.ports.is_empty() {
        return rule
            .ports
            .iter()
            .flat_map(|mapping| {
                outputs
                    .iter()
                    .filter(|output| mapping.matches_output(output))
                    .flat_map(|output| {
                        inputs
                            .iter()
                            .filter(|input| mapping.matches_input(input))
                            .map(|input| pair(output, input))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
    }

    [
        MediaKind::Audio,
        MediaKind::Midi,
        MediaKind::Video,
        MediaKind::Other,
    ]
    .into_iter()
    .flat_map(|media| {
        let outputs: Vec<&Port> = outputs
            .iter()
            .copied()
            .filter(|port| port.media == media)
            .collect();
        let inputs: Vec<&Port> = inputs
            .iter()
            .copied()
            .filter(|port| port.media == media)
            .collect();
        auto_pairs(&outputs, &inputs)
            .into_iter()
            .map(|(output, input)| pair(output, input))
            .collect::<Vec<_>>()
    })
    .collect()
}

/// Sinks expose monitor outputs; only use them when the node has nothing else.
fn source_outputs(graph: &Graph, node_id: u32) -> Vec<&Port> {
    let outputs: Vec<&Port> = graph.ports_of(node_id, Direction::Output).collect();
    if outputs.iter().all(|port| port.monitor) {
        return outputs;
    }
    outputs.into_iter().filter(|port| !port.monitor).collect()
}

fn auto_pairs<'a>(outputs: &[&'a Port], inputs: &[&'a Port]) -> Vec<(&'a Port, &'a Port)> {
    if outputs.is_empty() || inputs.is_empty() {
        return Vec::new();
    }
    let by_channel: Vec<_> = outputs
        .iter()
        .flat_map(|output| {
            inputs
                .iter()
                .filter(|input| output.channel.is_some() && input.channel == output.channel)
                .map(move |input| (*output, *input))
        })
        .collect();
    if !by_channel.is_empty() {
        return by_channel;
    }
    if outputs.len() == 1 {
        return inputs.iter().map(|input| (outputs[0], *input)).collect();
    }
    if inputs.len() == 1 {
        return outputs.iter().map(|output| (*output, inputs[0])).collect();
    }
    outputs
        .iter()
        .copied()
        .zip(inputs.iter().copied())
        .collect()
}

/// The volume change the config asks for on a freshly appeared node.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeChange {
    pub node_id: u32,
    pub channels: Option<Vec<f32>>,
    pub mute: Option<bool>,
}

pub fn initial_volume(node: &Node, settings: &Settings) -> Option<VolumeChange> {
    let current = node.volume.as_ref()?;
    let (level, mute) = settings.initial_volume(node);
    if level.is_none() && mute.is_none() {
        return None;
    }
    let channels = level.map(|level| vec![level_to_linear(level); current.channels.len().max(1)]);
    Some(VolumeChange {
        node_id: node.id,
        channels,
        mute,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{config::Config, graph::fixtures::Builder};

    fn settings(text: &str) -> Settings {
        Settings {
            config: Config::parse(text, Path::new("test.toml")).expect("test config parses"),
            ..Settings::default()
        }
    }

    fn mark_as_device(builder: &mut Builder, node: u32) {
        builder
            .graph
            .nodes
            .get_mut(&node)
            .expect("fixture node")
            .properties
            .insert(crate::graph::DEVICE_PROPERTY.into(), "true".into());
    }

    #[test]
    fn devices_are_created_and_removed_to_match_the_rules() {
        let mut builder = Builder::new();
        let kept = builder.node("comms", "Audio/Sink", &["FL", "FR"], &["FL", "FR"]);
        let stale = builder.node("old", "Audio/Sink", &["FL", "FR"], &["FL", "FR"]);
        let duplicate = builder.node("comms", "Audio/Sink", &["FL", "FR"], &["FL", "FR"]);
        let foreign = builder.node("system", "Audio/Sink", &["FL", "FR"], &[]);
        for node in [kept, stale, duplicate] {
            mark_as_device(&mut builder, node);
        }
        let settings = settings(
            r#"
            [[device]]
            name = "comms"
            [[device]]
            name = "system"
            [[device]]
            name = "music"
            kind = "source"
            "#,
        );
        let plan = plan_devices(&builder.graph, &settings);
        let created: Vec<&str> = plan.create.iter().map(|rule| rule.name.as_str()).collect();
        assert_eq!(created, ["music"]);
        assert_eq!(plan.destroy, [stale, duplicate]);
        assert!(!plan.destroy.contains(&foreign));
    }

    #[test]
    fn links_follow_the_default_sink() {
        let mut builder = Builder::new();
        let comms = builder.node("comms", "Audio/Sink", &["FL", "FR"], &["FL", "FR"]);
        let headset = builder.node("headset", "Audio/Sink", &["FL", "FR"], &[]);
        builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        builder.graph.default_sink = Some("headset".into());
        let plan = plan_links(
            &builder.graph,
            &settings("[[link]]\nfrom = \"comms\"\nto = \"@default-sink@\""),
        );
        assert_eq!(plan.create.len(), 2);
        assert!(plan.create.iter().all(|pair| pair.output_node == comms));
        assert!(plan.create.iter().all(|pair| pair.input_node == headset));
    }

    #[test]
    fn links_saved_from_the_gui_are_kept() {
        let mut builder = Builder::new();
        let app = builder.node("app", "Stream/Output/Audio", &[], &["FL", "FR"]);
        let sink = builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        let graph = &builder.graph;
        let output = &graph.ports[&builder.port(app, "output_FL")];
        let input = &graph.ports[&builder.port(sink, "playback_FR")];
        let mut settings = settings("");
        settings.state.links.push(LinkRule::exact(
            &graph.nodes[&app],
            output,
            &graph.nodes[&sink],
            input,
        ));
        let plan = plan_links(graph, &settings);
        assert_eq!(plan.create.len(), 1);
        assert_eq!(plan.create[0].output_port, output.id);
        assert_eq!(plan.create[0].input_port, input.id);
    }

    #[test]
    fn links_matching_channels() {
        let mut builder = Builder::new();
        let app = builder.node("spotify", "Stream/Output/Audio", &[], &["FL", "FR"]);
        let sink = builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        let plan = plan_links(
            &builder.graph,
            &settings("[[link]]\nfrom = \"spotify\"\nto = \"speakers\""),
        );
        let expected = vec![
            PortPair {
                output_node: app,
                output_port: builder.port(app, "output_FL"),
                input_node: sink,
                input_port: builder.port(sink, "playback_FL"),
            },
            PortPair {
                output_node: app,
                output_port: builder.port(app, "output_FR"),
                input_node: sink,
                input_port: builder.port(sink, "playback_FR"),
            },
        ];
        assert_eq!(plan.create, expected);
        assert!(plan.destroy.is_empty());
    }

    #[test]
    fn mono_source_fans_out() {
        let mut builder = Builder::new();
        builder.node("mic", "Audio/Source", &[], &["MONO"]);
        builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        let plan = plan_links(
            &builder.graph,
            &settings("[[link]]\nfrom = \"mic\"\nto = \"speakers\""),
        );
        assert_eq!(plan.create.len(), 2);
    }

    #[test]
    fn existing_links_are_kept() {
        let mut builder = Builder::new();
        let app = builder.node("spotify", "Stream/Output/Audio", &[], &["FL", "FR"]);
        let sink = builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        builder.link(
            builder.port(app, "output_FL"),
            builder.port(sink, "playback_FL"),
        );
        let plan = plan_links(
            &builder.graph,
            &settings("[[link]]\nfrom = \"spotify\"\nto = \"speakers\""),
        );
        assert_eq!(plan.create.len(), 1);
    }

    #[test]
    fn exclusive_rule_removes_other_links() {
        let mut builder = Builder::new();
        let app = builder.node("spotify", "Stream/Output/Audio", &[], &["FL", "FR"]);
        let speakers = builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        let headphones = builder.node("headphones", "Audio/Sink", &["FL", "FR"], &[]);
        let stray = builder.link(
            builder.port(app, "output_FL"),
            builder.port(speakers, "playback_FL"),
        );
        let unrelated_app = builder.node("firefox", "Stream/Output/Audio", &[], &["FL"]);
        builder.link(
            builder.port(unrelated_app, "output_FL"),
            builder.port(speakers, "playback_FL"),
        );

        let plan = plan_links(
            &builder.graph,
            &settings("[[link]]\nfrom = \"spotify\"\nto = \"headphones\"\nexclusive = true"),
        );
        assert_eq!(plan.destroy, vec![stray]);
        assert!(plan.create.iter().all(|pair| pair.input_node == headphones));
    }

    #[test]
    fn explicit_port_mapping() {
        let mut builder = Builder::new();
        let app = builder.node("synth", "Stream/Output/Audio", &[], &["FL", "FR"]);
        let sink = builder.node("speakers", "Audio/Sink", &["FL", "FR"], &[]);
        let plan = plan_links(
            &builder.graph,
            &settings("[[link]]\nfrom = \"synth\"\nto = \"speakers\"\nports = [[\"FL\", \"FR\"]]"),
        );
        assert_eq!(
            plan.create,
            vec![PortPair {
                output_node: app,
                output_port: builder.port(app, "output_FL"),
                input_node: sink,
                input_port: builder.port(sink, "playback_FR"),
            }]
        );
    }

    #[test]
    fn matches_through_alias() {
        let mut builder = Builder::new();
        builder.node("spotify", "Stream/Output/Audio", &[], &["FL", "FR"]);
        builder.node(
            "alsa_output.usb-focusrite",
            "Audio/Sink",
            &["FL", "FR"],
            &[],
        );
        let plan = plan_links(
            &builder.graph,
            &settings(
                "[[node]]\nmatch = \"alsa_output.usb-*\"\nalias = \"Monitors\"\n\n[[link]]\nfrom = \"spotify\"\nto = \"monitors\"",
            ),
        );
        assert_eq!(plan.create.len(), 2);
    }
}
