//! Undo and redo for the edits made in the editor.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use tuyau_core::{DeviceRule, PortPair, Position};

const LIMIT: usize = 200;
/// Volume changes on one node closer together than this are one step, so a
/// slider drag or a few scroll notches undo at once.
const MERGE_WINDOW: Duration = Duration::from_millis(800);

#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    Link(PortPair),
    Unlink(PortPair),
    Volume {
        node_id: u32,
        before: Vec<f32>,
        after: Vec<f32>,
    },
    Mute {
        node_id: u32,
        mute: bool,
    },
    /// Cards moved together, keyed by node name.
    Move(Vec<Moved>),
    Alias {
        name: String,
        before: Option<String>,
        after: Option<String>,
    },
    Hidden {
        names: BTreeSet<String>,
        hidden: bool,
    },
    Layout {
        before: Layout,
        after: Layout,
    },
    /// A virtual device declared in the state file, or removed from it.
    Device {
        rule: DeviceRule,
        present: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Moved {
    pub name: String,
    pub before: Position,
    pub after: Position,
}

/// Every node position: the saved ones, keyed by node name, and the shown ones.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub saved: BTreeMap<String, Position>,
    pub shown: BTreeMap<u32, Position>,
}

impl Edit {
    /// The edit that reverts this one.
    pub fn inverse(&self) -> Self {
        match self.clone() {
            Self::Link(pair) => Self::Unlink(pair),
            Self::Unlink(pair) => Self::Link(pair),
            Self::Volume {
                node_id,
                before,
                after,
            } => Self::Volume {
                node_id,
                before: after,
                after: before,
            },
            Self::Mute { node_id, mute } => Self::Mute {
                node_id,
                mute: !mute,
            },
            Self::Move(moves) => Self::Move(
                moves
                    .into_iter()
                    .map(|moved| Moved {
                        before: moved.after,
                        after: moved.before,
                        ..moved
                    })
                    .collect(),
            ),
            Self::Alias {
                name,
                before,
                after,
            } => Self::Alias {
                name,
                before: after,
                after: before,
            },
            Self::Hidden { names, hidden } => Self::Hidden {
                names,
                hidden: !hidden,
            },
            Self::Layout { before, after } => Self::Layout {
                before: after,
                after: before,
            },
            Self::Device { rule, present } => Self::Device {
                rule,
                present: !present,
            },
        }
    }
}

#[derive(Default)]
pub struct History {
    done: Vec<Edit>,
    undone: Vec<Edit>,
    last_record: Option<Instant>,
}

impl History {
    pub fn record(&mut self, edit: Edit) {
        self.record_at(edit, Instant::now());
    }

    fn record_at(&mut self, edit: Edit, now: Instant) {
        self.undone.clear();
        let recent = self
            .last_record
            .replace(now)
            .is_some_and(|last| now.duration_since(last) < MERGE_WINDOW);
        if let (
            true,
            Some(Edit::Volume {
                node_id: previous,
                after: previous_after,
                ..
            }),
            Edit::Volume { node_id, after, .. },
        ) = (recent, self.done.last_mut(), &edit)
            && previous == node_id
        {
            *previous_after = after.clone();
            return;
        }
        self.done.push(edit);
        if self.done.len() > LIMIT {
            self.done.remove(0);
        }
    }

    /// The edit to apply to undo the latest one.
    pub fn undo(&mut self) -> Option<Edit> {
        let edit = self.done.pop()?;
        let inverse = edit.inverse();
        self.undone.push(edit);
        self.last_record = None;
        Some(inverse)
    }

    /// The edit to apply to redo the latest undone one.
    pub fn redo(&mut self) -> Option<Edit> {
        let edit = self.undone.pop()?;
        self.done.push(edit.clone());
        self.last_record = None;
        Some(edit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(node_id: u32, before: f32, after: f32) -> Edit {
        Edit::Volume {
            node_id,
            before: vec![before],
            after: vec![after],
        }
    }

    #[test]
    fn undo_then_redo_round_trips() {
        let mut history = History::default();
        let pair = PortPair {
            output_node: 1,
            output_port: 2,
            input_node: 3,
            input_port: 4,
        };
        history.record(Edit::Link(pair));
        assert_eq!(history.undo(), Some(Edit::Unlink(pair)));
        assert_eq!(history.undo(), None);
        assert_eq!(history.redo(), Some(Edit::Link(pair)));
        assert_eq!(history.redo(), None);
    }

    #[test]
    fn a_group_move_undoes_in_one_step() {
        let moved = |name: &str, before: f32, after: f32| Moved {
            name: name.to_owned(),
            before: Position { x: before, y: 0.0 },
            after: Position { x: after, y: 0.0 },
        };
        let mut history = History::default();
        history.record(Edit::Move(vec![
            moved("a", 0.0, 10.0),
            moved("b", 5.0, 15.0),
        ]));
        assert_eq!(
            history.undo(),
            Some(Edit::Move(vec![
                moved("a", 10.0, 0.0),
                moved("b", 15.0, 5.0)
            ]))
        );
        assert_eq!(history.undo(), None);
    }

    #[test]
    fn new_edits_drop_the_redo_branch() {
        let mut history = History::default();
        history.record(Edit::Mute {
            node_id: 1,
            mute: true,
        });
        history.undo();
        history.record(Edit::Mute {
            node_id: 2,
            mute: true,
        });
        assert_eq!(history.redo(), None);
    }

    #[test]
    fn quick_volume_changes_merge() {
        let mut history = History::default();
        let start = Instant::now();
        history.record_at(volume(1, 0.2, 0.3), start);
        history.record_at(volume(1, 0.3, 0.5), start + Duration::from_millis(100));
        history.record_at(volume(1, 0.5, 0.6), start + Duration::from_secs(5));
        assert_eq!(history.undo(), Some(volume(1, 0.6, 0.5)));
        assert_eq!(history.undo(), Some(volume(1, 0.5, 0.2)));
    }
}
