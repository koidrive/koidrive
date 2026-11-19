use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;

use super::{IntentId, NodeId, Seen};
use crate::state::MapChanges;
use crate::tracked::TrackedMap;

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct SeenLists {
    lists: TrackedMap<NodeId, Vec<Seen>>,
    holding: BTreeMap<IntentId, BTreeSet<NodeId>>,
}

impl SeenLists {
    pub fn insert(&mut self, node: NodeId, entries: Vec<Seen>) {
        self.unindex(&node);
        for entry in &entries {
            self.holding
                .entry(entry.intent)
                .or_default()
                .insert(node.clone());
        }
        self.lists.insert(node, entries);
    }

    pub fn remove(&mut self, node: &str) {
        self.unindex(node);
        self.lists.remove(node);
    }

    pub fn holding(&self, intent: IntentId) -> &BTreeSet<NodeId> {
        static NONE: BTreeSet<NodeId> = BTreeSet::new();
        self.holding.get(&intent).unwrap_or(&NONE)
    }

    pub fn take_changes(&mut self) -> MapChanges<NodeId, Vec<Seen>> {
        self.lists.take_changes()
    }

    fn unindex(&mut self, node: &str) {
        let Some(entries) = self.lists.get(node) else {
            return;
        };
        for entry in entries {
            if let Entry::Occupied(mut holders) = self.holding.entry(entry.intent) {
                holders.get_mut().remove(node);
                if holders.get().is_empty() {
                    holders.remove();
                }
            }
        }
    }
}

impl Deref for SeenLists {
    type Target = BTreeMap<NodeId, Vec<Seen>>;

    fn deref(&self) -> &Self::Target {
        &self.lists
    }
}

impl From<BTreeMap<NodeId, Vec<Seen>>> for SeenLists {
    fn from(lists: BTreeMap<NodeId, Vec<Seen>>) -> Self {
        let mut holding: BTreeMap<IntentId, BTreeSet<NodeId>> = BTreeMap::new();
        for (node, entries) in &lists {
            for entry in entries {
                holding
                    .entry(entry.intent)
                    .or_default()
                    .insert(node.clone());
            }
        }
        Self {
            lists: lists.into(),
            holding,
        }
    }
}
