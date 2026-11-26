use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::graph::{Node, Port};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {reason}")]
    Read {
        path: PathBuf,
        reason: std::io::Error,
    },
    #[error("cannot write {path}: {reason}")]
    Write {
        path: PathBuf,
        reason: std::io::Error,
    },
    #[error("invalid {path}: {reason}")]
    Parse {
        path: PathBuf,
        reason: Box<toml::de::Error>,
    },
}

/// A case-insensitive glob, e.g. `"alsa_output.usb-*"` or `"Firefox"`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Pattern {
    source: String,
    matcher: GlobMatcher,
}

impl Pattern {
    pub fn new(source: &str) -> Result<Self, globset::Error> {
        let matcher = GlobBuilder::new(source)
            .case_insensitive(true)
            .literal_separator(false)
            .build()?
            .compile_matcher();
        Ok(Self {
            source: source.to_owned(),
            matcher,
        })
    }

    pub fn is_match(&self, value: &str) -> bool {
        self.matcher.is_match(value)
    }

    fn matches_any<'a>(&self, values: impl IntoIterator<Item = Option<&'a str>>) -> bool {
        values
            .into_iter()
            .flatten()
            .any(|value| self.is_match(value))
    }
}

impl TryFrom<String> for Pattern {
    type Error = globset::Error;

    fn try_from(source: String) -> Result<Self, Self::Error> {
        Self::new(&source)
    }
}

impl From<Pattern> for String {
    fn from(pattern: Pattern) -> Self {
        pattern.source
    }
}

impl fmt::Debug for Pattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}", self.source)
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

/// Selects nodes. A bare string matches the node name, description, nick,
/// application name or alias; a table restricts which fields are compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NodeMatcher {
    Any(Pattern),
    Fields(Box<NodeFields>),
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct NodeFields {
    pub name: Option<Pattern>,
    pub description: Option<Pattern>,
    pub application: Option<Pattern>,
    pub media_class: Option<Pattern>,
    pub alias: Option<Pattern>,
}

impl NodeMatcher {
    pub fn matches(&self, node: &Node, alias: Option<&str>) -> bool {
        match self {
            Self::Any(pattern) => pattern.matches_any([
                Some(node.name.as_str()),
                node.description.as_deref(),
                node.nick.as_deref(),
                node.application.as_deref(),
                alias,
            ]),
            Self::Fields(fields) => {
                let check = |pattern: &Option<Pattern>, value: Option<&str>| {
                    pattern
                        .as_ref()
                        .is_none_or(|pattern| value.is_some_and(|value| pattern.is_match(value)))
                };
                check(&fields.name, Some(&node.name))
                    && check(&fields.description, node.description.as_deref())
                    && check(&fields.application, node.application.as_deref())
                    && check(&fields.media_class, node.media_class.as_deref())
                    && check(&fields.alias, alias)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct NodeRule {
    #[serde(rename = "match")]
    pub matcher: NodeMatcher,
    pub alias: Option<String>,
    /// Perceptual level applied when the node appears: 1.0 is 100%.
    pub volume: Option<f32>,
    pub mute: Option<bool>,
    pub hidden: Option<bool>,
    pub position: Option<Position>,
}

/// Matches a port by name, alias or audio channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortMapping(pub Pattern, pub Pattern);

impl PortMapping {
    pub fn matches_output(&self, port: &Port) -> bool {
        port_matches(&self.0, port)
    }

    pub fn matches_input(&self, port: &Port) -> bool {
        port_matches(&self.1, port)
    }
}

fn port_matches(pattern: &Pattern, port: &Port) -> bool {
    pattern.matches_any([
        Some(port.name.as_str()),
        port.alias.as_deref(),
        port.channel.as_deref(),
    ])
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct LinkRule {
    pub from: NodeMatcher,
    pub to: NodeMatcher,
    /// Explicit `[output, input]` port pairs. Channels are paired automatically when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMapping>,
    /// Remove every other link leaving the matched output ports.
    #[serde(default)]
    pub exclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[serde(default, rename = "node", skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeRule>,
    #[serde(default, rename = "link", skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkRule>,
}

impl Config {
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            reason: Box::new(source),
        })
    }

    /// Loads the config, treating a missing file as an empty config.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_owned(),
                reason: source,
            }),
        }
    }

    pub fn default_path() -> PathBuf {
        std::env::var_os("TUYAU_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::config_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("tuyau/config.toml")
            })
    }
}

/// Choices made from the GUI that are worth keeping: renames, hidden nodes and
/// links made by hand. It changes only on those edits. The config file stays
/// read-only (it may live in the Nix store); `tuyau state export` turns this into
/// config rules.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct State {
    pub aliases: BTreeMap<String, String>,
    pub hidden: BTreeSet<String>,
    #[serde(rename = "link", skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkRule>,
}

/// Editor state that changes all the time and is not meant to be versioned:
/// node positions, the view, toggles.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Ui {
    pub positions: BTreeMap<String, Position>,
    /// Where the editor was looking when it closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<View>,
    /// Lay the whole graph out again whenever nodes or links come and go.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub auto_arrange: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub x: f32,
    pub y: f32,
    pub zoom: f32,
}

/// Keys of [`Ui`] that older versions kept in the state file.
const UI_KEYS: [&str; 3] = ["positions", "view", "auto-arrange"];

fn read_table(path: &Path) -> Result<Option<toml::Table>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                reason: Box::new(source),
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            reason: source,
        }),
    }
}

fn from_table<T: serde::de::DeserializeOwned>(
    table: toml::Table,
    path: &Path,
) -> Result<T, ConfigError> {
    table.try_into().map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        reason: Box::new(source),
    })
}

fn save_toml(value: &impl Serialize, path: &Path) -> Result<(), ConfigError> {
    let write_error = |source| ConfigError::Write {
        path: path.to_owned(),
        reason: source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(write_error)?;
    }
    let text = toml::to_string_pretty(value).expect("GUI state serializes to TOML");
    std::fs::write(path, text).map_err(write_error)
}

impl State {
    pub fn default_path() -> PathBuf {
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tuyau/state.toml")
    }

    /// Loads the state, treating a missing file as empty. Editor keys left there
    /// by older versions are skipped; [`Ui::load`] picks them up.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let Some(mut table) = read_table(path)? else {
            return Ok(Self::default());
        };
        for key in UI_KEYS {
            table.remove(key);
        }
        from_table(table, path)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        save_toml(self, path)
    }

    /// Rules reproducing this state, keyed by exact node name.
    pub fn to_config(&self) -> Config {
        let names: BTreeSet<&String> = self.aliases.keys().chain(self.hidden.iter()).collect();
        let nodes = names
            .into_iter()
            .map(|name| NodeRule {
                matcher: NodeMatcher::exact_name(name),
                alias: self.aliases.get(name).cloned(),
                volume: None,
                mute: None,
                hidden: self.hidden.contains(name).then_some(true),
                position: None,
            })
            .collect();
        Config {
            nodes,
            links: self.links.clone(),
        }
    }
}

impl Ui {
    /// The editor file that goes with a state file.
    pub fn path_for(state_path: &Path) -> PathBuf {
        state_path.with_file_name("ui.toml")
    }

    /// Loads the editor state. When its file does not exist yet, takes what older
    /// versions kept in the state file instead.
    pub fn load(path: &Path, state_path: &Path) -> Result<Self, ConfigError> {
        if let Some(table) = read_table(path)? {
            return from_table(table, path);
        }
        let Some(mut legacy) = read_table(state_path)? else {
            return Ok(Self::default());
        };
        legacy.retain(|key, _| UI_KEYS.contains(&key));
        from_table(legacy, state_path)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        save_toml(self, path)
    }
}

impl NodeMatcher {
    /// Matches the nodes named exactly `name`.
    pub fn exact_name(name: &str) -> Self {
        Self::Fields(Box::new(NodeFields {
            name: Some(Pattern::new(&escape_glob(name)).expect("escaped glob is valid")),
            ..NodeFields::default()
        }))
    }
}

impl LinkRule {
    /// The rule keeping one port of `source` linked to one port of `sink`.
    pub fn exact(source: &Node, output: &Port, sink: &Node, input: &Port) -> Self {
        let port = |port: &Port| Pattern::new(&escape_glob(&port.name)).expect("escaped glob");
        Self {
            from: NodeMatcher::exact_name(&source.name),
            to: NodeMatcher::exact_name(&sink.name),
            ports: vec![PortMapping(port(output), port(input))],
            exclusive: false,
        }
    }
}

fn escape_glob(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '*' | '?' | '[' | ']' | '{' | '}' | '\\' => vec!['[', character, ']'],
            _ => vec![character],
        })
        .collect()
}

/// Config and GUI state resolved together. GUI state wins over config so that
/// edits show up immediately.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub config: Config,
    pub state: State,
    pub ui: Ui,
}

impl Settings {
    /// Links from the config, then the ones made by hand in the GUI.
    pub fn link_rules(&self) -> impl Iterator<Item = &LinkRule> {
        self.config.links.iter().chain(&self.state.links)
    }

    fn config_rules<'a>(&'a self, node: &'a Node) -> impl Iterator<Item = &'a NodeRule> {
        let config_alias = self.config_alias(node);
        self.config
            .nodes
            .iter()
            .filter(move |rule| rule.matcher.matches(node, config_alias))
    }

    fn config_alias<'a>(&'a self, node: &Node) -> Option<&'a str> {
        self.config
            .nodes
            .iter()
            .find(|rule| rule.alias.is_some() && rule.matcher.matches(node, None))
            .and_then(|rule| rule.alias.as_deref())
    }

    pub fn alias<'a>(&'a self, node: &Node) -> Option<&'a str> {
        self.state
            .aliases
            .get(&node.name)
            .map(String::as_str)
            .or_else(|| self.config_alias(node))
    }

    pub fn display_name<'a>(&'a self, node: &'a Node) -> &'a str {
        self.alias(node).unwrap_or_else(|| node.label())
    }

    pub fn is_hidden(&self, node: &Node) -> bool {
        self.state.hidden.contains(&node.name)
            || self
                .config_rules(node)
                .find_map(|rule| rule.hidden)
                .unwrap_or(false)
    }

    pub fn position(&self, node: &Node) -> Option<Position> {
        self.ui
            .positions
            .get(&node.name)
            .copied()
            .or_else(|| self.config_rules(node).find_map(|rule| rule.position))
    }

    /// The volume and mute the config wants applied when this node appears.
    pub fn initial_volume(&self, node: &Node) -> (Option<f32>, Option<bool>) {
        let volume = self.config_rules(node).find_map(|rule| rule.volume);
        let mute = self.config_rules(node).find_map(|rule| rule.mute);
        (volume, mute)
    }

    pub fn matches(&self, matcher: &NodeMatcher, node: &Node) -> bool {
        matcher.matches(node, self.alias(node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
        [[node]]
        match = "alsa_output.usb-Focusrite*"
        alias = "Monitors"
        volume = 0.8

        [[node]]
        match = { application = "spotify", media-class = "Stream/Output/Audio" }
        alias = "Music"

        [[link]]
        from = "Music"
        to = "Monitors"
        exclusive = true

        [[link]]
        from = "Firefox"
        to = { name = "alsa_output.*" }
        ports = [["output_FL", "playback_FR"]]
    "#;

    #[test]
    fn parses_example_config() {
        let config = Config::parse(EXAMPLE, Path::new("test.toml")).expect("example parses");
        assert_eq!(config.nodes.len(), 2);
        assert_eq!(config.links.len(), 2);
        assert!(config.links[0].exclusive);
        assert_eq!(config.links[1].ports.len(), 1);
    }

    #[test]
    fn rejects_unknown_keys() {
        let error = Config::parse(
            "[[node]]\nmatch = \"x\"\nalais = \"typo\"",
            Path::new("bad.toml"),
        );
        assert!(error.is_err());
    }

    #[test]
    fn rejects_invalid_glob() {
        assert!(
            Config::parse("[[link]]\nfrom = \"[\"\nto = \"x\"", Path::new("bad.toml")).is_err()
        );
    }

    #[test]
    fn state_export_round_trips() {
        let mut state = State::default();
        state.aliases.insert("weird*name".into(), "Nice".into());
        state.hidden.insert("noise".into());
        let text = toml::to_string_pretty(&state.to_config()).expect("serializes");
        let config = Config::parse(&text, Path::new("export.toml")).expect("export parses back");
        let node = Node {
            id: 1,
            name: "weird*name".into(),
            ..Node::default()
        };
        let other = Node {
            name: "weirdXname".into(),
            ..node.clone()
        };
        let settings = Settings {
            config,
            ..Settings::default()
        };
        assert_eq!(settings.alias(&node), Some("Nice"));
        assert_eq!(settings.alias(&other), None);
    }

    #[test]
    fn layout_moves_out_of_an_old_state_file() {
        let directory =
            std::env::temp_dir().join(format!("tuyau-migration-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let state_path = directory.join("state.toml");
        std::fs::write(
            &state_path,
            "hidden = [\"noise\"]\nauto-arrange = true\n\n[positions]\nspeakers = { x = 1.0, y = 2.0 }\n",
        )
        .expect("writes old state");

        let state = State::load(&state_path).expect("old state still loads");
        let ui = Ui::load(&Ui::path_for(&state_path), &state_path).expect("layout migrates");
        std::fs::remove_dir_all(&directory).ok();

        assert!(state.hidden.contains("noise"));
        assert!(ui.auto_arrange);
        assert_eq!(ui.positions["speakers"], Position { x: 1.0, y: 2.0 });
    }
}
