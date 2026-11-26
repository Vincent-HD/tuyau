use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use tuyau_core::{
    Graph, PortPair, Settings,
    reconcile::{DevicePlan, LinkPlan, initial_volume, plan_devices, plan_links},
};

use crate::{Command, CommandSender};

/// How long to wait before asking again for a change that has not shown up yet.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// What one reconcile pass asked for.
#[derive(Debug, Default)]
pub struct Plan {
    pub links: LinkPlan,
    pub devices: DevicePlan,
}

/// Keeps the graph in line with the config: creates the declared devices and
/// links whenever they are missing (auto-reconnect) and applies volumes once per
/// new node.
#[derive(Default)]
pub struct Reconciler {
    requested_links: HashMap<PortPair, Instant>,
    requested_unlinks: HashMap<u32, Instant>,
    requested_devices: HashMap<String, Instant>,
    requested_removals: HashMap<u32, Instant>,
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
    ) -> Plan {
        let now = Instant::now();
        let recent = |at: &Instant| now.duration_since(*at) < RETRY_AFTER;
        self.requested_links.retain(|_, at| recent(at));
        self.requested_unlinks.retain(|_, at| recent(at));
        self.requested_devices.retain(|_, at| recent(at));
        self.requested_removals.retain(|_, at| recent(at));
        self.initialized_nodes
            .retain(|id| graph.nodes.contains_key(id));

        let mut devices = plan_devices(graph, settings);
        devices
            .create
            .retain(|rule| !self.requested_devices.contains_key(&rule.name));
        devices
            .destroy
            .retain(|id| !self.requested_removals.contains_key(id));
        for rule in &devices.create {
            self.requested_devices.insert(rule.name.clone(), now);
            commands.send(Command::CreateDevice(rule.clone()));
        }
        for id in &devices.destroy {
            self.requested_removals.insert(*id, now);
            commands.send(Command::Destroy(*id));
        }

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
        Plan {
            links: plan,
            devices,
        }
    }
}
