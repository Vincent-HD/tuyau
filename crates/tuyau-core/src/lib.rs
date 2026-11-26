//! PipeWire graph model, declarative config and the pure logic shared by the CLI and GUI.

pub mod config;
pub mod graph;
pub mod layout;
pub mod reconcile;

pub use config::{
    Config, ConfigError, LinkRule, NodeMatcher, NodeRule, Pattern, Position, Settings, State, Ui,
    View,
};
pub use graph::{Direction, Graph, Link, MediaKind, Node, NodeState, Port, PortPair, Volume};
