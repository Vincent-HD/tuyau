use std::{path::PathBuf, time::Duration};

use anyhow::{Context, bail};
use clap::{Subcommand, ValueEnum};
use tuyau_core::{
    Config, Direction, Graph, LinkRule, Node, NodeMatcher, Pattern, Settings, State, Ui,
    config::PortMapping,
    graph::level_to_linear,
    reconcile::{desired_links, plan_devices, plan_links},
};
use tuyau_pipewire::{Command as PipeWireCommand, Connection, Event, Reconciler};

const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

#[derive(Subcommand)]
pub enum Command {
    /// Open the graph editor (the default)
    Gui,
    /// List nodes with their ports and links
    #[command(alias = "ls")]
    List {
        /// Print the raw graph as JSON
        #[arg(long)]
        json: bool,
        /// Include nodes hidden by the config or GUI
        #[arg(long)]
        all: bool,
    },
    /// Link two nodes, pairing channels automatically
    Link {
        /// Source node: id, name, description or alias (globs allowed)
        from: String,
        /// Destination node
        to: String,
        /// Explicit OUTPUT:INPUT port pairs, e.g. FL:FR
        #[arg(short, long = "port")]
        ports: Vec<String>,
        /// Remove the other links leaving the source ports
        #[arg(long)]
        exclusive: bool,
    },
    /// Remove every link between two nodes
    Unlink { from: String, to: String },
    /// Show or set a node's volume: 80%, 0.8, +5% or -5%
    Volume {
        node: String,
        #[arg(allow_hyphen_values = true)]
        level: Option<String>,
    },
    /// Mute, unmute or toggle a node
    Mute {
        node: String,
        #[arg(value_enum, default_value = "toggle")]
        action: MuteAction,
    },
    /// Apply the config once: create its links and set its volumes
    Apply {
        /// Show what would change without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Keep applying the config as nodes come and go (auto-reconnect)
    Watch,
    /// Validate the config file
    Check,
    /// Manage what the GUI saves (renames, hidden nodes, hand-made links, layout)
    State {
        #[command(subcommand)]
        action: StateAction,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum MuteAction {
    On,
    Off,
    Toggle,
}

#[derive(Subcommand)]
pub enum StateAction {
    /// Print renames, hidden nodes and hand-made links as config rules
    Export,
    /// Forget every GUI tweak, layout included
    Clear,
    /// Print the paths of the state and layout files
    Path,
}

pub fn run(command: Command, paths: &Paths) -> anyhow::Result<()> {
    let settings = || -> anyhow::Result<Settings> {
        Ok(Settings {
            config: Config::load(&paths.config)?,
            state: State::load(&paths.state)?,
            ..Settings::default()
        })
    };
    match command {
        Command::Gui => unreachable!("handled by main"),
        Command::List { json, all } => {
            let settings = settings()?;
            let graph = connect()?.1;
            if json {
                println!("{}", serde_json::to_string_pretty(&*graph)?);
            } else {
                print_graph(&graph, &settings, all);
            }
            Ok(())
        }
        Command::Link {
            from,
            to,
            ports,
            exclusive,
        } => {
            let settings = settings()?;
            let (mut connection, graph) = connect()?;
            let rule = LinkRule {
                from: matcher(&from)?,
                to: matcher(&to)?,
                ports: ports
                    .iter()
                    .map(|pair| port_mapping(pair))
                    .collect::<Result<_, _>>()?,
                exclusive,
            };
            // Only this rule: links saved from the GUI are not part of this command.
            let settings = Settings {
                config: Config {
                    nodes: settings.config.nodes,
                    links: vec![rule],
                    devices: Vec::new(),
                },
                state: State {
                    links: Vec::new(),
                    ..settings.state
                },
                ui: settings.ui,
            };
            if desired_links(&graph, &settings).is_empty() {
                bail!("no compatible ports between {from:?} and {to:?}");
            }
            let plan = plan_links(&graph, &settings);
            if plan.is_empty() {
                println!("already linked");
                return Ok(());
            }
            for pair in plan.create {
                connection.send(PipeWireCommand::Link(pair));
            }
            for id in plan.destroy {
                connection.send(PipeWireCommand::Unlink(id));
            }
            connection.roundtrip(TIMEOUT)?;
            println!("linked {from} → {to}");
            Ok(())
        }
        Command::Unlink { from, to } => {
            let settings = settings()?;
            let (mut connection, graph) = connect()?;
            let sources = ids(select(&graph, &settings, &from)?);
            let sinks = ids(select(&graph, &settings, &to)?);
            let links: Vec<u32> = graph
                .links
                .values()
                .filter(|link| {
                    sources.contains(&link.ports.output_node)
                        && sinks.contains(&link.ports.input_node)
                })
                .map(|link| link.id)
                .collect();
            if links.is_empty() {
                println!("nothing to unlink");
                return Ok(());
            }
            for id in &links {
                connection.send(PipeWireCommand::Unlink(*id));
            }
            connection.roundtrip(TIMEOUT)?;
            println!("removed {} link(s)", links.len());
            Ok(())
        }
        Command::Volume { node, level } => {
            let settings = settings()?;
            let (mut connection, graph) = connect()?;
            let nodes = with_volume(select(&graph, &settings, &node)?)?;
            let Some(level) = level else {
                for node in nodes {
                    let volume = node.volume.as_ref().expect("filtered on volume");
                    println!(
                        "{}: {:.0}%{}",
                        settings.display_name(node),
                        volume.level() * 100.0,
                        if volume.mute { " (muted)" } else { "" }
                    );
                }
                return Ok(());
            };
            let change = parse_level(&level)?;
            for node in nodes {
                let volume = node.volume.as_ref().expect("filtered on volume");
                let target = match change {
                    LevelChange::Absolute(level) => level,
                    LevelChange::Relative(delta) => (volume.level() + delta).max(0.0),
                };
                let channels = vec![level_to_linear(target); volume.channels.len().max(1)];
                connection.send(PipeWireCommand::SetVolume {
                    node_id: node.id,
                    channels,
                });
                println!("{}: {:.0}%", settings.display_name(node), target * 100.0);
            }
            connection.roundtrip(TIMEOUT)?;
            Ok(())
        }
        Command::Mute { node, action } => {
            let settings = settings()?;
            let (mut connection, graph) = connect()?;
            for node in with_volume(select(&graph, &settings, &node)?)? {
                let current = node.volume.as_ref().is_some_and(|volume| volume.mute);
                let mute = match action {
                    MuteAction::On => true,
                    MuteAction::Off => false,
                    MuteAction::Toggle => !current,
                };
                connection.send(PipeWireCommand::SetMute {
                    node_id: node.id,
                    mute,
                });
                println!(
                    "{}: {}",
                    settings.display_name(node),
                    if mute { "muted" } else { "unmuted" }
                );
            }
            connection.roundtrip(TIMEOUT)?;
            Ok(())
        }
        Command::Apply { dry_run } => {
            let settings = settings()?;
            let (mut connection, graph) = connect()?;
            if dry_run {
                let devices = plan_devices(&graph, &settings);
                for rule in &devices.create {
                    println!("+ device {}", rule.name);
                }
                for id in &devices.destroy {
                    if let Some(node) = graph.nodes.get(id) {
                        println!("- device {}", node.name);
                    }
                }
                let plan = plan_links(&graph, &settings);
                for pair in &plan.create {
                    println!("+ {}", describe_pair(&graph, &settings, pair));
                }
                for id in &plan.destroy {
                    if let Some(link) = graph.links.get(id) {
                        println!("- {}", describe_pair(&graph, &settings, &link.ports));
                    }
                }
                if devices.is_empty() && plan.is_empty() {
                    println!("devices and links are up to date");
                }
                return Ok(());
            }
            let commands = connection.commands();
            let mut reconciler = Reconciler::default();
            let plan = reconciler.reconcile(&graph, &settings, &commands);
            let mut graph = connection.roundtrip(TIMEOUT)?;
            let mut links = plan.links;
            // Links to new devices can only be planned once their ports exist.
            if !plan.devices.create.is_empty() {
                let has_ports = |graph: &Graph| {
                    plan.devices.create.iter().all(|rule| {
                        graph.nodes.values().any(|node| {
                            node.name == rule.name
                                && graph.ports.values().any(|port| port.node_id == node.id)
                        })
                    })
                };
                for _ in 0..5 {
                    if has_ports(&graph) {
                        break;
                    }
                    graph = connection.roundtrip(TIMEOUT)?;
                }
                let more = reconciler.reconcile(&graph, &settings, &commands).links;
                links.create.extend(more.create);
                links.destroy.extend(more.destroy);
                connection.roundtrip(TIMEOUT)?;
            }
            println!(
                "created {} device(s), removed {} device(s), created {} link(s), removed {} link(s)",
                plan.devices.create.len(),
                plan.devices.destroy.len(),
                links.create.len(),
                links.destroy.len()
            );
            Ok(())
        }
        Command::Watch => watch(paths),
        Command::Check => {
            if !paths.config.exists() {
                println!(
                    "{} does not exist; using an empty config",
                    paths.config.display()
                );
                return Ok(());
            }
            let config = Config::load(&paths.config)?;
            println!(
                "{}: {} node rule(s), {} link rule(s), {} device(s)",
                paths.config.display(),
                config.nodes.len(),
                config.links.len(),
                config.devices.len()
            );
            Ok(())
        }
        Command::State { action } => match action {
            StateAction::Export => {
                let config = State::load(&paths.state)?.to_config();
                print!(
                    "{}",
                    toml::to_string_pretty(&config).context("cannot render state")?
                );
                Ok(())
            }
            StateAction::Clear => {
                let ui = Ui::path_for(&paths.state);
                State::default().save(&paths.state)?;
                Ui::default().save(&ui)?;
                println!("cleared {} and {}", paths.state.display(), ui.display());
                Ok(())
            }
            StateAction::Path => {
                println!("{}", paths.state.display());
                println!("{}", Ui::path_for(&paths.state).display());
                Ok(())
            }
        },
    }
}

fn connect() -> anyhow::Result<(Connection, std::sync::Arc<Graph>)> {
    let mut connection = Connection::spawn()?;
    let graph = connection.roundtrip(TIMEOUT)?;
    // Node params (volumes) arrive after the registry; one more roundtrip collects them.
    let graph = connection.roundtrip(TIMEOUT).unwrap_or(graph);
    Ok((connection, graph))
}

fn watch(paths: &Paths) -> anyhow::Result<()> {
    let mut settings = Settings {
        config: Config::load(&paths.config)?,
        state: State::load(&paths.state)?,
        ..Settings::default()
    };
    let mut modified = config_modified(paths);
    let connection = Connection::spawn()?;
    let commands = connection.commands();
    let events = connection.events();
    let mut reconciler = Reconciler::default();
    eprintln!("watching {}", paths.config.display());
    loop {
        match events.recv_blocking() {
            Ok(Event::Graph(graph)) => {
                let current = config_modified(paths);
                if current != modified {
                    modified = current;
                    // The state file holds the links made in the GUI.
                    match (Config::load(&paths.config), State::load(&paths.state)) {
                        (Ok(config), Ok(state)) => {
                            settings.config = config;
                            settings.state = state;
                            reconciler.reset();
                            eprintln!("reloaded {}", paths.config.display());
                        }
                        (Err(error), _) | (_, Err(error)) => {
                            eprintln!("keeping previous settings: {error}");
                        }
                    }
                }
                let plan = reconciler.reconcile(&graph, &settings, &commands);
                for rule in &plan.devices.create {
                    eprintln!("+ device {}", rule.name);
                }
                for id in &plan.devices.destroy {
                    if let Some(node) = graph.nodes.get(id) {
                        eprintln!("- device {}", node.name);
                    }
                }
                let plan = plan.links;
                for pair in &plan.create {
                    eprintln!("+ {}", describe_pair(&graph, &settings, pair));
                }
                for id in &plan.destroy {
                    if let Some(link) = graph.links.get(id) {
                        eprintln!("- {}", describe_pair(&graph, &settings, &link.ports));
                    }
                }
            }
            Ok(Event::Error(message)) => eprintln!("pipewire: {message}"),
            Ok(Event::Synced(_)) => {}
            Ok(Event::Disconnected) | Err(_) => bail!("disconnected from PipeWire"),
        }
    }
}

fn config_modified(paths: &Paths) -> [Option<std::time::SystemTime>; 2] {
    [&paths.config, &paths.state].map(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    })
}

fn matcher(query: &str) -> anyhow::Result<NodeMatcher> {
    Ok(NodeMatcher::Any(
        Pattern::new(query).with_context(|| format!("invalid pattern {query:?}"))?,
    ))
}

fn port_mapping(pair: &str) -> anyhow::Result<PortMapping> {
    let (output, input) = pair
        .split_once(':')
        .with_context(|| format!("expected OUTPUT:INPUT, got {pair:?}"))?;
    Ok(PortMapping(Pattern::new(output)?, Pattern::new(input)?))
}

fn select<'a>(graph: &'a Graph, settings: &Settings, query: &str) -> anyhow::Result<Vec<&'a Node>> {
    let nodes: Vec<&Node> = match query.parse::<u32>() {
        Ok(id) => graph.nodes.get(&id).into_iter().collect(),
        Err(_) => {
            let matcher = matcher(query)?;
            graph
                .nodes
                .values()
                .filter(|node| settings.matches(&matcher, node))
                .collect()
        }
    };
    if nodes.is_empty() {
        bail!("no node matches {query:?}");
    }
    Ok(nodes)
}

fn with_volume(nodes: Vec<&Node>) -> anyhow::Result<Vec<&Node>> {
    let nodes: Vec<&Node> = nodes
        .into_iter()
        .filter(|node| node.volume.is_some())
        .collect();
    if nodes.is_empty() {
        bail!("none of the matching nodes has a volume");
    }
    Ok(nodes)
}

fn ids(nodes: Vec<&Node>) -> Vec<u32> {
    nodes.into_iter().map(|node| node.id).collect()
}

#[derive(Debug, PartialEq)]
enum LevelChange {
    Absolute(f32),
    Relative(f32),
}

fn parse_level(text: &str) -> anyhow::Result<LevelChange> {
    let (relative, sign, rest) = match text.as_bytes().first() {
        Some(b'+') => (true, 1.0, &text[1..]),
        Some(b'-') => (true, -1.0, &text[1..]),
        _ => (false, 1.0, text),
    };
    let value = match rest.strip_suffix('%') {
        Some(percent) => percent.trim().parse::<f32>().map(|value| value / 100.0),
        None => rest.trim().parse::<f32>(),
    }
    .with_context(|| format!("invalid volume {text:?}, expected 80%, 0.8, +5% or -5%"))?;
    Ok(if relative {
        LevelChange::Relative(sign * value)
    } else {
        LevelChange::Absolute(value)
    })
}

fn print_graph(graph: &Graph, settings: &Settings, all: bool) {
    for node in graph.nodes.values() {
        if !all && settings.is_hidden(node) {
            continue;
        }
        let mut header = format!("{:>5}  {}", node.id, settings.display_name(node));
        if settings.display_name(node) != node.name {
            header.push_str(&format!("  [{}]", node.name));
        }
        if let Some(class) = &node.media_class {
            header.push_str(&format!("  {class}"));
        }
        if let Some(volume) = &node.volume {
            header.push_str(&format!("  {:.0}%", volume.level() * 100.0));
            if volume.mute {
                header.push_str(" muted");
            }
        }
        println!("{header}");
        for direction in [Direction::Output, Direction::Input] {
            for port in graph.ports_of(node.id, direction) {
                let peers: Vec<String> = graph
                    .links
                    .values()
                    .filter_map(|link| match direction {
                        Direction::Output if link.ports.output_port == port.id => {
                            Some(describe_endpoint(
                                graph,
                                settings,
                                link.ports.input_node,
                                link.ports.input_port,
                            ))
                        }
                        Direction::Input if link.ports.input_port == port.id => {
                            Some(describe_endpoint(
                                graph,
                                settings,
                                link.ports.output_node,
                                link.ports.output_port,
                            ))
                        }
                        _ => None,
                    })
                    .collect();
                let arrow = match direction {
                    Direction::Output => "→",
                    Direction::Input => "←",
                };
                if peers.is_empty() {
                    println!("         {} {}", arrow, port.name);
                } else {
                    println!("         {} {}  {}", arrow, port.name, peers.join(", "));
                }
            }
        }
    }
}

fn describe_endpoint(graph: &Graph, settings: &Settings, node_id: u32, port_id: u32) -> String {
    let node = graph
        .nodes
        .get(&node_id)
        .map(|node| settings.display_name(node))
        .unwrap_or("?");
    let port = graph
        .ports
        .get(&port_id)
        .map(|port| port.name.as_str())
        .unwrap_or("?");
    format!("{node}:{port}")
}

fn describe_pair(graph: &Graph, settings: &Settings, pair: &tuyau_core::PortPair) -> String {
    format!(
        "{} → {}",
        describe_endpoint(graph, settings, pair.output_node, pair.output_port),
        describe_endpoint(graph, settings, pair.input_node, pair.input_port)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_levels() {
        assert_eq!(parse_level("80%").unwrap(), LevelChange::Absolute(0.8));
        assert_eq!(parse_level("0.5").unwrap(), LevelChange::Absolute(0.5));
        assert_eq!(parse_level("+5%").unwrap(), LevelChange::Relative(0.05));
        assert_eq!(parse_level("-10%").unwrap(), LevelChange::Relative(-0.1));
        assert!(parse_level("loud").is_err());
    }
}
