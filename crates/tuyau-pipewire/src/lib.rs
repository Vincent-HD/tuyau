//! Runs a PipeWire client on its own thread, publishing graph snapshots and
//! accepting commands from any thread.

mod params;
mod reconciler;
mod thread;

use std::{sync::Arc, thread::JoinHandle, time::Duration};

use anyhow::{Context, anyhow, bail};
use pipewire as pw;
use tuyau_core::{Graph, PortPair, config::DeviceRule};

pub use reconciler::{Plan, Reconciler};

#[derive(Debug, Clone)]
pub enum Command {
    Link(PortPair),
    Unlink(u32),
    /// Creates a virtual device that outlives this client.
    CreateDevice(DeviceRule),
    /// Removes any global object, e.g. a device tuyau created.
    Destroy(u32),
    /// Linear channel volumes.
    SetVolume {
        node_id: u32,
        channels: Vec<f32>,
    },
    SetMute {
        node_id: u32,
        mute: bool,
    },
    /// Answered with [`Event::Synced`] once every earlier command was handled.
    Sync(u64),
    Quit,
}

#[derive(Debug, Clone)]
pub enum Event {
    Graph(Arc<Graph>),
    Synced(u64),
    Error(String),
    Disconnected,
}

pub struct Connection {
    commands: pw::channel::Sender<Command>,
    events: async_channel::Receiver<Event>,
    thread: Option<JoinHandle<()>>,
    next_sync: u64,
}

impl Connection {
    pub fn spawn() -> anyhow::Result<Self> {
        let (commands, command_receiver) = pw::channel::channel();
        let (event_sender, events) = async_channel::unbounded();
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || thread::run(command_receiver, event_sender, ready_sender))
            .context("cannot spawn the PipeWire thread")?;
        ready_receiver
            .recv()
            .map_err(|_| anyhow!("the PipeWire thread exited during startup"))?
            .map_err(|error| anyhow!("cannot connect to PipeWire: {error}"))?;
        Ok(Self {
            commands,
            events,
            thread: Some(thread),
            next_sync: 0,
        })
    }

    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::warn!("PipeWire thread is gone; command dropped");
        }
    }

    pub fn commands(&self) -> CommandSender {
        CommandSender(self.commands.clone())
    }

    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }

    /// Blocks until every command sent so far was handled, returning the graph at that point.
    pub fn roundtrip(&mut self, timeout: Duration) -> anyhow::Result<Arc<Graph>> {
        self.next_sync += 1;
        let token = self.next_sync;
        self.send(Command::Sync(token));
        let deadline = std::time::Instant::now() + timeout;
        let mut graph = None;
        loop {
            if std::time::Instant::now() > deadline {
                bail!("PipeWire did not answer within {timeout:?}");
            }
            match self.events.recv_blocking() {
                Ok(Event::Graph(snapshot)) => graph = Some(snapshot),
                Ok(Event::Synced(done)) if done == token => {
                    return graph.ok_or_else(|| anyhow!("PipeWire sent no graph"));
                }
                Ok(Event::Synced(_)) => {}
                Ok(Event::Error(message)) => log::warn!("PipeWire: {message}"),
                Ok(Event::Disconnected) | Err(_) => bail!("disconnected from PipeWire"),
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A cheap handle for sending commands from other threads or UI callbacks.
#[derive(Clone)]
pub struct CommandSender(pw::channel::Sender<Command>);

impl CommandSender {
    pub fn send(&self, command: Command) {
        if self.0.send(command).is_err() {
            log::warn!("PipeWire thread is gone; command dropped");
        }
    }
}
