// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Trees;
use crate::ids::{IdGen, unix_ms};
use crate::question::{Answer, Held, Limit, Question, QuestionKind, Revisions};
use crate::state::GuardChanges;
use crate::tracked::{Marked, TrackedMap, TrackedSet};
use crate::tree::{Kind, NodeId};

pub const WINDOW_MS: u64 = 10 * 60 * 1000;

/// The most deletions the window takes without asking.
pub const MAX_DELETIONS: u64 = 1000;

pub const MIN_SPACE_FILES: u64 = 20;

/// What the window counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub deletions: u64,
    /// The files the deletions remove.
    pub affected_files: u64,
    /// The files of the space before the deletions.
    pub space_files: u64,
}

impl Window {
    /// The limits these counts cross.
    pub fn exceeded(&self) -> Vec<Limit> {
        let mut exceeded = Vec::new();
        if self.deletions > MAX_DELETIONS {
            exceeded.push(Limit::Deletions);
        }
        if self.space_files >= MIN_SPACE_FILES && self.affected_files * 2 > self.space_files {
            exceeded.push(Limit::Share);
        }
        exceeded
    }
}

/// A deletion the window counts: when it was first planned, and the files it removed when it was
/// last planned. For another device's deletion, when this device last removed some of it, and the
/// files it removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counted {
    pub at_ms: u64,
    pub files: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Checked {
    pub held: BTreeSet<NodeId>,
    pub restored: BTreeMap<NodeId, Held>,
}

/// What the guard keeps between runs besides its question, which is one of the open questions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardState {
    /// The deletions that count now, by topmost node. A deletion leaves when its window ends, when
    /// L holds its node again, and when the person answers a question about it.
    pub ledger: BTreeMap<NodeId, Counted>,
    /// Deletions the person confirmed, planned without counting until they are acknowledged.
    pub confirmed: BTreeMap<NodeId, Held>,
    /// The limits the questions of the confirmed deletions crossed. A question that holds one of
    /// them again after it changed names these limits, also when the window crosses none.
    pub confirmed_limits: BTreeSet<Limit>,
    /// The removals that applied other devices' deletions and count now, by deletion. A deletion
    /// leaves when the server no longer holds it, and when this device removed all of it and its
    /// last removal is older than the window, so a removal that stops and goes on later counts
    /// whole. None leaves by age while a notice about them is open.
    pub remote: BTreeMap<RemoteDeletion, Counted>,
    /// The deletions a dismissed notice counted. Later removals of one of them are not counted
    /// again. A deletion leaves when the server no longer holds it, or when this device removed
    /// all of it.
    pub dismissed: BTreeSet<RemoteDeletion>,
}

/// Another device's deletion: one trashing on the server, and its topmost node, the outermost node
/// it trashed that L still held when this device removed it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemoteDeletion {
    pub top: NodeId,
    pub trash: Trash,
}

/// Which trashing a deletion is, as far as R tells.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trash {
    /// R holds the tombstones: the trash group the server gave the delete, and the seq of its
    /// commit, R's `meta_rev` of each node it trashed. A node trashed earlier keeps its group
    /// when a folder above it is deleted, and a node deleted again after a restore gets a new one.
    Group { group: String, seq: u64 },
    /// R holds no record of the removed node, as after a bootstrap that did not list the trash:
    /// the server trashed it at this seq or before, the seq up to which R holds every change.
    Unknown { no_later_than: u64 },
}

impl RemoteDeletion {
    fn of(trees: &Trees, within: &[NodeId], last_seq: u64) -> Self {
        let trash = |id: &NodeId| {
            (trees.remote().get(id).filter(|node| node.trashed)).map(|node| Trash::Group {
                group: node.trash_group.clone(),
                seq: node.meta_rev,
            })
        };
        let own = trash(&within[0]);
        let top = (within.iter()).take_while(|id| trash(id) == own).last();
        RemoteDeletion {
            top: top.expect("the removed node").clone(),
            trash: own.unwrap_or(Trash::Unknown {
                no_later_than: last_seq,
            }),
        }
    }

    fn takes(&self, removed: &RemoteDeletion, within: &[NodeId]) -> bool {
        match (&self.trash, &removed.trash) {
            (Trash::Group { group: a, .. }, Trash::Group { group: b, .. }) => a == b,
            (Trash::Unknown { no_later_than }, Trash::Group { seq, .. }) => {
                seq <= no_later_than && within.contains(&self.top)
            }
            (_, Trash::Unknown { .. }) => within.contains(&self.top),
        }
    }

    fn undone(&self, trees: &Trees) -> bool {
        let Some(node) = trees.remote().get(&self.top) else {
            return false;
        };
        match &self.trash {
            Trash::Group { group, .. } => !trees.trashes_group(group),
            Trash::Unknown { no_later_than } => !node.trashed || node.meta_rev > *no_later_than,
        }
    }

    fn removed(&self, trees: &Trees) -> bool {
        let top = trees.remote().get(&self.top);
        let (group, ours) = match &self.trash {
            Trash::Group { group, .. } => (
                trees.holds_trash_group(group),
                top.is_none_or(|node| node.trashed && node.trash_group == *group),
            ),
            Trash::Unknown { .. } => (false, top.is_none_or(|node| node.trashed)),
        };
        (!ours || trees.local().get(&self.top).is_none()) && !group
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRemoval {
    pub within: Vec<NodeId>,
    pub files: Vec<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Guard {
    ledger: TrackedMap<NodeId, Counted>,
    confirmed: TrackedMap<NodeId, Held>,
    confirmed_limits: Marked<BTreeSet<Limit>>,
    remote: TrackedMap<RemoteDeletion, Counted>,
    dismissed: TrackedSet<RemoteDeletion>,
}

impl From<GuardState> for Guard {
    fn from(state: GuardState) -> Self {
        let GuardState {
            ledger,
            confirmed,
            confirmed_limits,
            remote,
            dismissed,
        } = state;
        Self {
            ledger: ledger.into(),
            confirmed: confirmed.into(),
            confirmed_limits: confirmed_limits.into(),
            remote: remote.into(),
            dismissed: dismissed.into(),
        }
    }
}

impl Guard {
    pub(crate) fn state(&self) -> GuardState {
        GuardState {
            ledger: self.ledger.to_map(),
            confirmed: self.confirmed.to_map(),
            confirmed_limits: (*self.confirmed_limits).clone(),
            remote: self.remote.to_map(),
            dismissed: self.dismissed.to_set(),
        }
    }

    pub(crate) fn take_changes(&mut self) -> GuardChanges {
        let Self {
            ledger,
            confirmed,
            confirmed_limits,
            remote,
            dismissed,
        } = self;
        GuardChanges {
            ledger: ledger.take_changes(),
            confirmed: confirmed.take_changes(),
            confirmed_limits: confirmed_limits.take_changed().cloned(),
            remote: remote.take_changes(),
            dismissed: dismissed.take_changes(),
        }
    }

    pub(crate) fn check(
        &mut self,
        asked: &mut Option<Question>,
        pending: BTreeMap<NodeId, Held>,
        trees: &Trees,
        given_up_files: u64,
        ids: &mut IdGen,
        now_ms: u64,
    ) -> Checked {
        self.dismissed
            .retain(|deletion| !deletion.undone(trees) && !deletion.removed(trees));
        self.remote.retain(|deletion, _| !deletion.undone(trees));
        not_after(&mut self.ledger, now_ms);
        self.ledger.retain(|id, counted| {
            counted.at_ms + WINDOW_MS > now_ms && trees.local().get(id).is_none()
        });
        let mut unconfirmed = BTreeSet::new();
        self.confirmed.retain(|id, held| {
            let kept = pending.get(id).is_some_and(|left| {
                (left.members.iter())
                    .all(|(node, revisions)| held.members.get(node) == Some(revisions))
            });
            if !kept {
                unconfirmed.extend(held.members.keys().cloned());
            }
            kept
        });
        let mut open: BTreeMap<NodeId, Held> = (pending.into_iter())
            .filter(|(id, _)| !self.confirmed.contains_key(id))
            .collect();
        self.count(&open, trees, now_ms);
        let mut carried = match asked {
            Some(Question {
                kind: QuestionKind::MassDelete { exceeded, .. },
                ..
            }) => exceeded.clone(),
            _ => Vec::new(),
        };
        if !unconfirmed.is_empty() {
            carried.extend(self.confirmed_limits.iter());
        }
        if self.confirmed.is_empty() {
            self.confirmed_limits.set(BTreeSet::new());
        }
        let mut checked = Checked::default();
        let was_asked = match asked.take() {
            Some(question) if question.held == open && question.answer.is_none() => {
                checked.held = holds(&open);
                *asked = Some(question);
                return checked;
            }
            Some(question) if question.held == open && question.answer == Some(Answer::Delete) => {
                self.ledger.retain(|top, _| !open.contains_key(top));
                self.confirmed.extend(open);
                self.confirmed_limits.get_mut().extend(carried);
                return checked;
            }
            Some(question) if question.answer == Some(Answer::Restore) => {
                let answered: BTreeSet<&NodeId> = question.holds().collect();
                let (restored, left): (TrackedSet<NodeId>, TrackedSet<NodeId>) = (open.values())
                    .flat_map(|held| held.members.keys().cloned())
                    .partition(|node| answered.contains(node));
                self.ledger.retain(|top, _| !open.contains_key(top));
                checked.restored = deletions(trees, &restored);
                open = deletions(trees, &left);
                self.count(&open, trees, now_ms);
                true
            }
            asked => asked.is_some(),
        };
        let held_before = was_asked
            || (open.values())
                .any(|held| held.members.keys().any(|node| unconfirmed.contains(node)));
        let window = self.window(&open, trees, given_up_files);
        let mut exceeded = window.exceeded();
        if open.is_empty() || (!held_before && exceeded.is_empty()) {
            return checked;
        }
        exceeded.extend(carried);
        exceeded.sort();
        exceeded.dedup();
        checked.held = holds(&open);
        *asked = Some(Question {
            id: ids.uuid_v7(now_ms),
            asked_at_ms: now_ms,
            kind: QuestionKind::MassDelete {
                deletions: window.deletions,
                affected_files: window.affected_files,
                space_files: window.space_files,
                exceeded,
            },
            held: open,
            answer: None,
            shown: BTreeMap::new(),
            frozen: None,
        });
        checked
    }

    pub(crate) fn confirm(&mut self, held: BTreeMap<NodeId, Held>) {
        self.ledger.retain(|top, _| !held.contains_key(top));
        self.confirmed.extend(held);
    }

    fn count(&mut self, open: &BTreeMap<NodeId, Held>, trees: &Trees, now_ms: u64) {
        for (id, held) in open {
            let at_ms = self.ledger.get(id).map_or(now_ms, |counted| counted.at_ms);
            let files = (held.members.keys())
                .filter(|node| !made_in_window(node, now_ms))
                .filter(|node| (trees.synced().live(node)).is_some_and(|s| s.kind == Kind::File))
                .count() as u64;
            self.ledger.insert(id.clone(), Counted { at_ms, files });
        }
    }

    fn window(&self, open: &BTreeMap<NodeId, Held>, trees: &Trees, given_up_files: u64) -> Window {
        let gone: u64 = (self.ledger.iter())
            .filter(|(id, _)| trees.synced().live(id).is_none())
            .map(|(_, counted)| counted.files)
            .sum();
        let affected_files = (self.ledger.iter())
            .map(|(id, counted)| open.get(id).map_or(counted.files, |held| held.files))
            .sum();
        Window {
            deletions: self.ledger.len() as u64,
            affected_files,
            space_files: trees.synced_files() - given_up_files + gone,
        }
    }

    pub(crate) fn count_remote(
        &mut self,
        removal: RemoteRemoval,
        last_seq: u64,
        noticed: bool,
        trees: &Trees,
        given_up_files: u64,
        now_ms: u64,
    ) -> Option<Window> {
        let removed = RemoteDeletion::of(trees, &removal.within, last_seq);
        let takes = |deletion: &RemoteDeletion| deletion.takes(&removed, &removal.within);
        let dismissed = self.dismissed.iter().any(takes);
        self.dismissed
            .retain(|deletion| !deletion.undone(trees) && !deletion.removed(trees));
        if dismissed {
            return None;
        }
        not_after(&mut self.remote, now_ms);
        self.remote.retain(|deletion, _| !deletion.undone(trees));
        let held = self.remote.keys().find(|deletion| takes(deletion)).cloned();
        if !noticed {
            self.remote.retain(|deletion, counted| {
                Some(deletion) == held.as_ref()
                    || !deletion.removed(trees)
                    || counted.at_ms + WINDOW_MS > now_ms
            });
        }
        let counted = (self.remote).get_or_insert_with(held.unwrap_or(removed), || Counted {
            at_ms: now_ms,
            files: 0,
        });
        counted.at_ms = now_ms;
        counted.files += (removal.files.iter())
            .filter(|id| !made_in_window(id, now_ms))
            .count() as u64;
        let affected_files = self.remote.values().map(|counted| counted.files).sum();
        Some(Window {
            deletions: self.remote.len() as u64,
            affected_files,
            space_files: trees.synced_files() - given_up_files + affected_files,
        })
    }

    pub(crate) fn dismiss_remote(&mut self) {
        self.dismissed.extend(self.remote.take_all().into_keys());
    }
}

fn made_in_window(id: &NodeId, now_ms: u64) -> bool {
    unix_ms(&id.to_string()).is_some_and(|made| made <= now_ms && now_ms < made + WINDOW_MS)
}

fn not_after<K: Ord + Clone>(counts: &mut TrackedMap<K, Counted>, now_ms: u64) {
    let later: Vec<K> = (counts.iter())
        .filter(|(_, counted)| counted.at_ms > now_ms)
        .map(|(key, _)| key.clone())
        .collect();
    for key in later {
        if let Some(counted) = counts.get_mut(&key) {
            counted.at_ms = now_ms;
        }
    }
}

pub(crate) fn deletions(trees: &Trees, deleted: &TrackedSet<NodeId>) -> BTreeMap<NodeId, Held> {
    let mut children: BTreeMap<&NodeId, Vec<&NodeId>> = BTreeMap::new();
    let mut tops = Vec::new();
    for id in deleted {
        match trees.synced().live(id).map(|shape| &shape.parent) {
            Some(parent) if deleted.contains(parent) => {
                children.entry(parent).or_default().push(id)
            }
            _ => tops.push(id),
        }
    }
    let revisions = |node: &NodeId| {
        (trees.remote().get(node)).map(|remote| Revisions {
            content_rev: remote.content_rev,
            meta_rev: remote.meta_rev,
        })
    };
    tops.into_iter()
        .map(|top| {
            let mut members = BTreeMap::new();
            let mut below = vec![top];
            while let Some(node) = below.pop() {
                members.insert(node.clone(), revisions(node));
                below.extend(children.get(node).into_iter().flatten());
            }
            let files = (members.keys())
                .filter(|node| {
                    trees
                        .synced()
                        .live(node)
                        .is_some_and(|s| s.kind == Kind::File)
                })
                .count() as u64;
            (top.clone(), Held { members, files })
        })
        .collect()
}

fn holds(held: &BTreeMap<NodeId, Held>) -> BTreeSet<NodeId> {
    (held.values())
        .flat_map(|held| held.members.keys().cloned())
        .collect()
}
