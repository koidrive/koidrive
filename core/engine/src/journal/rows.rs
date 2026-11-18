use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use super::{Field, HeldRow, IntentId, NodeId, Row, RowKey, RowState, Value};
use crate::tracked::TrackedMap;

/// The journal's rows, full and compact, with the keys of the rows made over each intent and of the place rows
/// under each parent.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Rows {
    full: TrackedMap<RowKey, Row>,
    compact: BTreeMap<RowKey, HeldRow>,
    full_over: BTreeSet<(IntentId, RowKey)>,
    compact_over: BTreeSet<(IntentId, RowKey)>,
    under: BTreeSet<(NodeId, RowKey)>,
}

#[cfg(test)]
thread_local! {
    pub(super) static VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn visit<T>(_: &T) {
    #[cfg(test)]
    VISITED.with(|visited| visited.set(visited.get() + 1));
}

fn bases(row: &Row) -> impl Iterator<Item = IntentId> {
    let flight = match &row.state {
        RowState::Local {
            flight: Some(flight),
        } => flight.base,
        _ => None,
    };
    row.base.into_iter().chain(flight)
}

fn parent(row: &Row) -> Option<&NodeId> {
    match &row.value {
        Value::Place(place) => Some(&place.parent),
        _ => None,
    }
}

fn index(
    full_over: &mut BTreeSet<(IntentId, RowKey)>,
    under: &mut BTreeSet<(NodeId, RowKey)>,
    key: &RowKey,
    row: &Row,
) {
    full_over.extend(bases(row).map(|base| (base, key.clone())));
    under.extend(parent(row).map(|parent| (parent.clone(), key.clone())));
}

fn keys_of<T: Ord + Clone>(index: &BTreeSet<(T, RowKey)>, of: T) -> Vec<RowKey> {
    let first = (of.clone(), (NodeId::new(), Field::Place));
    (index.range((Bound::Included(&first), Bound::Unbounded)))
        .take_while(|(at, _)| *at == of)
        .map(|(_, key)| key.clone())
        .collect()
}

impl Rows {
    pub(super) fn restore(full: BTreeMap<RowKey, Row>) -> Self {
        let mut rows = Self::default();
        for (key, row) in &full {
            index(&mut rows.full_over, &mut rows.under, key, row);
        }
        Self {
            full: full.into(),
            ..rows
        }
    }

    pub(super) fn get(&self, key: &RowKey) -> Option<&Row> {
        self.full.get(key)
    }

    pub(super) fn get_compact(&self, key: &RowKey) -> Option<&HeldRow> {
        self.compact.get(key)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&RowKey, &Row)> {
        self.full.iter().inspect(visit)
    }

    pub(super) fn iter_compact(&self) -> impl Iterator<Item = (&RowKey, &HeldRow)> {
        self.compact.iter().inspect(visit)
    }

    /// The keys of the full rows whose base, or whose flight's base, is `from`.
    pub(super) fn full_over(&self, from: IntentId) -> Vec<RowKey> {
        keys_of(&self.full_over, from)
    }

    /// The keys of the compact rows whose base is `from`.
    pub(super) fn compact_over(&self, from: IntentId) -> Vec<RowKey> {
        keys_of(&self.compact_over, from)
    }

    /// The keys of the full place rows that put a node under `parent`.
    pub(super) fn under(&self, parent: &str) -> Vec<RowKey> {
        keys_of(&self.under, parent.to_owned())
    }

    pub(super) fn insert(&mut self, key: RowKey, row: Row) {
        self.unindex(&key);
        index(&mut self.full_over, &mut self.under, &key, &row);
        self.full.insert(key, row);
    }

    pub(super) fn insert_quietly(&mut self, key: RowKey, row: Row) {
        self.unindex(&key);
        index(&mut self.full_over, &mut self.under, &key, &row);
        self.full.insert_quietly(key, row);
    }

    pub(super) fn remove(&mut self, key: &RowKey) -> Option<Row> {
        self.unindex(key);
        self.full.remove(key)
    }

    pub(super) fn hand_off(&mut self, key: &RowKey) -> Option<(Row, bool)> {
        self.unindex(key);
        self.full.hand_off(key)
    }

    /// Changes the full row of `key` in place, as [`TrackedMap::get_mut`] does.
    pub(super) fn update<T>(
        &mut self,
        key: &RowKey,
        change: impl FnOnce(&mut Row) -> T,
    ) -> Option<T> {
        self.unindex(key);
        let row = self.full.get_mut(key)?;
        let changed = change(row);
        index(&mut self.full_over, &mut self.under, key, row);
        Some(changed)
    }

    pub(super) fn insert_compact(&mut self, key: RowKey, held: HeldRow) {
        self.remove_compact(&key);
        self.compact_over
            .extend(held.base.map(|base| (base, key.clone())));
        self.compact.insert(key, held);
    }

    pub(super) fn remove_compact(&mut self, key: &RowKey) -> Option<HeldRow> {
        let held = self.compact.remove(key)?;
        if let Some(base) = held.base {
            self.compact_over.remove(&(base, key.clone()));
        }
        Some(held)
    }

    pub(super) fn update_compact<T>(
        &mut self,
        key: &RowKey,
        change: impl FnOnce(&mut HeldRow) -> T,
    ) -> Option<T> {
        let held = self.compact.get_mut(key)?;
        if let Some(base) = held.base {
            self.compact_over.remove(&(base, key.clone()));
        }
        let changed = change(held);
        self.compact_over
            .extend(held.base.map(|base| (base, key.clone())));
        Some(changed)
    }

    pub(super) fn take_changes(&mut self) -> Vec<(RowKey, Option<Row>)> {
        self.full.take_changes()
    }

    fn unindex(&mut self, key: &RowKey) {
        let Some(row) = self.full.get(key) else {
            return;
        };
        for base in bases(row) {
            self.full_over.remove(&(base, key.clone()));
        }
        if let Some(parent) = parent(row) {
            self.under.remove(&(parent.clone(), key.clone()));
        }
    }
}
