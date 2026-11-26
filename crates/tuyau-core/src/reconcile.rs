use std::collections::BTreeSet;

use crate::{
    config::{LinkRule, Settings},
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

fn rule_pairs(graph: &Graph, settings: &Settings, rule: &LinkRule) -> Vec<PortPair> {
    let matching = |matcher, direction| {
        graph.nodes.values().filter(move |node| {
            graph.has_ports(node.id, direction) && settings.matches(matcher, node)
        })
    };
    matching(&rule.from, Direction::Output)
        .flat_map(|source| {
            matching(&rule.to, Direction::Input)
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
