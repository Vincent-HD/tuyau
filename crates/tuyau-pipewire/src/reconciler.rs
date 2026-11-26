use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use tuyau_core::{
    Graph, PortPair, Settings,
    reconcile::{LinkPlan, initial_volume, plan_links},
};

use crate::{Command, CommandSender};

/// How long to wait before asking again for a change that has not shown up yet.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// Keeps the graph in line with the config: creates the declared links whenever
/// their nodes appear (auto-reconnect) and applies volumes once per new node.
#[derive(Default)]
pub struct Reconciler {
    requested_links: HashMap<PortPair, Instant>,
    requested_unlinks: HashMap<u32, Instant>,
    initialized_nodes: HashSet<u32>,
}

impl Reconciler {
    /// Forget which nodes already got their initial volume, so it applies again.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn reconcile(
        &mut self,
        graph: &Graph,
        settings: &Settings,
        commands: &CommandSender,
    ) -> LinkPlan {
        let now = Instant::now();
        let recent = |at: &Instant| now.duration_since(*at) < RETRY_AFTER;
        self.requested_links.retain(|_, at| recent(at));
        self.requested_unlinks.retain(|_, at| recent(at));
        self.initialized_nodes
            .retain(|id| graph.nodes.contains_key(id));

        let mut plan = plan_links(graph, settings);
        plan.create
            .retain(|pair| !self.requested_links.contains_key(pair));
        plan.destroy
            .retain(|id| !self.requested_unlinks.contains_key(id));
        for pair in &plan.create {
            self.requested_links.insert(*pair, now);
            commands.send(Command::Link(*pair));
        }
        for id in &plan.destroy {
            self.requested_unlinks.insert(*id, now);
            commands.send(Command::Unlink(*id));
        }

        for node in graph.nodes.values() {
            if node.volume.is_none() || !self.initialized_nodes.insert(node.id) {
                continue;
            }
            let Some(change) = initial_volume(node, settings) else {
                continue;
            };
            if let Some(channels) = change.channels {
                commands.send(Command::SetVolume {
                    node_id: change.node_id,
                    channels,
                });
            }
            if let Some(mute) = change.mute {
                commands.send(Command::SetMute {
                    node_id: change.node_id,
                    mute,
                });
            }
        }
        plan
    }
}
