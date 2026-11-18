// SPDX-License-Identifier: Apache-2.0

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::names::{name_key, nfc};
use koidrive_engine::tree::NodeId;
use koidrive_proto::v1::{NodeState, NodeStatus};

use super::disk::{Disk, Place};
use super::{pending_stem, tag};

/// A check of the oracle that the engine on main fails on some schedules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Check {
    /// Only the device whose user gave a folder its owed name finishes the pending name.
    OwnerFinish,
    /// A delete stays unless a change its user did not see brings the node back.
    DeleteUndone,
    /// A place a user gave a node holds over every earlier place its user saw.
    PlaceKept,
    /// A write a user made stays on the server unless a later change that saw it replaced it.
    WriteKept,
    /// Any other change a user made stays on the server unless a later change that saw it, a
    /// delete that saw it or the lost-change rule settles it.
    IntentKept,
}

impl Check {
    pub const ALL: [Check; 5] = [
        Check::OwnerFinish,
        Check::DeleteUndone,
        Check::PlaceKept,
        Check::WriteKept,
        Check::IntentKept,
    ];
}

struct Expectation {
    check: Check,
    why: Option<String>,
    fired: BTreeSet<String>,
}

thread_local! {
    static EXPECTED: RefCell<BTreeMap<u64, Expectation>> = const { RefCell::new(BTreeMap::new()) };
    static NEXT: Cell<u64> = const { Cell::new(0) };
}

/// Keeps failures of one check quiet on this thread while it lives. A guard from [`expect_red`]
/// matches one exact message and panics on drop when that failure never happened, so the change
/// that fixes the engine removes it. Every other failure still reports.
#[must_use]
pub struct ExpectedRed {
    id: u64,
    must_fire: bool,
}

fn guard(check: Check, why: Option<String>, must_fire: bool) -> ExpectedRed {
    let id = NEXT.with(|next| next.replace(next.get() + 1));
    let expectation = Expectation {
        check,
        why,
        fired: BTreeSet::new(),
    };
    EXPECTED.with(|expected| expected.borrow_mut().insert(id, expectation));
    ExpectedRed { id, must_fire }
}

pub fn expect_red(check: Check, why: &str) -> ExpectedRed {
    guard(check, Some(why.to_owned()), true)
}

/// Keeps every failure of the check quiet, whatever its message, and asks for none.
pub fn allow_red(check: Check) -> ExpectedRed {
    guard(check, None, false)
}

impl ExpectedRed {
    /// Ends the guard and returns the messages of the failures it kept quiet.
    pub fn fired(self) -> BTreeSet<String> {
        let expectation = EXPECTED.with(|expected| expected.borrow_mut().remove(&self.id));
        expectation
            .map(|expectation| expectation.fired)
            .unwrap_or_default()
    }
}

impl Drop for ExpectedRed {
    fn drop(&mut self) {
        let expectation = EXPECTED.with(|expected| expected.borrow_mut().remove(&self.id));
        let Some(expectation) = expectation else {
            return;
        };
        if self.must_fire && !std::thread::panicking() {
            assert!(
                !expectation.fired.is_empty(),
                "{:?} did not fail with {:?} on a schedule expected to fail it so",
                expectation.check,
                expectation.why
            );
        }
    }
}

pub(super) fn expected(check: Check, why: &str) -> bool {
    EXPECTED.with(|expected| {
        let mut hit = false;
        for expectation in expected.borrow_mut().values_mut() {
            if expectation.check == check && expectation.why.as_deref().is_none_or(|w| w == why) {
                expectation.fired.insert(why.to_owned());
                hit = true;
            }
        }
        hit
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Place,
    Content,
    Status,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Value {
    Place {
        parent: Option<u128>,
        name: String,
    },
    Content {
        write: i64,
        digest: Vec<u8>,
    },
    Live,
    Trashed {
        subtree: Vec<u128>,
        replaced_by: Option<u128>,
    },
}

impl Value {
    pub(super) fn field(&self) -> Field {
        match self {
            Value::Place { .. } => Field::Place,
            Value::Content { .. } => Field::Content,
            Value::Live | Value::Trashed { .. } => Field::Status,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Intent {
    pub(super) author: usize,
    pub(super) made: u64,
    ino: u128,
    kind: &'static str,
    pub(super) value: Value,
    held: bool,
    pub(super) applied: u64,
    pub(super) lost: bool,
    pub(super) dropped: bool,
    pub(super) forgotten: bool,
    pub(super) answer: bool,
}

pub(super) struct Server {
    root: NodeId,
    nodes: BTreeMap<NodeId, NodeState>,
}

impl Server {
    pub(super) fn of(root: &str, nodes: Vec<NodeState>) -> Self {
        Server {
            root: root.to_owned(),
            nodes: (nodes.into_iter())
                .map(|node| (node.node_id.clone(), node))
                .collect(),
        }
    }

    pub(super) fn live(&self, id: &str) -> Option<&NodeState> {
        (self.nodes.get(id)).filter(|node| node.status == NodeStatus::Live as i32)
    }

    pub(super) fn above(&self, id: &str) -> Vec<&str> {
        let mut chain = Vec::new();
        let mut at = self.nodes.get(id).map(|node| node.parent_id.as_str());
        while let Some(parent) = at.filter(|parent| !parent.is_empty()) {
            if chain.contains(&parent) {
                break;
            }
            chain.push(parent);
            at = self.nodes.get(parent).map(|node| node.parent_id.as_str());
        }
        chain
    }
}

fn digest_of(node: &NodeState) -> Option<&[u8]> {
    let content = node.current.as_ref()?.content.as_ref()?;
    Some(content.manifest_digest.as_slice())
}

fn named(held: &str, given: &str) -> bool {
    let (held, given) = (nfc(held), nfc(given));
    if held == given {
        return true;
    }
    let (stem, ext) = match given.rfind('.') {
        Some(dot) if dot > 0 => given.split_at(dot),
        _ => (given.as_str(), ""),
    };
    held.starts_with(&format!("{stem} (conflict from ")) && held.ends_with(&format!("){ext}"))
}

#[derive(Default)]
pub(super) struct Intents {
    list: Vec<Intent>,
    seen: Vec<BTreeMap<usize, u64>>,
    nodes: Vec<BTreeMap<u128, BTreeSet<NodeId>>>,
    merged: BTreeMap<NodeId, NodeId>,
    copies: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

impl Intents {
    pub(super) fn new(devices: usize) -> Self {
        Intents {
            list: Vec::new(),
            seen: vec![BTreeMap::new(); devices],
            nodes: vec![BTreeMap::new(); devices],
            merged: BTreeMap::new(),
            copies: BTreeMap::new(),
        }
    }

    pub(super) fn record(
        &mut self,
        author: usize,
        made: u64,
        ino: u128,
        kind: &'static str,
        value: Value,
    ) -> usize {
        self.seen[author].insert(self.list.len(), made);
        self.list.push(Intent {
            author,
            made,
            ino,
            kind,
            value,
            held: false,
            applied: 0,
            lost: false,
            dropped: false,
            forgotten: false,
            answer: false,
        });
        self.list.len() - 1
    }

    pub(super) fn intent(&self, i: usize) -> &Intent {
        &self.list[i]
    }

    pub(super) fn replaced(&self, i: usize) -> bool {
        let intent = &self.list[i];
        (self.list[i + 1..].iter()).any(|later| {
            later.author == intent.author
                && later.ino == intent.ino
                && later.value.field() == intent.value.field()
        })
    }

    pub(super) fn len(&self) -> usize {
        self.list.len()
    }

    pub(super) fn answered(&mut self, author: usize, made: u64, ino: u128, value: Value) {
        self.record(author, made, ino, "publish", value);
        self.list.last_mut().expect("recorded above").answer = true;
    }

    pub(super) fn ino_of(&self, d: usize, node: &str) -> Option<u128> {
        (self.nodes[d].iter())
            .find(|(_, nodes)| nodes.contains(node))
            .map(|(ino, _)| *ino)
    }

    pub(super) fn merged(&mut self, from: &str, into: &str) {
        self.merged.insert(from.to_owned(), into.to_owned());
    }

    pub(super) fn copied(&mut self, from: &str, copy: &str) {
        (self.copies.entry(from.to_owned()).or_default()).insert(copy.to_owned());
    }

    pub(super) fn with_copies<'a>(&'a self, node: &'a NodeId) -> impl Iterator<Item = &'a NodeId> {
        std::iter::once(node).chain(self.copies.get(node).into_iter().flatten())
    }

    pub(super) fn entries_of<'a>(&'a self, node: &'a NodeId) -> impl Iterator<Item = &'a NodeId> {
        (self.nodes.iter().flat_map(|inos| inos.values()))
            .filter(move |nodes| nodes.contains(node))
            .flatten()
            .flat_map(|id| self.with_copies(id))
    }

    fn holders(&self, i: usize) -> impl Iterator<Item = &NodeId> {
        self.nodes(i).flat_map(|id| self.with_copies(id))
    }

    pub(super) fn nodes_of(&self, device: usize, ino: u128) -> impl Iterator<Item = &NodeId> {
        let ids = self.nodes[device].get(&ino).into_iter().flatten();
        ids.flat_map(|id| std::iter::once(id).chain(self.merged.get(id)))
    }

    pub(super) fn nodes(&self, i: usize) -> impl Iterator<Item = &NodeId> {
        self.nodes_of(self.list[i].author, self.list[i].ino)
    }

    fn on(&self, i: usize, node: &str) -> bool {
        self.nodes(i).any(|id| id == node)
    }

    fn parent_is(&self, i: usize, parent: Option<u128>, held: &str, root: &str) -> bool {
        match parent {
            None => held == root,
            Some(ino) => self.nodes_of(self.list[i].author, ino).any(|id| id == held),
        }
    }

    pub(super) fn saw(&self, j: usize, i: usize) -> bool {
        let made = self.list[j].made;
        (self.seen[self.list[j].author].get(&i)).is_some_and(|at| *at <= made)
    }

    pub(super) fn observe(
        &mut self,
        d: usize,
        clock: u64,
        disk: &Disk,
        inos: &BTreeMap<u128, NodeId>,
        root: &str,
    ) {
        for (ino, node) in inos {
            self.nodes[d].entry(*ino).or_default().insert(node.clone());
        }
        type Shown<'a> = (Option<&'a str>, &'a str, Option<&'a [u8]>);
        let shown: BTreeMap<&NodeId, Shown> = (disk.entries().iter())
            .filter_map(|(ino, entry)| {
                let parent = match &entry.place {
                    Place::Root => Some(root),
                    Place::In(parent) => inos.get(parent).map(|id| id.as_str()),
                    Place::Parked(_) => return None,
                };
                let digest = (entry.content.as_ref()).map(|c| c.manifest_digest.as_slice());
                Some((inos.get(ino)?, (parent, entry.name.as_str(), digest)))
            })
            .collect();
        let writes: BTreeSet<i64> = (disk.entries().values())
            .filter_map(|entry| Some(tag(entry.content.as_ref()?)))
            .collect();
        for i in 0..self.list.len() {
            if self.seen[d].contains_key(&i) {
                continue;
            }
            let visible = {
                let mut nodes = self.nodes(i).map(|id| (id, shown.get(id)));
                match &self.list[i].value {
                    Value::Place { parent, name } => nodes.any(|(_, shown)| {
                        shown.is_some_and(|(above, held, _)| {
                            named(held, name)
                                && above
                                    .is_some_and(|above| self.parent_is(i, *parent, above, root))
                        })
                    }),
                    Value::Content { write, digest } => {
                        writes.contains(write)
                            || nodes.any(|(_, shown)| {
                                shown.is_some_and(|(.., held)| *held == Some(digest.as_slice()))
                            })
                    }
                    Value::Live => nodes.any(|(_, shown)| shown.is_some()),
                    Value::Trashed { .. } => {
                        let nodes: Vec<_> = nodes.collect();
                        !nodes.is_empty() && nodes.iter().all(|(_, shown)| shown.is_none())
                    }
                }
            };
            if visible {
                self.seen[d].insert(i, clock);
            }
        }
    }

    fn holds(&self, i: usize, server: &Server, in_trash: bool) -> bool {
        let there = |id: &str| match in_trash {
            true => server.nodes.get(id),
            false => server.live(id),
        };
        match &self.list[i].value {
            Value::Place { parent, name } => self.nodes(i).any(|id| {
                let merged_into = self.merged.values().any(|into| into == id);
                there(id).is_some_and(|node| {
                    (named(&node.name, name)
                        || merged_into && named(&name_key(&node.name), &name_key(name)))
                        && self.parent_is(i, *parent, &node.parent_id, &server.root)
                })
            }),
            Value::Content { digest, .. } => self
                .holders(i)
                .any(|id| there(id).is_some_and(|node| digest_of(node) == Some(digest.as_slice()))),
            Value::Live => self.nodes(i).any(|id| server.live(id).is_some()),
            Value::Trashed { .. } => {
                self.nodes(i).next().is_some() && self.nodes(i).all(|id| server.live(id).is_none())
            }
        }
    }

    pub(super) fn sync(&mut self, server: &Server, clock: u64, before: Option<&Server>) {
        for i in 0..self.list.len() {
            let holds = self.holds(i, server, true);
            let revived_without = |before: &Server| {
                let Value::Content { digest, .. } = &self.list[i].value else {
                    return false;
                };
                let digest = Some(digest.as_slice());
                self.nodes(i).any(|id| {
                    (before.nodes.get(id)).is_some_and(|node| {
                        node.status != NodeStatus::Live as i32 && digest_of(node) == digest
                    }) && server
                        .live(id)
                        .is_some_and(|node| digest_of(node) != digest)
                })
            };
            let lost = before.is_some_and(|before| !holds || revived_without(before));
            let intent = &mut self.list[i];
            if holds && intent.applied == 0 {
                intent.applied = clock;
            }
            intent.lost |= lost && intent.held;
            intent.held = holds;
        }
    }

    pub(super) fn rewound(&mut self, clock: u64) {
        for intent in &mut self.list {
            intent.dropped |= intent.made < clock;
        }
    }

    pub(super) fn rolled_back(&mut self, d: usize, clock: u64) {
        for intent in &mut self.list {
            intent.forgotten |= intent.author == d && intent.made < clock;
        }
    }

    pub(super) fn give_up(&mut self, d: usize, nodes: &BTreeSet<NodeId>, deletes_only: bool) {
        for i in 0..self.list.len() {
            let delete = matches!(self.list[i].value, Value::Trashed { .. });
            let given_up = self.list[i].author == d
                && (delete || !deletes_only)
                && self.nodes(i).any(|id| nodes.contains(id));
            self.list[i].dropped |= given_up;
        }
    }

    pub(super) fn placed(
        &self,
        author: Option<usize>,
        node: &str,
        parent: &str,
        name: &str,
        root: &str,
    ) -> bool {
        let stem = pending_stem(name);
        (0..self.list.len()).any(|i| {
            let Value::Place {
                parent: above,
                name: given,
            } = &self.list[i].value
            else {
                return false;
            };
            author.is_none_or(|author| self.list[i].author == author)
                && self.on(i, node)
                && (named(name, given) || stem.is_some_and(|stem| named(stem, given)))
                && self.parent_is(i, *above, parent, root)
        })
    }

    pub(super) fn bytes_of(&self, write: i64) -> Option<&Vec<u8>> {
        (self.list.iter()).find_map(|intent| match &intent.value {
            Value::Content {
                write: made,
                digest,
            } if *made == write => Some(digest),
            _ => None,
        })
    }

    pub(super) fn deleted(&self, d: usize, node: &str) -> bool {
        (0..self.list.len()).any(|i| {
            let Value::Trashed { subtree, .. } = &self.list[i].value else {
                return false;
            };
            self.list[i].author == d
                && (subtree.iter()).any(|ino| self.nodes_of(d, *ino).any(|id| id == node))
        })
    }

    pub(super) fn held_bytes_of(&self, write: i64) -> bool {
        (self.list.iter()).any(|intent| {
            intent.applied > 0
                && matches!(intent.value, Value::Content { write: made, .. } if made == write)
        })
    }

    pub(super) fn wrote(&self, d: usize, write: i64) -> bool {
        (self.list.iter()).any(|intent| {
            intent.author == d
                && matches!(intent.value, Value::Content { write: made, .. } if made == write)
        })
    }

    fn settled(&self, i: usize, server: &Server) -> bool {
        let intent = &self.list[i];
        let field = intent.value.field();
        let covers = |j: usize| {
            let Value::Trashed { subtree, .. } = &self.list[j].value else {
                return false;
            };
            let author = self.list[j].author;
            let deleted: BTreeSet<&NodeId> = (subtree.iter())
                .flat_map(|ino| self.nodes_of(author, *ino))
                .collect();
            self.holders(i).any(|id| {
                deleted.contains(id)
                    || (server.above(id).into_iter()).any(|above| self.on(j, above))
            })
        };
        let into_itself = match intent.value {
            Value::Place {
                parent: Some(parent),
                ..
            } => self.nodes_of(intent.author, parent).any(|above| {
                self.nodes(i)
                    .any(|id| id == above || server.above(above).contains(&id.as_str()))
            }),
            _ => false,
        };
        into_itself
            || (0..self.list.len()).any(|j| {
                let other = &self.list[j];
                if j == i || other.dropped && !other.held {
                    return false;
                }
                let later = field == Field::Place
                    && !self.saw(i, j)
                    && (other.applied > intent.applied || other.held);
                let held_as = |k: usize| match (&intent.value, &self.list[k].value) {
                    (Value::Content { digest, .. }, Value::Content { digest: seen, .. }) => {
                        k != i
                            && intent.applied > 0
                            && digest == seen
                            && self.nodes(i).any(|id| self.on(k, id))
                    }
                    _ => false,
                };
                let replaced = other.value.field() == field
                    && self.nodes(i).any(|id| self.on(j, id))
                    && (self.saw(j, i)
                        || later
                        || (0..self.list.len()).any(|k| held_as(k) && self.saw(j, k)));
                let looped = later
                    && other.held
                    && self.nodes(i).any(|id| {
                        let Value::Place {
                            parent: Some(parent),
                            ..
                        } = intent.value
                        else {
                            return false;
                        };
                        self.nodes_of(intent.author, parent).any(|above| {
                            let chain = server.above(above);
                            chain.contains(&id.as_str())
                                && self
                                    .nodes(j)
                                    .any(|moved| moved == above || chain.contains(&moved.as_str()))
                        })
                    });
                let saw_the_action = (0..self.list.len()).any(|k| {
                    let part = &self.list[k];
                    (part.author, part.made, part.ino) == (intent.author, intent.made, intent.ino)
                        && self.saw(j, k)
                });
                let saw_the_value = (0..self.list.len()).any(|k| {
                    let Value::Place { parent, name } = &intent.value else {
                        return false;
                    };
                    let Value::Place {
                        parent: seen_parent,
                        name: seen_name,
                    } = &self.list[k].value
                    else {
                        return false;
                    };
                    let same_parent = match (parent, seen_parent) {
                        (None, None) => true,
                        (Some(parent), Some(seen_parent)) => {
                            self.nodes_of(intent.author, *parent).any(|id| {
                                self.nodes_of(self.list[k].author, *seen_parent)
                                    .any(|seen| seen == id)
                            })
                        }
                        _ => false,
                    };
                    k != i
                        && self.saw(j, k)
                        && nfc(name) == nfc(seen_name)
                        && same_parent
                        && self.nodes(i).any(|id| self.on(k, id))
                });
                let merged = self.nodes(i).any(|id| {
                    self.merged.contains_key(id)
                        || (self.merged.iter()).any(|(from, into)| into == id && self.on(j, from))
                });
                let deleted = matches!(other.value, Value::Trashed { .. })
                    && covers(j)
                    && (saw_the_action
                        || saw_the_value
                        || merged
                        || field != Field::Content && intent.lost && other.made > intent.made);
                replaced || looped || deleted
            })
    }

    pub(super) fn check(
        &self,
        server: &Server,
        gone: impl Fn(usize) -> bool,
    ) -> Result<(), (String, bool)> {
        for (i, intent) in self.list.iter().enumerate() {
            if intent.dropped
                || intent.forgotten
                || gone(intent.author)
                || self.nodes(i).next().is_none()
            {
                continue;
            }
            let author = intent.author;
            if matches!(intent.value, Value::Trashed { .. })
                || intent.answer
                || self.holds(i, server, false)
                || self.settled(i, server)
            {
                continue;
            }
            let seen_back = (0..self.list.len()).any(|j| {
                j != i
                    && matches!(self.list[j].value, Value::Place { .. })
                    && self.nodes(i).any(|id| self.on(j, id))
                    && self.saw(i, j)
                    && self.holds(j, server, false)
            });
            let node = self.nodes(i).next().expect("checked above");
            let kind = intent.kind;
            let why = match &intent.value {
                Value::Content { write, .. } => format!(
                    "write {write} of the {kind} of node {node} on device {author} is not in the \
                     server's tree, and no later change that saw it, no delete that saw it and no \
                     rule for lost changes settles it"
                ),
                Value::Place { name, .. } if seen_back => format!(
                    "the {kind} on device {author} that puts node {node} at {name:?} is not on the \
                     server, though its user saw the place the server holds and no rule for lost \
                     changes settles it"
                ),
                Value::Place { name, .. } => format!(
                    "the {kind} on device {author} that puts node {node} at {name:?} is not on the \
                     server, and no later change that saw it, no delete that saw it and no rule \
                     for lost changes settles it"
                ),
                _ => format!(
                    "node {node} of the {kind} on device {author} is not in the server's tree, and \
                     no delete that saw it and no rule for lost changes settles it"
                ),
            };
            let check = match intent.value {
                Value::Place { .. } if seen_back => Check::PlaceKept,
                Value::Content { .. } => Check::WriteKept,
                _ => Check::IntentKept,
            };
            if expected(check, &why) {
                continue;
            }
            return Err((why, matches!(intent.value, Value::Content { .. })));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_the_server_gives_while_the_name_is_taken_is_the_name() {
        assert!(named("a", "a"));
        assert!(named("E\u{301}", "\u{c9}"));
        assert!(named(
            "C (conflict from device-0 2026-11-09 2).TXT",
            "C.TXT"
        ));
        assert!(named("a (conflict from device-1 2026-11-09)", "a"));
        assert!(!named("A (pending)", "A"));
        assert!(!named("b", "a"));
        assert!(!named("ab (conflict from device-1 2026-11-09)", "a"));
    }
}
