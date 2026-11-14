// SPDX-License-Identifier: Apache-2.0

//! Classifying a node (docs/design/sync-engine.md section 2.3) and the dirty set the planner reads.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use koidrive_ignore::Status;

use crate::ignore::{LocalRules, Matcher, RuleFile, is_rules_file};
use crate::names::name_key;
use crate::state::{Agreed, MovedHere, StateError, TreesChanges, TreesState};
use crate::tracked::{TrackedMap, TrackedSet};
use crate::tree::{
    Content, FileIdentity, Kind, LocalNode, Names, Node, NodeId, RemoteNode, Shape, Tree,
    TreeError, same, same_content,
};

/// The most mutually dependent [`Class::Same`] updates [`Trees::settle`] searches as one group.
/// Every subset of a group is tried, so the search costs up to 2^n checks of n nodes.
pub const SETTLE_GROUP_LIMIT: usize = 12;

/// How many of the versions R held for one file before its current one
/// [`Trees::replaced_versions`] keeps. A loss that rolls a file back further than that is not
/// proven, and the file's content is kept beside R's as a conflict copy.
pub const REPLACED_VERSIONS_KEPT: usize = 64;

/// How many of the earlier live records R held of one node [`TreesState::past_positions`] keeps.
/// A restore that takes a node back further than that does not prove a move of this device lost,
/// and R's position is taken as a remote move.
///
/// [`TreesState::past_positions`]: crate::state::TreesState::past_positions
pub const PAST_POSITIONS_KEPT: usize = 64;

/// An S update of an acknowledged op that waits until S stays a valid tree with it
/// ([`Trees::waiting`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Waiting {
    /// The shape S takes, or `None` to remove the node from S.
    pub shape: Option<Shape>,
    pub origin: Origin,
}

/// Where a waiting S update came from, so that a server loss can tell whether the commit it
/// belongs to is lost (design section 4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The acknowledged results of this device's commit with this `device_seq`.
    Commit(u64),
    /// No commit: a Discard, a restore the mass-delete guard made, an effect, an update both sides
    /// agree on, or loss recovery.
    Other,
}

/// Where a node stands against its merge base S.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// L = S = R.
    InSync,
    /// L = S ≠ R: apply R to disk.
    Remote,
    /// R = S ≠ L: upload as ops.
    Local,
    /// L ≠ S, R ≠ S, L = R: both sides made the same change, so S := R.
    Same,
    /// L ≠ S, R ≠ S, L ≠ R: rebase on the client.
    Concurrent,
}

/// The table of section 2.3, with `None` as ⊥ in each tree. A node that is not dirty on one side
/// takes the other side's value, and a node dirty on both sides is never overwritten.
pub fn classify(l: Option<&Shape>, s: Option<&Shape>, r: Option<&Shape>) -> Class {
    match (same(l, s), same(r, s)) {
        (true, true) => Class::InSync,
        (true, false) => Class::Remote,
        (false, true) => Class::Local,
        (false, false) if same(l, r) => Class::Same,
        (false, false) => Class::Concurrent,
    }
}

/// Whether `tree` holds `id` live and placed below one of `folders`, through live, placed folders.
fn below<N: Node>(tree: &Tree<N>, id: &str, folders: &BTreeSet<NodeId>) -> bool {
    let held = |id: &str| {
        tree.get(id)
            .filter(|node| node.is_live() && node.is_placed())
    };
    let Some(mut current) = held(id) else {
        return false;
    };
    for _ in 0..tree.len() {
        let parent = current.shape().parent.as_str();
        if folders.contains(parent) {
            return true;
        }
        if parent == tree.root {
            return false;
        }
        match held(parent) {
            Some(node) => current = node,
            None => return false,
        }
    }
    false
}

fn path<'a, N: Node>(tree: &'a Tree<N>, id: &str) -> Option<(Vec<&'a str>, Kind)> {
    let held = |id: &str| tree.get(id).filter(|node| node.is_live());
    let shape = held(id)?.shape();
    let mut names = vec![shape.name.as_str()];
    let mut current = shape.parent.as_str();
    for _ in 0..=tree.len() {
        if current == tree.root {
            names.reverse();
            return Some((names, shape.kind));
        }
        let parent = held(current)?.shape();
        names.push(&parent.name);
        current = &parent.parent;
    }
    None
}

/// The nodes of a tree by parent and the `name_key` of their name.
#[derive(Clone, Debug, Default)]
struct ByName(BTreeMap<(NodeId, String), BTreeSet<NodeId>>);

impl ByName {
    fn insert(&mut self, id: &str, shape: &Shape) {
        let key = (shape.parent.clone(), name_key(&shape.name));
        self.0.entry(key).or_default().insert(id.to_owned());
    }

    fn remove(&mut self, id: &str, shape: &Shape) {
        let key = (shape.parent.clone(), name_key(&shape.name));
        if let Some(ids) = self.0.get_mut(&key) {
            ids.remove(id);
            if ids.is_empty() {
                self.0.remove(&key);
            }
        }
    }

    /// The nodes under `parent` whose name has the `name_key` of `name`.
    fn holders<'a>(&'a self, parent: &str, name: &str) -> impl Iterator<Item = &'a NodeId> + 'a {
        let key = (parent.to_owned(), name_key(name));
        self.0.get(&key).into_iter().flatten()
    }

    fn children<'a>(&'a self, parent: &'a str) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.0
            .range((parent.to_owned(), String::new())..)
            .take_while(move |((of, _), _)| of == parent)
            .flat_map(|(_, ids)| ids)
    }
}

/// A node a delete of this device trashed, with the shape R holds in the trash, which is the shape
/// the delete saw.
#[derive(Clone, Debug, PartialEq)]
pub struct Trashed {
    pub id: NodeId,
    pub shape: Shape,
    pub content_rev: u64,
    pub edit_rev: u64,
    pub version_id: String,
    pub kept: Option<NodeId>,
}

/// A change of another device that R held of a node before a bootstrap replaced it, and that this
/// device had not taken, since L and S held the node alike ([`Trees::untaken_changes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Untaken {
    /// R held other content live, with whether a commit of this device that the server lost made
    /// R's record of the file's position.
    Edit { moved_in_lost_commit: bool },
    /// R held the node in the trash.
    Delete,
}

/// R, L and S of one space, and the nodes whose entries changed since they were last found in sync.
/// Every mutation goes through these methods, so a change in any tree marks its node dirty, and
/// [`Trees::settle`] and the planner look only at what changed.
///
/// The trees also own the ignore rules of this device (section 5.10) and decide with them which
/// nodes are excluded here ([`Trees::excluded`]). The rules are not part of the stored state: the
/// driver passes them again after a restart, and nothing is planned until it has
/// ([`Trees::rules_known`]).
#[derive(Clone, Debug)]
pub struct Trees {
    r: Tree<RemoteNode>,
    l: Tree<LocalNode>,
    s: Tree<Shape>,
    /// The files S holds.
    s_files: u64,
    /// The nodes L holds that R holds trashed, counted by trash group.
    held_trash: BTreeMap<String, u64>,
    /// The nodes R holds trashed, counted by trash group.
    trashed: BTreeMap<String, u64>,
    dirty: TrackedSet<NodeId>,
    unsearched: TrackedSet<NodeId>,
    reverted: TrackedSet<NodeId>,
    parked_as: TrackedMap<NodeId, NodeId>,
    waiting: TrackedMap<NodeId, Waiting>,
    /// The nodes whose content the commit in flight creates or updates, each with whether S's content
    /// changed since the commit was built.
    watched: TrackedMap<NodeId, bool>,
    /// The nodes the commit in flight creates or moves, each with the latest position L and R
    /// agreed on since the commit was built.
    watched_positions: TrackedMap<NodeId, Option<Agreed>>,
    /// The nodes a delete of this device trashed, each with the seq of its commit, while R holds
    /// them trashed by that commit.
    trashed_here: TrackedMap<NodeId, u64>,
    removed_here: TrackedMap<NodeId, u64>,
    /// See [`TreesState::discarded`].
    discarded: TrackedMap<NodeId, Shape>,
    adopted: BTreeSet<NodeId>,
    /// The files whose content an edit of this device set, each with the seq of its commit, while
    /// R holds that content.
    edited_here: TrackedMap<NodeId, u64>,
    moved_here: TrackedMap<NodeId, MovedHere>,
    past_positions: TrackedMap<NodeId, Vec<Agreed>>,
    /// See [`TreesState::unsent_edits`].
    unsent_edits: TrackedSet<NodeId>,
    /// See [`TreesState::dropped_edits`].
    dropped_edits: TrackedSet<NodeId>,
    replaced_versions: TrackedMap<NodeId, Vec<String>>,
    resends: TrackedSet<NodeId>,
    local_changes: TrackedSet<NodeId>,
    kept_originals: TrackedMap<NodeId, NodeId>,
    ignore: Matcher,
    /// Nodes whose local copy the user moved into an ignored folder that is no node. L keeps the
    /// entry each had before the move.
    outside: TrackedSet<NodeId>,
    lost: TrackedSet<NodeId>,
    /// The nodes of `outside`, and those L held under them, that left L because another entry took their name
    /// ([`Trees::displace_local`]), each with the identity L held for it.
    displaced: TrackedMap<NodeId, FileIdentity>,
    /// False from a restore until the rules of a full scan are in force.
    rules_known: bool,
    /// The folders whose rules the last scan of each could not read, by node: a folder renamed is the same folder,
    /// and two folders whose names differ only in case are two folders.
    unknown_rules: BTreeSet<NodeId>,
    /// The local entries the open questions hold ([`Trees::set_asked`]).
    asked: BTreeSet<NodeId>,
    /// Dirty nodes marked since the last settle, the only ones whose class can have changed.
    unsettled: BTreeSet<NodeId>,
    /// Dirty nodes the last settle left as [`Class::Same`], which the next one tries again.
    same: BTreeSet<NodeId>,
    /// Nodes whose entry in R, L or S changed since [`Trees::take_changed`].
    changed: BTreeSet<NodeId>,
    /// The placed nodes of L.
    l_names: ByName,
    /// Every node of L, placed or parked, by its identity.
    l_identities: HashMap<FileIdentity, BTreeSet<NodeId>>,
    /// The nodes of S.
    s_names: ByName,
    /// Nodes whose S position neither placed L nor R holds, which [`Trees::settle`] tries to
    /// release every time, since whether it may depends on the rules and on other nodes.
    orphaned: BTreeSet<NodeId>,
    /// The live files of R that are rules files.
    remote_rules_files: BTreeSet<NodeId>,
}

/// Equal when R, L, S and the state the store keeps are equal. What the trees track only until the
/// next settle or checkpoint is left out.
impl PartialEq for Trees {
    fn eq(&self, other: &Self) -> bool {
        self.r == other.r
            && self.l == other.l
            && self.s == other.s
            && self.state() == other.state()
            && self.ignore == other.ignore
            && self.outside == other.outside
            && self.rules_known == other.rules_known
            && self.unknown_rules == other.unknown_rules
    }
}

impl Trees {
    pub fn new(root: impl Into<NodeId>) -> Self {
        let root = root.into();
        Self {
            r: Tree::new(root.clone()),
            l: Tree::new(root.clone()),
            s: Tree::new(root),
            s_files: 0,
            held_trash: BTreeMap::new(),
            trashed: BTreeMap::new(),
            dirty: TrackedSet::default(),
            unsearched: TrackedSet::default(),
            reverted: TrackedSet::default(),
            parked_as: TrackedMap::default(),
            waiting: TrackedMap::default(),
            watched: TrackedMap::default(),
            watched_positions: TrackedMap::default(),
            trashed_here: TrackedMap::default(),
            removed_here: TrackedMap::default(),
            discarded: TrackedMap::default(),
            adopted: BTreeSet::new(),
            edited_here: TrackedMap::default(),
            moved_here: TrackedMap::default(),
            past_positions: TrackedMap::default(),
            unsent_edits: TrackedSet::default(),
            dropped_edits: TrackedSet::default(),
            replaced_versions: TrackedMap::default(),
            resends: TrackedSet::default(),
            local_changes: TrackedSet::default(),
            kept_originals: TrackedMap::default(),
            ignore: Matcher::new(LocalRules::default()),
            outside: TrackedSet::default(),
            lost: TrackedSet::default(),
            displaced: TrackedMap::default(),
            rules_known: true,
            unknown_rules: BTreeSet::new(),
            asked: BTreeSet::new(),
            unsettled: BTreeSet::new(),
            same: BTreeSet::new(),
            changed: BTreeSet::new(),
            l_names: ByName::default(),
            l_identities: HashMap::new(),
            s_names: ByName::default(),
            orphaned: BTreeSet::new(),
            remote_rules_files: BTreeSet::new(),
        }
    }

    pub fn remote(&self) -> &Tree<RemoteNode> {
        &self.r
    }

    pub fn local(&self) -> &Tree<LocalNode> {
        &self.l
    }

    pub fn synced(&self) -> &Tree<Shape> {
        &self.s
    }

    /// The number of files S holds, kept as S changes.
    pub fn synced_files(&self) -> u64 {
        self.s_files
    }

    /// Whether L still holds a node that R holds trashed in `group`.
    pub fn holds_trash_group(&self, group: &str) -> bool {
        self.held_trash.contains_key(group)
    }

    /// Whether R holds a node trashed in `group`.
    pub fn trashes_group(&self, group: &str) -> bool {
        self.trashed.contains_key(group)
    }

    /// The trash group of a node L holds and R holds trashed.
    fn held_trash_group(&self, id: &str) -> Option<String> {
        self.l.get(id)?;
        let remote = self.r.get(id).filter(|remote| remote.trashed)?;
        Some(remote.trash_group.clone())
    }

    /// Counts `id` in `held_trash` by its trash group, `add` or not, before and after a change of
    /// its R or L.
    fn count_held_trash(&mut self, id: &str, add: bool) {
        let group = self.held_trash_group(id);
        count_group(&mut self.held_trash, group, add);
    }

    /// Counts `id` in `trashed` by its trash group, `add` or not, before and after a change of
    /// its R.
    fn count_trashed(&mut self, id: &str, add: bool) {
        let group = (self.r.get(id)).filter(|node| node.trashed);
        let group = group.map(|node| node.trash_group.clone());
        count_group(&mut self.trashed, group, add);
    }

    pub fn dirty(&self) -> &BTreeSet<NodeId> {
        &self.dirty
    }

    /// The nodes whose entry in R, L or S changed since the last call, which empties the set. Each
    /// mutation adds its node, and so does [`Trees::settle`] for each node whose S it updates.
    pub fn take_changed(&mut self) -> BTreeSet<NodeId> {
        std::mem::take(&mut self.changed)
    }

    /// The placed nodes of L under `parent`, by the `name_key` of their names.
    pub fn local_children<'a>(&'a self, parent: &'a str) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.l_names.children(parent)
    }

    /// The nodes of S under `parent`.
    pub fn synced_children<'a>(&'a self, parent: &'a str) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.s_names.children(parent)
    }

    /// The nodes of S under `parent` whose name has the `name_key` of `name`.
    pub(crate) fn synced_holders<'a>(
        &'a self,
        parent: &str,
        name: &str,
    ) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.s_names.holders(parent, name)
    }

    /// The placed node of L under `parent` named exactly `name`.
    pub fn local_child(&self, parent: &str, name: &str) -> Option<&NodeId> {
        (self.l_names.holders(parent, name))
            .filter(|id| self.l.get(id).is_some_and(|node| node.shape.name == name))
            .last()
    }

    /// A node of L, placed or parked, with `identity`: the greatest ID when several have it.
    pub fn local_with_identity(&self, identity: &FileIdentity) -> Option<&NodeId> {
        self.l_identities.get(identity).and_then(|ids| ids.last())
    }

    /// The nodes of L, placed or parked, with `identity`, in ID order.
    pub fn locals_with_identity(
        &self,
        identity: &FileIdentity,
    ) -> impl DoubleEndedIterator<Item = &NodeId> {
        self.l_identities.get(identity).into_iter().flatten()
    }

    /// The placed nodes of L under `parent` whose name has the `name_key` of `name`.
    pub fn local_holders<'a>(
        &'a self,
        parent: &str,
        name: &str,
    ) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.l_names.holders(parent, name)
    }

    fn mark(&mut self, id: &str) {
        self.dirty.insert(id.to_owned());
        self.unsettled.insert(id.to_owned());
        self.changed.insert(id.to_owned());
    }

    pub fn set_remote(&mut self, id: NodeId, node: RemoteNode) {
        self.apply_remote(id, node, Made::Elsewhere);
    }

    pub fn set_remote_made_here(&mut self, id: NodeId, node: RemoteNode) {
        self.apply_remote(id, node, Made::Here);
    }

    pub fn set_remote_edited_here(&mut self, id: NodeId, node: RemoteNode) {
        self.apply_remote(id, node, Made::Edit);
    }

    pub fn set_remote_answered_here(&mut self, id: NodeId, node: RemoteNode) {
        self.apply_remote(id, node, Made::Answer);
    }

    fn all_trees_hold(&self, id: &str) -> bool {
        (self.l.get(id).zip(self.s.live(id)).zip(self.r.live(id))).is_some_and(
            |((local, synced), remote)| local.shape.same(synced) && synced.same(remote),
        )
    }

    fn record_local_change(&mut self, id: NodeId) {
        if !self.all_trees_hold(&id) {
            self.local_changes.insert(id);
        }
    }

    fn end_change_all_trees_hold(&mut self, id: &str) {
        if self.all_trees_hold(id) {
            self.local_changes.remove(id);
        }
    }

    fn apply_remote(&mut self, id: NodeId, node: RemoteNode, made: Made) {
        let made_here = made != Made::Elsewhere;
        let edit = matches!(made, Made::Edit | Made::Answer);
        let hidden = (self.r.get(&id)).is_some_and(|old| old.content_rev != node.content_rev)
            && !node.trashed
            && self.edited(&id)
            && (self
                .acknowledged(&id)
                .and_then(|shape| shape.content.as_ref()))
            .zip(node.shape.content.as_ref())
            .is_some_and(|(synced, remote)| same_content(synced, remote))
            && !(self.l.live(&id).and_then(|local| local.content.as_ref()))
                .zip(node.shape.content.as_ref())
                .is_some_and(|(local, remote)| same_content(local, remote));
        let synced = self.acknowledged(&id);
        let local = self.l.get(&id);
        let local_gone = !made_here && local.is_none();
        let old = (self.r.get(&id)).filter(|old| !old.trashed && !node.trashed);
        let no_user_change = old.is_some_and(|old| {
            old.edit_rev == node.edit_rev
                && old.meta_edit_rev == node.meta_edit_rev
                && synced.is_some_and(|synced| synced.same(&old.shape))
        });
        let passed_over_delete = local_gone
            && no_user_change
            && old.is_some_and(|old| {
                old.content_rev != node.content_rev || old.meta_rev != node.meta_rev
            });
        let placed_here = no_user_change
            && old.is_some_and(|old| {
                old.content_rev == node.content_rev && old.meta_rev != node.meta_rev
            })
            && local.is_some_and(|local| {
                !local.parked && synced.is_some_and(|synced| synced.same(&local.shape))
            });
        let edited_over_delete = local_gone
            && synced.is_some()
            && (self.r.get(&id)).is_some_and(|old| {
                old.edit_rev != node.edit_rev
                    && (old.content_rev == node.content_rev || old.edit_rev < old.content_rev)
            });
        let moved_over_delete = local_gone
            && synced.is_some()
            && old.is_some_and(|old| {
                old.meta_rev == node.meta_rev && old.meta_edit_rev != node.meta_edit_rev
            });
        if let Some(replaced) = (self.r.get(&id))
            .map(|old| &old.version_id)
            .filter(|old| !old.is_empty() && **old != node.version_id)
        {
            let versions = (self.replaced_versions).get_or_insert_with(id.clone(), Vec::new);
            if !versions.contains(replaced) {
                versions.push(replaced.clone());
            }
            if versions.len() > REPLACED_VERSIONS_KEPT {
                versions.remove(0);
            }
        }
        let edited_here = edit
            && !node.trashed
            && (self.r.get(&id)).is_some_and(|old| old.edit_rev != node.edit_rev);
        let removed_again = made_here
            && !node.trashed
            && self.l.get(&id).is_none()
            && self.removed_here.contains_key(&id);
        let sent = (self.acknowledged(&id))
            .filter(|_| edited_here && made == Made::Edit)
            .map(|synced| Shape {
                content: node.shape.content.clone(),
                ..synced.clone()
            });
        let shape = node.shape.clone();
        if edited_here {
            self.edited_here.insert(id.clone(), node.edit_rev);
        }
        self.place_remote(id.clone(), node);
        if let Some(sent) = sent
            && !self.set_synced_if_valid(id.clone(), Some(sent.clone()))
        {
            self.wait(id.clone(), Some(sent), Origin::Other);
        }
        if removed_again && !self.set_synced_if_valid(id.clone(), Some(shape.clone())) {
            self.wait(id.clone(), Some(shape.clone()), Origin::Other);
        }
        if hidden && !self.set_synced_if_valid(id.clone(), None) {
            self.wait(id.clone(), None, Origin::Other);
        }
        let placed_here = placed_here
            && !path(&self.r, &id).is_some_and(|(names, kind)| self.ignores(&names, kind))
            && (shape.parent == self.l.root
                || (self.l.get(&shape.parent))
                    .is_some_and(|parent| !parent.parked && parent.shape.kind == Kind::Dir))
            && self
                .local_holders(&shape.parent, &shape.name)
                .all(|other| *other == id);
        if placed_here && self.set_synced_if_valid(id.clone(), Some(shape.clone())) {
            self.revert(id.clone());
        }
        if passed_over_delete && !self.set_synced_if_valid(id.clone(), Some(shape.clone())) {
            self.wait(id.clone(), Some(shape), Origin::Other);
        }
        if (edited_over_delete || moved_over_delete) && !self.set_synced_if_valid(id.clone(), None)
        {
            self.wait(id, None, Origin::Other);
        }
    }

    /// Sets R's node from a listing, which can take R back to a version that a lost commit
    /// replaced. A listing records no replaced version, and of those recorded it keeps only the ones
    /// up to the version it lists.
    pub fn set_listed_remote(&mut self, id: NodeId, node: RemoteNode) {
        let moved = (self.r.get(&id)).is_some_and(|old| old.version_id != node.version_id);
        if moved && let Some(versions) = self.replaced_versions.get_mut(&id) {
            let kept = (versions.iter())
                .position(|version| *version == node.version_id)
                .map_or(0, |at| at + 1);
            versions.truncate(kept);
            if versions.is_empty() {
                self.replaced_versions.remove(&id);
            }
        }
        self.place_remote(id, node);
    }

    fn place_remote(&mut self, id: NodeId, node: RemoteNode) {
        let sent_by_another = !node.trashed
            && self.resends.contains(&id)
            && (self.l.live(&id)).is_some_and(|local| {
                let holds_local = |remote: &Shape| {
                    let at = Shape {
                        parent: remote.parent.clone(),
                        name: remote.name.clone(),
                        ..local.clone()
                    };
                    at.same(remote)
                };
                holds_local(&node.shape) && !self.r.live(&id).is_some_and(holds_local)
            });
        self.mark(&id);
        let trashed = node.trashed;
        if (self.trashed_here.get(&id)).is_some_and(|&seq| !(node.trashed && node.meta_rev == seq))
        {
            self.trashed_here.remove(&id);
            self.kept_originals.remove(&id);
        }
        if (self.removed_here.get(&id)).is_some_and(|&seq| !(node.trashed && node.meta_rev == seq))
        {
            self.removed_here.remove(&id);
        }
        if node.trashed && !self.trashed_here.contains_key(&id) {
            self.kept_originals.remove(&id);
        }
        if node.trashed {
            self.displaced.remove(&id);
        }
        self.discarded.remove(&id);
        if (self.edited_here.get(&id)).is_some_and(|&seq| node.trashed || node.edit_rev != seq) {
            self.edited_here.remove(&id);
        }
        let kept = |seq: u64| {
            node.meta_rev >= seq
                && (self.r.get(&id)).is_some_and(|old| old.shape.same_position(&node.shape))
        };
        if (self.moved_here.get(&id)).is_some_and(|moved| !kept(moved.seq)) {
            self.moved_here.remove(&id);
        }
        let replaced = (self.r.get(&id)).filter(|old| !old.trashed && old.meta_rev < node.meta_rev);
        if let Some(old) = replaced {
            let past = Agreed {
                rev: old.meta_rev,
                parent: old.shape.parent.clone(),
                name: old.shape.name.clone(),
            };
            let earlier = self.past_positions.get_or_insert_with(id.clone(), Vec::new);
            if !earlier.contains(&past) {
                earlier.push(past);
            }
            if earlier.len() > PAST_POSITIONS_KEPT {
                earlier.remove(0);
            }
        }
        if node.is_live() && node.shape.kind == Kind::File && is_rules_file(&node.shape.name) {
            self.remote_rules_files.insert(id.clone());
        } else {
            self.remote_rules_files.remove(&id);
        }
        self.count_held_trash(&id, false);
        self.count_trashed(&id, false);
        self.r.insert(id.clone(), node);
        self.count_held_trash(&id, true);
        self.count_trashed(&id, true);
        self.note_position(&id);
        self.record_edit_in_trash(&id);
        self.end_change_all_trees_hold(&id);
        if trashed {
            self.yield_resends();
        }
        if sent_by_another {
            self.take_local(id);
        }
    }

    pub(crate) fn resends(&self) -> &BTreeSet<NodeId> {
        &self.resends
    }

    pub(crate) fn end_resend(&mut self, id: &str) {
        self.resends.remove(id);
    }

    fn local_chain<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        std::iter::successors(Some(id), |id| {
            self.l.get(id).map(|node| node.shape.parent.as_str())
        })
        .take(self.l.len() + 1)
    }

    fn changes(&self) -> BTreeSet<&NodeId> {
        (self.local_changes.iter())
            .chain(self.unsent_edits.iter())
            .filter(|id| !self.resends.contains(*id) && !self.excluded(id))
            .filter(|id| {
                matches!(
                    self.acknowledged_class(id),
                    Class::Local | Class::Concurrent
                )
            })
            .collect()
    }

    fn holding_changes(&self) -> BTreeSet<NodeId> {
        (self.changes().into_iter())
            .flat_map(|id| self.local_chain(id).skip(1))
            .map(str::to_owned)
            .collect()
    }

    pub(crate) fn yield_resends(&mut self) {
        if self.resends.is_empty() {
            return;
        }
        let holding_changes = self.holding_changes();
        let mut yielded: Vec<NodeId> = (self.resends.iter())
            .filter(|id| !holding_changes.contains(*id))
            .filter(|id| {
                self.local_chain(id)
                    .any(|id| self.r.get(id).is_some_and(|remote| remote.trashed))
            })
            .cloned()
            .collect();
        yielded.sort_by_key(|id| self.local_chain(id).count());
        for id in yielded {
            self.take_local(id);
        }
    }

    pub(crate) fn resend_folders_of_changes(&mut self) {
        let lacking = |id: &NodeId| self.r.get(id).is_none() && self.acknowledged(id).is_some();
        let released: Vec<NodeId> = (self.changes().into_iter())
            .filter(|id| !self.held(id))
            .cloned()
            .collect();
        let changed: Vec<NodeId> = released.iter().filter(|id| lacking(id)).cloned().collect();
        let holding: BTreeSet<NodeId> = (released.iter())
            .flat_map(|id| self.local_chain(id).skip(1))
            .map(str::to_owned)
            .collect();
        let mut folders: Vec<NodeId> = (holding.into_iter())
            .filter(|id| lacking(id) && !self.held(id) && !changed.contains(id))
            .collect();
        folders.sort_by_key(|id| std::cmp::Reverse(self.local_chain(id).count()));
        for id in changed.into_iter().chain(folders.iter().cloned()) {
            if !self.set_synced_if_valid(id.clone(), None) {
                self.wait(id, None, Origin::Other);
            }
        }
        self.resends.extend(folders);
        self.yield_resends();
    }

    pub(crate) fn take_local(&mut self, id: NodeId) {
        self.resends.remove(&id);
        self.unsent_edits.remove(&id);
        let Some(local) = self.l.get(&id).map(|node| node.shape.clone()) else {
            return;
        };
        if !self.set_synced_if_valid(id.clone(), Some(local.clone())) {
            self.wait(id, Some(local), Origin::Other);
        }
    }

    pub(crate) fn write_held_beside(&self, id: &str) -> bool {
        let (Some(local), Some(remote)) = (self.l.live(id), self.r.live(id)) else {
            return false;
        };
        let Some(write) = local.content.as_ref() else {
            return false;
        };
        let unchanged_here = |other: &str, held: &Content| match self.l.live(other) {
            Some(local) => (local.content.as_ref()).is_some_and(|kept| same_content(kept, held)),
            None => self.s.get(other).is_none(),
        };
        local.same_position(remote)
            && (self.r.iter()).any(|(other, node)| {
                other != id
                    && node.is_live()
                    && node.shape.parent == remote.parent
                    && (node.shape.content.as_ref()).is_some_and(|held| {
                        same_content(held, write)
                            && held.mtime_ns == write.mtime_ns
                            && unchanged_here(other, held)
                    })
            })
    }

    pub fn remove_remote(&mut self, id: &str) {
        self.remote_rules_files.remove(id);
        self.trashed_here.remove(id);
        self.removed_here.remove(id);
        self.kept_originals.remove(id);
        self.count_held_trash(id, false);
        self.count_trashed(id, false);
        self.displaced.remove(id);
        self.edited_here.remove(id);
        self.replaced_versions.remove(id);
        self.resends.remove(id);
        self.moved_here.remove(id);
        self.past_positions.remove(id);
        if self.r.remove(id).is_some() {
            self.mark(id);
        }
    }

    /// The live files of R that are rules files ([`is_rules_file`]).
    pub fn remote_rules_files(&self) -> &BTreeSet<NodeId> {
        &self.remote_rules_files
    }

    /// Sets L. A reverted node that L now holds at a position other than its old one and R's was
    /// moved by the user, and is no longer reverted: that move is committed instead.
    pub fn set_local(&mut self, id: NodeId, node: LocalNode) {
        if let Some(old) = self.l.get(&id) {
            if !old.shape.same(&node.shape) {
                self.resends.remove(&id);
            }
            let moved = !old.parked && !node.parked && !old.shape.same_position(&node.shape);
            let to_remote =
                (self.r.live(&id)).is_some_and(|remote| node.shape.same_position(remote));
            if moved && !to_remote {
                self.reverted.remove(&id);
            }
            if !old.parked {
                self.l_names.remove(&id, &old.shape);
            }
            let identity = old.identity;
            self.forget_identity(&id, &identity);
        }
        if !node.parked {
            self.parked_as.remove(&id);
            self.l_names.insert(&id, &node.shape);
        }
        self.lost.remove(&id);
        if self.outside.remove(&id) {
            let children: Vec<NodeId> = self.l_names.children(&id).cloned().collect();
            self.outside.extend(children);
        }
        self.displaced.remove(&id);
        self.kept_originals.remove(&id);
        (self.l_identities.entry(node.identity).or_default()).insert(id.clone());
        self.mark(&id);
        self.count_held_trash(&id, false);
        self.l.insert(id.clone(), node);
        self.count_held_trash(&id, true);
        self.note_position(&id);
        self.record_edit_in_trash(&id);
        self.end_change_all_trees_hold(&id);
    }

    /// Sets L as this device's user made it: from a scan of the disk, or as the copy that keeps a
    /// change of the user. A shape other than the one L holds is recorded as a change of the user
    /// ([`TreesState::local_changes`]), and a change back to the shape S holds ends the record unless
    /// an update of S waits. A change to a file this device sends again after a server loss
    /// ([`TreesState::resends`]) is an edit the server does not have ([`Trees::unsent_edits`]), even
    /// when it ends at the bytes S holds.
    pub fn set_local_changed(&mut self, id: NodeId, node: LocalNode) {
        if !(self.l.get(&id)).is_some_and(|old| old.shape.same(&node.shape)) {
            if node.shape.content.is_some() && self.resends.contains(&id) {
                self.unsent_edits.insert(id.clone());
            }
            if !self.waiting.contains_key(&id) && same(Some(&node.shape), self.s.live(&id)) {
                self.local_changes.remove(&id);
            } else {
                self.local_changes.insert(id.clone());
            }
        }
        self.set_local(id, node);
    }

    pub(crate) fn sent_shape(&mut self, id: &str, shape: &Shape) {
        if (self.l.get(id)).is_some_and(|local| !local.shape.same(shape)) {
            self.record_local_change(id.to_owned());
        }
    }

    pub fn remove_local(&mut self, id: &str) {
        self.parked_as.remove(id);
        self.unknown_rules.remove(id);
        self.outside.remove(id);
        self.lost.remove(id);
        self.count_held_trash(id, false);
        self.unsent_edits.remove(id);
        self.resends.remove(id);
        self.local_changes.remove(id);
        self.reverted.remove(id);
        if let Some(old) = self.l.remove(id) {
            if !old.parked {
                self.l_names.remove(id, &old.shape);
            }
            self.forget_identity(id, &old.identity);
            self.mark(id);
        }
    }

    fn forget_identity(&mut self, id: &str, identity: &FileIdentity) {
        if let Some(ids) = self.l_identities.get_mut(identity) {
            ids.remove(id);
            if ids.is_empty() {
                self.l_identities.remove(identity);
            }
        }
    }

    /// The rules in force on this device. New trees hold the built-in exclusions only.
    pub fn ignore(&self) -> &Matcher {
        &self.ignore
    }

    /// Replaces this device's include and exclude lists, which are never synced.
    pub fn set_local_rules(&mut self, local: LocalRules) {
        self.ignore.set_local(local, std::iter::empty());
    }

    /// Sets the rules of the `.koiignore` file in the folder at `dir`, the names from the root to
    /// it, or removes them with `None`. The driver parses each rules file it observes on this
    /// device's disk and passes the rules here with the observation.
    pub fn set_rules<S: AsRef<str>>(&mut self, dir: &[S], rules: Option<RuleFile>) {
        self.ignore.set_rules(dir, rules, std::iter::empty());
    }

    /// Forgets the rules of every `.koiignore` file, for a caller about to pass all of them again.
    pub fn clear_rules(&mut self) {
        self.ignore.clear_rules();
    }

    /// Records that a scan could not read the folder `id` of L, or the root, so the rules below it are unknown until
    /// a scan reads it again ([`Trees::set_rules_known`]) or L no longer holds it. Meanwhile nothing is planned for a
    /// node below it ([`Trees::held`]).
    pub fn set_rules_unknown(&mut self, id: &str) {
        if id == self.l.root || self.l.get(id).is_some() {
            self.unknown_rules.insert(id.to_owned());
        }
    }

    /// The folders whose rules are unknown ([`Trees::set_rules_unknown`]).
    pub fn unknown_rules(&self) -> &BTreeSet<NodeId> {
        &self.unknown_rules
    }

    /// Records that a scan read the folder `id` and passed its rules, or found it has none.
    pub fn set_rules_known(&mut self, id: &str) {
        self.unknown_rules.remove(id);
    }

    /// Sets the local entries R does not hold that the open questions hold (the commit planner's
    /// questions), which stay on disk as they are until the person answers.
    pub fn set_asked(&mut self, asked: BTreeSet<NodeId>) {
        self.asked = asked;
    }

    /// Whether an open question holds the node's local entry ([`Trees::set_asked`]), or L, S or R
    /// places it below a folder whose rules are unknown ([`Trees::set_rules_unknown`]). Neither an effect nor a
    /// commit is planned for it, and no effect parks it to make room for another node.
    pub fn held(&self, id: &str) -> bool {
        self.asked.contains(id)
            || !self.unknown_rules.is_empty()
                && (below(&self.l, id, &self.unknown_rules)
                    || below(&self.s, id, &self.unknown_rules)
                    || below(&self.r, id, &self.unknown_rules))
    }

    /// Whether `id` is an entry the person discarded on a first link (section 6.4), that R does not
    /// hold and the user changed since: L holds it at another place or with other content than
    /// the person discarded, or holds an entry below it, held by no question, that was not
    /// discarded there or that is reclaimed itself. The entry is the user's again, so it is not
    /// removed: the commit planner drops it from S, with every reclaimed folder above it, and
    /// sends them again.
    pub fn reclaimed(&self, id: &str) -> bool {
        self.discard_state(id) == Some(Discard::Reclaimed)
    }

    fn discard_state(&self, id: &str) -> Option<Discard> {
        let (Some(local), Some(discarded)) = (self.l.get(id), self.discarded.get(id)) else {
            return None;
        };
        if self.r.get(id).is_some() {
            return None;
        }
        let mut changed = !local.shape.same(discarded);
        let mut holds_excluded = false;
        for child in self.local_children(id) {
            if changed {
                break;
            }
            let state = self.discard_state(child);
            let excluded = !self.parked(child)
                && (state == Some(Discard::Kept) || self.ignored_in_place(child));
            holds_excluded |= excluded;
            changed = !excluded
                && !self.held(child)
                && ((self.discarded.get(child)).is_none_or(|shape| shape.parent != id)
                    || state == Some(Discard::Reclaimed));
        }
        match (changed, holds_excluded) {
            (true, _) if !self.parked(id) && self.ignored_in_place(id) => None,
            (true, _) => Some(Discard::Reclaimed),
            (false, true) => Some(Discard::Kept),
            (false, false) => Some(Discard::Pending),
        }
    }

    pub(crate) fn adopt_pending(&mut self, id: NodeId) {
        self.adopted.insert(id);
    }

    pub(crate) fn adopted(&self, id: &str) -> bool {
        self.adopted.contains(id)
    }

    pub(crate) fn rewound(&mut self) {
        self.adopted.clear();
        self.watched.clear();
        self.watched_positions.clear();
        self.trashed_here.clear();
        self.edited_here.clear();
        let r = &self.r;
        self.reverted.retain(|id| r.get(id).is_some());
        let waiting: Vec<NodeId> = self.waiting.keys().cloned().collect();
        for id in waiting {
            if let Some(waiting) = self.waiting.get_mut(&id) {
                waiting.origin = Origin::Other;
            }
        }
    }

    pub(crate) fn is_discarded(&self, id: &str) -> bool {
        self.discarded.contains_key(id) && !self.reclaimed(id)
    }

    pub(crate) fn rekey_discarded(&mut self, from: &str, to: &str) {
        if let Some(shape) = self.discarded.remove(from) {
            self.discarded.insert(to.to_owned(), shape);
        }
        let children: Vec<NodeId> = (self.discarded.iter())
            .filter(|(_, shape)| shape.parent == from)
            .map(|(id, _)| id.clone())
            .collect();
        for id in children {
            if let Some(shape) = self.discarded.get_mut(&id) {
                shape.parent = to.to_owned();
            }
        }
    }

    pub(crate) fn discard(
        &mut self,
        ids: impl IntoIterator<Item = NodeId>,
        shown: &BTreeMap<NodeId, Shape>,
    ) {
        for id in ids {
            if self.unsent_edits.remove(&id) {
                self.mark(&id);
            }
            if let (None, Some(shape)) = (self.r.get(&id), shown.get(&id)) {
                self.discarded.insert(id, shape.clone());
            }
        }
    }

    pub(crate) fn approve(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        for id in ids {
            if self.reverted.remove(&id) {
                self.mark(&id);
            }
        }
    }

    pub(crate) fn reclaim(&mut self, among: impl Fn(&str) -> bool) -> Vec<NodeId> {
        let reclaimed: BTreeSet<NodeId> = (self.discarded.keys())
            .filter(|id| among(id) && self.reclaimed(id))
            .cloned()
            .collect();
        let below_reclaimed = |id: &str| {
            let mut parent = self.s.get(id).map(|shape| &shape.parent);
            for _ in 0..self.s.len() {
                match parent {
                    Some(dir) if reclaimed.contains(dir) => return true,
                    Some(dir) => parent = self.s.get(dir).map(|shape| &shape.parent),
                    None => return false,
                }
            }
            false
        };
        let excluded_below: Vec<NodeId> = (self.discarded.keys())
            .filter(|id| self.excluded(id) && below_reclaimed(id))
            .cloned()
            .collect();
        let mut left: Vec<NodeId> = reclaimed.iter().cloned().chain(excluded_below).collect();
        let mut dropped = Vec::new();
        loop {
            let before = left.len();
            left.retain(|id| {
                let done = self.set_synced_if_valid(id.clone(), None);
                if done {
                    dropped.push(id.clone());
                }
                !done
            });
            if left.len() == before {
                break;
            }
        }
        if !left.is_empty() && self.drop_together(&left) {
            dropped.append(&mut left);
        }
        for id in reclaimed.iter().filter(|id| !left.contains(id)) {
            self.discarded.remove(id);
        }
        let (l, s, waiting) = (&self.l, &self.s, &self.waiting);
        (self.discarded)
            .retain(|id, _| l.get(id).is_some() || s.get(id).is_some() || waiting.contains_key(id));
        dropped
    }

    fn drop_together(&mut self, ids: &[NodeId]) -> bool {
        let mut offered: BTreeMap<NodeId, Option<Shape>> = (self.waiting.iter())
            .map(|(id, waiting)| (id.clone(), waiting.shape.clone()))
            .chain((self.same.iter()).map(|id| (id.clone(), self.r.live(id).cloned())))
            .filter(|(id, _)| !ids.contains(id))
            .collect();
        let mut updates: BTreeMap<NodeId, Option<Shape>> =
            ids.iter().map(|id| (id.clone(), None)).collect();
        loop {
            let mut s = self.s.clone();
            for (id, shape) in &updates {
                match shape {
                    Some(shape) => {
                        s.insert(id.clone(), shape.clone());
                    }
                    None => {
                        s.remove(id);
                    }
                }
            }
            let involved: Vec<NodeId> = match s.check(Names::ByKey) {
                Ok(()) => break,
                Err(TreeError::NameTaken { parent, name }) => (s.iter())
                    .filter(|(_, shape)| {
                        shape.parent == parent && name_key(&shape.name) == name_key(&name)
                    })
                    .map(|(id, _)| id.clone())
                    .collect(),
                Err(
                    TreeError::MissingParent(id)
                    | TreeError::ParentNotDir(id)
                    | TreeError::Cycle(id),
                ) => {
                    let mut chain = vec![id];
                    for _ in 0..s.len() {
                        match s.get(chain.last().expect("the chain starts with a node")) {
                            Some(shape) if !chain.contains(&shape.parent) => {
                                chain.push(shape.parent.clone())
                            }
                            _ => break,
                        }
                    }
                    chain
                }
                Err(TreeError::RootStored) => return false,
            };
            let needed: Vec<(NodeId, Option<Shape>)> = (involved.iter())
                .filter_map(|id| offered.remove_entry(id))
                .collect();
            if needed.is_empty() {
                return false;
            }
            updates.extend(needed);
        }
        for (id, shape) in updates {
            self.set_synced_unmarked(&id, shape);
            self.same.remove(&id);
            self.mark(&id);
        }
        true
    }

    /// The caller has passed the rules files of a full scan made since the last restore, and the
    /// device-local list. Planning starts again.
    pub fn rules_scanned(&mut self) {
        self.rules_known = true;
    }

    /// False from [`Trees::restore`] until [`Trees::rules_scanned`]. The stored state does not hold
    /// the rules, so until the first full scan has reported them an excluded node would look
    /// like any other; meanwhile no effect and no commit is planned.
    pub fn rules_known(&self) -> bool {
        self.rules_known
    }

    /// Records that the user moved the local copy of `id` into an ignored folder that is no node,
    /// so L cannot hold its new position. L keeps the entry as it was, and the node is excluded
    /// until an observation places it again or removes it. When the node is placed again, its
    /// children are recorded in its place: the user may have moved them elsewhere while it was
    /// excluded, so each stays excluded until an observation places it too. The record is part of
    /// the stored state. A scan after a restart cannot always make it again: it does not find the
    /// node in the ignored folder when another file has taken the node's path.
    pub fn ignore_local(&mut self, id: &str) {
        if self.l.get(id).is_some() {
            self.outside.insert(id.to_owned());
            self.lost.remove(id);
        }
    }

    /// Records, as [`Trees::ignore_local`] does, a folder the disk lost that L keeps because a node recorded below it
    /// is in an ignored folder. Once nothing recorded is left below it, the folder may leave L, since the disk does not
    /// hold it anywhere. The record is part of the stored state.
    pub fn keep_lost_local(&mut self, id: &str) {
        if self.l.get(id).is_some() {
            self.outside.insert(id.to_owned());
            self.lost.insert(id.to_owned());
        }
    }

    /// The folders [`Trees::keep_lost_local`] recorded that no observation has placed or removed since.
    pub fn lost_local(&self) -> &BTreeSet<NodeId> {
        &self.lost
    }

    /// Whether [`Trees::ignore_local`] recorded the node, or a folder L holds it under.
    pub fn recorded_local(&self, id: &str) -> bool {
        let mut above = id;
        for _ in 0..=self.l.len() {
            if self.outside.contains(above) {
                return true;
            }
            match self.l.get(above) {
                Some(node) => above = &node.shape.parent,
                None => break,
            }
        }
        false
    }

    /// Takes the node out of L because another entry took its name, and keeps the record that its local copy is in
    /// an ignored folder: L cannot hold two entries at one name, and the copy is still there. The node stays excluded
    /// ([`Trees::excluded`]) until an observation places it again or R holds it trashed. This record is part of the
    /// stored state, since no scan can find the node again once L has no entry for it. The caller passes only a node
    /// [`Trees::recorded_local`] holds for.
    pub fn displace_local(&mut self, id: &str) {
        let Some(identity) = self.l.get(id).map(|node| node.identity) else {
            return;
        };
        self.remove_local(id);
        if self.r.live(id).is_some() {
            self.displaced.insert(id.to_owned(), identity);
        }
    }

    /// The nodes [`Trees::displace_local`] took out of L, each with the identity L held for it.
    pub fn displaced_local(&self) -> &BTreeMap<NodeId, FileIdentity> {
        &self.displaced
    }

    /// The node [`Trees::displace_local`] took out of L with `identity`: the greatest ID when several have it. An
    /// identity without a birth time finds none: the record can outlive the node's copy, and the file system may
    /// give its inode to another file, which only the birth time tells apart.
    pub fn displaced_with_identity(&self, identity: &FileIdentity) -> Option<&NodeId> {
        (self.displaced.iter().rev())
            .find(|(_, held)| *held == identity && identity.birth_ns.is_some())
            .map(|(id, _)| id)
    }

    /// Whether S holds the node at a position that neither placed L nor R holds, the nodes
    /// [`Trees::released`] may give that position up for.
    fn orphaned_in_s(&self, id: &str) -> bool {
        let Some(synced) = self.s.live(id) else {
            return false;
        };
        let held = |shape: Option<&Shape>| shape.is_some_and(|shape| shape.same_position(synced));
        let placed = (self.l.get(id))
            .filter(|node| !node.parked)
            .map(|node| &node.shape);
        !held(placed) && !held(self.r.live(id))
    }

    /// What S may become for an excluded node whose S position neither L nor R holds any more,
    /// in order of preference, or `None` when the node is not one. A parked node holds no
    /// position in L, so one that the rules now leave out gives up its S position too, also
    /// while its old name is taken and it cannot be moved back. Nothing is planned for the node,
    /// so no op would ever move its S entry, and the name it holds in S would keep every other
    /// node from being acknowledged there. With the node in L and live in R, S takes R's
    /// position, or else the position the local copy has, and keeps its content, so the content
    /// is still compared with the last synced one when the node is included again. Otherwise, or
    /// when S can hold the node at neither position, S drops the node, which is then resolved
    /// like a node never synced: what R holds is downloaded, and what only L holds is uploaded.
    fn released(&self, id: &str) -> Option<Vec<Option<Shape>>> {
        let synced = self.s.live(id)?;
        let local = self.l.get(id);
        let remote = self.r.live(id);
        let held = |shape: Option<&Shape>| shape.is_some_and(|shape| shape.same_position(synced));
        let placed = local.filter(|node| !node.parked).map(|node| &node.shape);
        if held(placed) || held(remote) || !self.ignored(id) {
            return None;
        }
        let at = |shape: &Shape| {
            Some(Shape {
                parent: shape.parent.clone(),
                name: shape.name.clone(),
                ..synced.clone()
            })
        };
        let mut next = Vec::new();
        if local.is_some()
            && let Some(remote) = remote
        {
            next.push(at(remote));
            next.extend(placed.map(at));
        }
        next.push(None);
        Some(next)
    }

    /// The nodes recorded by [`Trees::ignore_local`] that no observation has placed or removed
    /// since.
    pub fn ignored_local(&self) -> &BTreeSet<NodeId> {
        &self.outside
    }

    /// Whether the rules exclude the entry of `kind` at `path`, the names from the root to it.
    pub fn ignores<S: AsRef<str>>(&self, path: &[S], kind: Kind) -> bool {
        let kind = match kind {
            Kind::Dir => koidrive_ignore::Kind::Dir,
            Kind::File => koidrive_ignore::Kind::File,
        };
        self.ignore.status(path, kind) != Status::Included
    }

    /// Whether the node is excluded on this device (section 5.10): its position in R, S or L
    /// matches the rules, or so does its place in L below its R parent, or a folder R holds it under is excluded, or its local copy, or a folder L holds it under, was moved into an
    /// ignored folder, or it is a discarded folder L still holds unchanged with an excluded entry in it. Nothing is planned for an excluded node, in either direction. Its S entry
    /// stays the merge base it was: S takes only what L and R both hold, and gives up a position
    /// neither holds any more, so that the entry never keeps another node out of S. A node that L holds at an included position
    /// different from S's is not excluded: the user moved it out, and the move is planned like
    /// any local move. A parked node is not excluded either until the effects have placed it
    /// again, since it must not stay in the parked directory ([`Trees::ignored`]).
    pub fn excluded(&self, id: &str) -> bool {
        !self.l.get(id).is_some_and(|node| node.parked) && self.ignored(id)
    }

    /// Whether L no longer holds a node while R holds an excluded node below it. The commit planner sends no
    /// `Delete` for such a folder, which stays on the server with what it holds that this device leaves out
    /// (section 5.10), and the effects do not create it again, so nothing is left to sync for it. The returned check
    /// remembers what it found below each node, so asking it about every node of R reads each node once.
    pub fn kept_for_excluded(&self) -> impl Fn(&str) -> bool + '_ {
        let children = OnceCell::new();
        let below = RefCell::new(HashMap::new());
        move |id| {
            if self.l.get(id).is_some() || self.r.live(id).is_none() {
                return false;
            }
            let children = children.get_or_init(|| {
                let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
                for (node, _) in self.r.iter() {
                    if let Some(shape) = self.r.live(node) {
                        children
                            .entry(shape.parent.as_str())
                            .or_default()
                            .push(node);
                    }
                }
                children
            });
            self.excluded_below(id, children, &mut below.borrow_mut())
        }
    }

    fn excluded_below(
        &self,
        id: &str,
        children: &HashMap<&str, Vec<&str>>,
        below: &mut HashMap<String, bool>,
    ) -> bool {
        if let Some(found) = below.get(id) {
            return *found;
        }
        below.insert(id.to_owned(), false);
        let found = (children.get(id).into_iter().flatten())
            .any(|child| self.excluded(child) || self.excluded_below(child, children, below));
        below.insert(id.to_owned(), found);
        found
    }

    /// Whether the node is outside the sync tree on this device, as [`Trees::excluded`] decides
    /// it, also when the node is parked. The effects move a parked node that is ignored back to
    /// the position L keeps for it, and it is excluded from then on.
    pub fn ignored(&self, id: &str) -> bool {
        self.discard_state(id) == Some(Discard::Kept) || self.ignored_in_place(id)
    }

    fn ignored_in_place(&self, id: &str) -> bool {
        if let Some(ignored) = self.ignored_by_paths(id) {
            return ignored;
        }
        let mut above = Vec::new();
        let mut current = id;
        for _ in 0..self.r.len() {
            let Some(remote) = self.r.live(current) else {
                break;
            };
            let folder = remote.parent.as_str();
            let Some(placed) = self.r.live(folder).filter(|_| folder != self.r.root) else {
                break;
            };
            let apart =
                |shape: Option<&Shape>| shape.is_some_and(|shape| !shape.same_position(placed));
            let apart = apart(self.l.get(folder).map(|node| &node.shape))
                || apart(self.s.live(folder))
                || self.outside.contains(folder)
                || self.displaced.contains_key(folder);
            above.push((folder, current, apart));
            current = folder;
        }
        let Some(top) = above.iter().rposition(|(.., apart)| *apart) else {
            return false;
        };
        above[..=top]
            .iter()
            .find_map(|&(folder, child, apart)| {
                if apart {
                    self.ignored_by_paths(folder)
                } else {
                    self.excluded_where_child_paths_miss(folder, child)
                        .then_some(true)
                }
            })
            .unwrap_or(false)
    }

    fn excluded_where_child_paths_miss(&self, folder: &str, child: &str) -> bool {
        let there = path(&self.r, folder);
        let synced = path(&self.s, folder).filter(|synced| {
            Some(synced) != there.as_ref()
                && self
                    .s
                    .live(child)
                    .is_none_or(|shape| shape.parent != folder)
        });
        let written = (self.l.get(folder).is_none())
            .then(|| self.written(folder))
            .flatten()
            .filter(|written| Some(written) != there.as_ref());
        [synced, written]
            .iter()
            .flatten()
            .any(|(names, kind)| self.ignores(names, *kind))
    }

    fn written(&self, id: &str) -> Option<(Vec<&str>, Kind)> {
        let remote = self.r.live(id)?;
        let mut names = vec![remote.name.as_str()];
        let mut current = remote.parent.as_str();
        for _ in 0..=self.l.len() {
            if current == self.l.root {
                names.reverse();
                return Some((names, remote.kind));
            }
            if current == id {
                return None;
            }
            let parent = &self.l.get(current)?.shape;
            names.push(&parent.name);
            current = &parent.parent;
        }
        None
    }

    fn ignored_by_paths(&self, id: &str) -> Option<bool> {
        if self.recorded_local(id) || self.displaced.contains_key(id) {
            return Some(true);
        }
        let ignored =
            |path: Option<(Vec<&str>, Kind)>| path.map(|(names, kind)| self.ignores(&names, kind));
        let placed = path(&self.l, id);
        let local = ignored(placed.clone());
        let moved = match (self.l.get(id), self.s.live(id)) {
            (Some(local), Some(synced)) => !local.shape.same_position(synced),
            _ => false,
        };
        if local == Some(false) && moved {
            return Some(false);
        }
        let there = path(&self.r, id);
        let written = self.written(id);
        let matches = |path: &Option<(Vec<&str>, Kind)>| {
            (path.as_ref()).is_some_and(|(names, kind)| self.ignores(names, *kind))
        };
        (local == Some(true)
            || ignored(path(&self.s, id)) == Some(true)
            || matches(&there)
            || (written != there && written != placed && matches(&written)))
        .then_some(true)
    }

    pub(crate) fn ignored_in_remote(&self, id: &str) -> bool {
        path(&self.r, id).is_some_and(|(names, kind)| self.ignores(&names, kind))
    }

    /// The name of a parked node's entry in the parked directory: its ID, or, for a node re-keyed
    /// while parked, the ID it was parked under. The entry keeps that name until it leaves the
    /// parked directory.
    pub fn parked_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.parked_as.get(id).map_or(id, String::as_str)
    }

    /// Records that parked node `id` sits in the parked directory under `name`.
    pub fn park_as(&mut self, id: NodeId, name: NodeId) {
        if id != name {
            self.parked_as.insert(id, name);
        }
    }

    /// S updates of acknowledged ops that would not leave S a valid tree yet, by node. The commit
    /// layer makes each as soon as it fits, falls back to S's position once R and L have both left
    /// the waiting one, and drops one once neither L nor R holds the node; until then the node is
    /// left out of planning, unless L moved it away from the waiting position, where R still holds it.
    pub fn waiting(&self) -> &BTreeMap<NodeId, Waiting> {
        &self.waiting
    }

    /// Watches S's content of `ids`, the nodes whose content the commit just built creates or updates, and
    /// stops watching any other node.
    pub fn watch_content(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        self.watched.clear();
        self.watched.extend(ids.into_iter().map(|id| (id, false)));
    }

    /// Whether S's content of `id` changed since [`Trees::watch_content`] started watching it,
    /// even if it changed back.
    pub fn took_content(&self, id: &str) -> bool {
        self.watched.get(id) == Some(&true)
    }

    /// Watches the position L and R agree on for `ids`, the nodes the commit just built creates or
    /// moves, and stops watching any other node.
    pub fn watch_positions(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        self.watched_positions.clear();
        (self.watched_positions).extend(ids.into_iter().map(|id| (id, None)));
    }

    /// The latest position of `id` that L and R agreed on since [`Trees::watch_positions`] started
    /// watching it, when R's record that held it came after `seq`.
    pub fn position_after(&self, id: &str, seq: u64) -> Option<&Agreed> {
        (self.watched_positions.get(id)?.as_ref()).filter(|agreed| agreed.rev > seq)
    }

    pub(crate) fn take_waiting(&mut self) -> BTreeMap<NodeId, Waiting> {
        self.waiting.take_all()
    }

    pub(crate) fn wait(&mut self, id: NodeId, shape: Option<Shape>, origin: Origin) {
        self.mark(&id);
        self.waiting.insert(id, Waiting { shape, origin });
    }

    /// Sets S, or clears it with `None` (S := ⊥).
    pub fn set_synced(&mut self, id: NodeId, shape: Option<Shape>) {
        self.set_synced_unmarked(&id, shape);
        self.mark(&id);
    }

    /// Sets S for one node, or clears it, only when S stays a valid tree. Returns whether it did.
    pub fn set_synced_if_valid(&mut self, id: NodeId, shape: Option<Shape>) -> bool {
        if !self.fits_synced(&id, shape.as_ref()) {
            return false;
        }
        self.set_synced(id, shape);
        true
    }

    /// Records that a delete of this device, in the commit at `seq`, trashed `id`, if R holds it
    /// trashed by that commit.
    pub fn record_trashed(&mut self, id: &str, seq: u64, kept: Option<&str>) {
        if (self.r.get(id)).is_some_and(|remote| remote.trashed && remote.meta_rev == seq) {
            self.trashed_here.insert(id.to_owned(), seq);
            match kept {
                Some(kept) => self.kept_originals.insert(id.to_owned(), kept.to_owned()),
                None => self.kept_originals.remove(id),
            };
        }
    }

    pub fn is_removed_here(&self, id: &str) -> bool {
        self.removed_here.contains_key(id)
    }

    pub fn removed_here(&self) -> BTreeMap<NodeId, u64> {
        (*self.removed_here).clone()
    }

    /// After a listing replaces R, makes each node of `removed` that R now holds live and L
    /// doesn't hold a local delete again when `here` says this device brought it back at the
    /// node's revision.
    pub fn listed_removals(&mut self, removed: BTreeMap<NodeId, u64>, here: impl Fn(u64) -> bool) {
        for (id, removed_at) in removed {
            let Some(node) = self.r.get(&id).filter(|node| !node.trashed) else {
                continue;
            };
            let edited_since = node.content_rev.max(node.edit_rev) > removed_at;
            if self.l.get(&id).is_some() || edited_since || !here(node.meta_rev) {
                continue;
            }
            let shape = node.shape.clone();
            if !self.set_synced_if_valid(id.clone(), Some(shape.clone())) {
                self.wait(id, Some(shape), Origin::Other);
            }
        }
    }

    pub(crate) fn removed_restored(&self, id: &str, target: &str) -> Vec<NodeId> {
        let Some(node) = self.r.get(id).filter(|node| node.trashed) else {
            return Vec::new();
        };
        let in_group = |member: &str| {
            let mut current = member;
            for _ in 0..=self.r.len() {
                if current == id {
                    return true;
                }
                match self.r.get(current) {
                    Some(above) if above.trashed && above.trash_group == node.trash_group => {
                        current = &above.shape.parent
                    }
                    _ => return false,
                }
            }
            false
        };
        let mut parents = BTreeSet::new();
        let mut current = if target.is_empty() {
            &node.shape.parent
        } else {
            target
        };
        while let Some(parent) = self.r.get(current).filter(|parent| parent.trashed) {
            if !parents.insert(current) {
                break;
            }
            current = &parent.shape.parent;
        }
        (self.removed_here.keys())
            .filter(|member| in_group(member) || parents.contains(member.as_str()))
            .cloned()
            .collect()
    }

    pub fn kept_original(&self, id: &str) -> Option<&NodeId> {
        (self.kept_originals.get(id)).filter(|kept| {
            self.l.get(kept).is_some()
                || (self.s.get(kept).is_none() && !self.trashed_here.contains_key(*kept))
        })
    }

    /// Records that an edit of this device set the content of `id` to `sent`, at R's edit
    /// revision, if R holds that content live.
    pub fn record_edited(&mut self, id: &str, sent: Option<&Content>) {
        let Some(remote) = (self.r.get(id)).filter(|remote| !remote.trashed) else {
            return;
        };
        if (remote.shape.content.as_ref().zip(sent))
            .is_some_and(|(held, sent)| same_content(held, sent))
        {
            self.edited_here.insert(id.to_owned(), remote.edit_rev);
        }
    }

    /// Records that a create or a move of this device, in the commit at `seq`, set the position of
    /// `id`, if R holds the position of that commit, or held it and holds the same position again.
    pub fn record_moved(&mut self, id: &str, seq: u64) {
        let held = |remote: &RemoteNode| {
            remote.meta_rev == seq
                || (self.past_positions.get(id).into_iter().flatten()).any(|past| {
                    past.rev == seq
                        && past.parent == remote.shape.parent
                        && past.name == remote.shape.name
                })
        };
        if (self.r.get(id)).is_some_and(|remote| !remote.trashed && held(remote)) {
            let moved = MovedHere {
                seq,
                upgraded: false,
            };
            self.moved_here.insert(id.to_owned(), moved);
        }
    }

    /// Forgets the earlier records of R below `rev`, the seq of the oldest commit of this device
    /// that [`Commits::held_through`] can still name, except the newest of them, which a commit at
    /// `rev` or later may have replaced.
    pub fn forget_past_positions_below(&mut self, rev: u64) {
        let forgotten: Vec<(NodeId, Vec<Agreed>)> = (self.past_positions.iter())
            .filter_map(|(id, past)| {
                let newest_below = (past.iter().map(|position| position.rev))
                    .filter(|&past_rev| past_rev < rev)
                    .max();
                let keep =
                    |position: &&Agreed| position.rev >= rev || Some(position.rev) == newest_below;
                if past.iter().all(|position| keep(&position)) {
                    return None;
                }
                Some((id.clone(), past.iter().filter(keep).cloned().collect()))
            })
            .collect();
        for (id, kept) in forgotten {
            if kept.is_empty() {
                self.past_positions.remove(&id);
            } else {
                self.past_positions.insert(id, kept);
            }
        }
    }

    /// The nodes whose position a create or a move of this device set, and whose position S and R
    /// still hold, R live or in the trash, for [`Trees::forget_lost`] once a bootstrap has replaced
    /// R.
    pub fn moved_here(&self) -> Vec<(NodeId, MovedHere)> {
        (self.moved_here.iter())
            .filter(|(id, _)| {
                let remote = self.r.get(id).map(|remote| &remote.shape);
                remote
                    .zip(self.s.live(id))
                    .is_some_and(|(r, s)| r.same_position(s))
            })
            .map(|(id, moved)| (id.clone(), moved.clone()))
            .collect()
    }

    /// The files whose content an edit of this device set, and whose content S and R still hold,
    /// each with the seq of that edit's commit, for [`Trees::forget_lost`] once a bootstrap has
    /// replaced R.
    pub fn edited_here(&self) -> Vec<(NodeId, u64)> {
        (self.edited_here.iter())
            .filter(|(id, _)| {
                let content = |shape: Option<&Shape>| shape.and_then(|shape| shape.content.clone());
                let remote = content(self.r.live(id));
                let synced = content(self.s.live(id));
                matches!((remote, synced), (Some(remote), Some(synced)) if same_content(&remote, &synced))
            })
            .map(|(id, seq)| (id.clone(), *seq))
            .collect()
    }

    /// The nodes whose content L and S hold as R holds it, live or in the trash, a folder's as any
    /// folder's, where no commit of this device that the server lost set R's content and L holds no
    /// unsent edit ([`Trees::unsent_edits`]): `lost_here` tells whether a seq is one of those
    /// commits. For [`Trees::forget_lost`] once a bootstrap has replaced R.
    pub fn taken_from_others(&self, lost_here: impl Fn(u64) -> bool) -> Vec<NodeId> {
        (self.s.iter())
            .filter(|(id, _)| !self.unsent_edits.contains(*id))
            .filter(|(id, synced)| {
                let holds = |shape: Option<&Shape>| {
                    shape.is_some_and(|shape| match (&shape.content, &synced.content) {
                        (Some(held), Some(synced)) => same_content(held, synced),
                        (held, synced) => held.is_none() && synced.is_none(),
                    })
                };
                let remote = (self.r.get(id))
                    .filter(|remote| !lost_here(remote.content_rev.max(remote.edit_rev)));
                holds(remote.map(|remote| &remote.shape)) && holds(self.l.live(id))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The changes of other devices that this device has not taken yet, for [`Trees::forget_lost`]
    /// once a bootstrap has replaced R: the nodes L holds as S does, with no local change below them,
    /// while R holds them in the trash, and the files whose content L and S hold alike, with no
    /// waiting S update to other content, while R holds other content live. Neither holds an unsent edit or content an edit of this
    /// device set ([`Trees::edited_here`]). Each edit comes with whether a commit of this device
    /// that the server lost made R's record of the file's position: `lost_here` tells whether a seq
    /// is one of those commits.
    pub fn untaken_changes(&self, lost_here: impl Fn(u64) -> bool) -> Vec<(NodeId, Untaken)> {
        let holding_changes = self.holding_changes();
        (self.s.iter())
            .filter(|(id, _)| {
                !self.unsent_edits.contains(*id) && !self.edited_here.contains_key(*id)
            })
            .filter_map(|(id, synced)| {
                let remote = self.r.get(id)?;
                if remote.trashed {
                    let unchanged = self.l.live(id).is_some_and(|local| local.same(synced))
                        && !holding_changes.contains(id);
                    return unchanged.then(|| (id.clone(), Untaken::Delete));
                }
                let held = synced.content.as_ref()?;
                let acknowledged = self.acknowledged(id)?.content.as_ref()?;
                let local = self.l.live(id)?.content.as_ref()?;
                let theirs = remote.shape.content.as_ref()?;
                (same_content(local, held)
                    && same_content(acknowledged, held)
                    && !same_content(theirs, held))
                .then(|| {
                    let moved_in_lost_commit = lost_here(remote.meta_rev);
                    (
                        id.clone(),
                        Untaken::Edit {
                            moved_in_lost_commit,
                        },
                    )
                })
            })
            .collect()
    }

    /// R's record of each node, live or in the trash, for [`Trees::forget_lost`] once a bootstrap
    /// has replaced R.
    pub fn remote_records(&self) -> BTreeMap<NodeId, RemoteNode> {
        (self.r.iter())
            .map(|(id, remote)| (id.clone(), remote.clone()))
            .collect()
    }

    /// The nodes a delete of this device trashed and R still holds trashed by it, for
    /// [`Trees::forget_lost`] once a bootstrap has replaced R.
    pub fn trashed_here(&self) -> Vec<Trashed> {
        (self.trashed_here.keys())
            .filter_map(|id| {
                let remote = self.r.get(id)?;
                Some(Trashed {
                    id: id.clone(),
                    shape: remote.shape.clone(),
                    content_rev: remote.content_rev,
                    edit_rev: remote.edit_rev,
                    version_id: remote.version_id.clone(),
                    kept: self.kept_originals.get(id).cloned(),
                })
            })
            .collect()
    }

    /// Forgets what the server may have lost, after a bootstrap from a server that is behind what
    /// this device applied (design §4.4). S forgets every node that R does not hold at all or holds
    /// live with another shape, and every node S holds below one. A node L still holds is then
    /// created again instead of being taken for a remote delete, under its own ID and at the
    /// position L holds: a node R does not hold at all is no longer reverted, since the server's
    /// rename or merge that reverted it was lost too. One whose content differs from
    /// R's is kept beside R's as a conflict copy instead of being replaced by R's older content. A
    /// node R holds in the trash keeps S, so it stays a remote delete, also below a node S forgets,
    /// unless it is a file whose edit the server lost (see `edited` below).
    /// S then forgets each such ancestor only once the delete has taken what it held: the update
    /// waits ([`Trees::waiting`]) and is made as soon as it fits.
    ///
    /// `records` holds R's record of each node before the bootstrap ([`Trees::remote_records`]).
    /// S's content came from that record or an earlier one. A node keeps S whatever R's shape when
    /// that record survived: R now holds it at the same version, or its content revision is at or
    /// below `held_through`. The server then kept S's content, and what R holds otherwise is a
    /// remote change this device has not taken yet.
    ///
    /// A node R holds live as S does but at another position keeps S, so R's position is a remote
    /// move: a device that did not set S's position cannot tell whether a lost commit set it, and
    /// sending it again would undo a move another device made since. `moved` lists the
    /// nodes whose position a create or a move of this device set before the bootstrap
    /// ([`Trees::moved_here`]). S forgets such a node when the server lost that commit, its seq
    /// above `held_through`, and R holds the node as it was before the commit: at a revision at or
    /// below `held_through`, or with a record R held earlier, or when the store upgrade
    /// recorded the move without those records. So the position this device set is sent again,
    /// unless another device moved the node after the loss. S takes R's position for these nodes
    /// before `edited` and `synced` take R's content at the position S holds, so a file whose
    /// move and edit were both lost is sent again as both. Where S cannot take that position yet,
    /// it waits, and `edited` takes R's content at the position that waits, so the lost edit waits
    /// with it.
    ///
    /// `trashed` lists what deletes of this device had trashed before the bootstrap
    /// ([`Trees::trashed_here`]), and `held_through` the seq of the newest commit of this device the
    /// server still counts ([`Commits::held_through`]), at or below which every seq is from before
    /// the loss. A node of `trashed` that R now holds live as it was deleted, with no revision
    /// above `held_through` or at a position this device saw with the revisions and version the
    /// delete trashed, lost its delete: nothing changed it after the loss, since a server never
    /// gives a version ID twice (RFC 0016).
    /// S takes R's shape for it, parents first, so the delete is sent again. Any other node of it is
    /// downloaded again, since a restore or another change after the loss can take a seq the lost
    /// commits had.
    ///
    /// `edited` lists the files whose content an edit of this device set before the bootstrap
    /// ([`Trees::edited_here`]), each with the seq of that edit's commit, and the files of
    /// [`TreesState::dropped_edits`] whose content in L differs from S's. A file of it that R now holds,
    /// live or in the trash, with content of no revision above `held_through` or at a version the
    /// log replaced for it before ([`TreesState::replaced_versions`]), lost the edit. S takes R's
    /// content for it at the position S holds, so the content L holds is sent again as an edit
    /// instead of being kept beside R's as a conflict copy. A delete R holds trashed the content
    /// from before the edit, so the edit beats it (R8): the server revives the file instead of this
    /// device moving the edit to the local trash. That also undoes a delete by a device that saw the
    /// edit before the loss, which the engine cannot tell apart: the revived file can be deleted
    /// again, while an edit in the local trash would be missing from the server.
    ///
    /// A file S forgets that L and R hold may hold, or later get, an edit of this device to the
    /// bytes R now holds, such as the content the server went back to (GUN-438). So may a file of
    /// `edited` whose edit's commit has a seq above `held_through`, which the server lost. L and R
    /// would then agree, so the edit would be taken for R's content, and a resend of the lost
    /// content would replace it. Each such file is recorded ([`Trees::unsent_edits`]), so L's
    /// content is sent, and a resend then meets an edit it did not see. A forgotten file whose
    /// content L, S and R all hold alike is not recorded: only its position was lost. A file R holds
    /// in the trash keeps S, and is recorded when L holds content S does not, since a device that
    /// restores it from the trash with L's bytes would otherwise make L, S and R agree.
    ///
    /// `synced` lists the nodes whose content L and S held as R did before the bootstrap, where no
    /// commit of this device that the server lost set R's content ([`Trees::taken_from_others`]).
    /// A lost move of this device leaves the content another device's, so the node is listed.
    /// A file of it that R now holds live at the position S holds, at a version the log replaced for it before
    /// ([`TreesState::replaced_versions`]), holds content from before the loss that S's
    /// content replaced: nothing changed it since. S takes R's shape, so the content L holds is sent
    /// again as an edit instead of being kept beside R's as a conflict copy. Any other file of it may
    /// hold an edit another device made after the loss, or content older than R's that a listing
    /// gave, so it stays on the path below.
    ///
    /// A node of `synced` that S takes R's shape for above, or forgets below while L holds it as S
    /// did, is sent again with no change of this device's user in it ([`TreesState::resends`]). A
    /// delete that R holds of it, or of a folder L holds it under, before the server takes the
    /// resend was made after the loss by a user who may have held what the resend brings back, so
    /// the delete beats the resend (GUN-548), unless L holds a change of this device's user below
    /// the resend: that change beats the delete (R8) and needs the resend as its folder. Once L
    /// holds no such change, the next build gives way. A change L holds below a folder S holds and
    /// R lacks, also one found after the folder gave way, sends the folder again at the next build,
    /// and a node S holds and R lacks that L changes itself is created again at L's position.
    /// Resends give way parents first, so S takes each folder before what it holds.
    /// R taking the node live with the content L holds, where it lacked the node or held other
    /// content, ends its resend too: another device sent it first, and its position is a remote
    /// move. The device that made the change sends it again itself, as above.
    ///
    /// A node of `moved` or `edited` whose commit has a seq above `held_through` holds a change of
    /// this device's user that the server lost ([`TreesState::local_changes`]).
    ///
    /// `accepted` is the count of this device's commits the server still holds (*w*, section
    /// 2.8). An S update that waits ([`Trees::waiting`]) from a commit above it came from a commit
    /// the server lost. The update is dropped, a removal as well as a shape, so S keeps what it held: L's change is
    /// compared with S and sent again when R holds the node as S does, and kept beside R's when R
    /// holds it otherwise. A node R does not hold at all is then no longer reverted, as one S
    /// forgets. An update from a kept commit, or from no commit, stays, so a change another device
    /// made since reads as a remote change once the update fits. That holds for a node R holds in
    /// the trash too: a lost update kept there would stand for S in the effects, which would then
    /// replace an edit only L holds with content another device restores, and later commits take
    /// its slot again. No update keeps a slot above `accepted` after a loss.
    ///
    /// A position L and R agreed on while the commit in flight was out ([`Trees::position_after`])
    /// is kept only where the server still holds the record that showed it: one of a seq at or
    /// below `held_through`, or one R now holds the node at, live and at that revision. Any other
    /// record may be lost, and a later record can take its seq, so a revision comparison cannot
    /// tell it survived (GUN-457). The agreement is then taken again from L and the new R.
    ///
    /// `untaken` lists the changes of other devices that this device had not taken before the
    /// bootstrap ([`Trees::untaken_changes`]). A node L held as S did while R held it in the trash
    /// was deleted by another device, so L holds nothing the server lacks. It keeps S whatever the
    /// new R holds, so the trash, R lacking the node, or the content a restore brought back is a
    /// remote change, and the delete is not undone.
    ///
    /// The other changes are files whose content L and S held alike while R held other content
    /// live, each with whether a lost commit of this device set R's position of it.
    /// R's content was newer than S's, so L holds nothing the server lacks. Each such file that the
    /// new R holds, live or in the trash, keeps S's content and is never recorded as holding an
    /// unsent edit, so whatever content R holds now is taken as a remote change, and a delete made
    /// since follows it (GUN-591). It keeps S's position too, so a move R holds is a remote change,
    /// except where a lost commit of this device set the position and R holds the file live: S
    /// takes R's position, so L's is sent again. One that S forgets below a folder S forgets gets that shape
    /// back as a waiting update (section 2.7) once the folder is in S again, instead of L's older
    /// content being kept beside R's as a conflict copy. One that S forgets while L holds it as S
    /// did is sent again with no change of this device's user in it, as a node of `synced` is,
    /// unless a lost commit of this device set its position.
    ///
    /// [`Commits::held_through`]: crate::commit::Commits::held_through
    #[allow(clippy::too_many_arguments)]
    pub fn forget_lost(
        &mut self,
        trashed: Vec<Trashed>,
        edited: Vec<(NodeId, u64)>,
        synced: Vec<NodeId>,
        moved: Vec<(NodeId, MovedHere)>,
        untaken: Vec<(NodeId, Untaken)>,
        records: BTreeMap<NodeId, RemoteNode>,
        held_through: u64,
        accepted: u64,
    ) {
        let seen = |id: &NodeId, remote: &RemoteNode| {
            (self.past_positions.get(id).into_iter().flatten()).any(|past| {
                past.rev == remote.meta_rev
                    && past.parent == remote.shape.parent
                    && past.name == remote.shape.name
            })
        };
        let before = |id: &NodeId, moved: &MovedHere| {
            (self.r.get(id))
                .filter(|remote| !remote.trashed)
                .is_some_and(|remote| {
                    remote.meta_rev <= held_through
                        || (moved.upgraded && remote.meta_rev < moved.seq)
                        || seen(id, remote)
                })
        };
        let edited: Vec<(NodeId, u64)> = (edited.into_iter())
            .chain(
                (self.dropped_edits.iter())
                    .filter(|id| self.local_content_differs(id))
                    .map(|id| (id.clone(), u64::MAX)),
            )
            .collect();
        self.dropped_edits.clear();
        let lost_here = (moved.iter().map(|(id, moved)| (id, moved.seq)))
            .chain(edited.iter().map(|(id, seq)| (id, *seq)))
            .filter(|(id, seq)| *seq > held_through && self.l.get(id).is_some())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let lost_moves: BTreeSet<NodeId> = (moved.into_iter())
            .filter(|(id, moved)| moved.seq > held_through && before(id, moved))
            .map(|(id, _)| id)
            .collect();
        let lost_edits: BTreeSet<NodeId> = (edited.iter())
            .filter(|(_, seq)| *seq > held_through)
            .map(|(id, _)| id.clone())
            .collect();
        let deleted_here: BTreeSet<NodeId> =
            (trashed.iter()).map(|deleted| deleted.id.clone()).collect();
        let mut deletes: Vec<(NodeId, Shape, Option<NodeId>)> = (trashed.into_iter())
            .filter(|deleted| {
                self.l.get(&deleted.id).is_none() && self.s.get(&deleted.id).is_none()
            })
            .filter(|deleted| {
                (deleted.kept.as_ref())
                    .is_none_or(|kept| deleted_here.contains(kept) || self.r.live(kept).is_some())
            })
            .filter_map(|deleted| {
                let remote = (self.r.get(&deleted.id))
                    .filter(|remote| !remote.trashed && remote.shape.same(&deleted.shape))
                    .filter(|remote| {
                        remote.content_rev.max(remote.meta_rev) <= held_through
                            || (seen(&deleted.id, remote)
                                && remote.content_rev == deleted.content_rev
                                && remote.edit_rev == deleted.edit_rev
                                && remote.version_id == deleted.version_id)
                    })?;
                Some((deleted.id, remote.shape.clone(), deleted.kept))
            })
            .collect();
        for id in lost_here {
            self.record_local_change(id);
        }
        while let Some(at) = (0..deletes.len()).find(|&at| {
            let (id, shape, _) = &deletes[at];
            self.fits_synced(id, Some(shape))
        }) {
            let (id, shape, kept) = deletes.swap_remove(at);
            if let Some(kept) = kept.filter(|kept| !deleted_here.contains(kept)) {
                self.kept_originals.insert(id.clone(), kept);
            }
            self.set_synced(id, Some(shape));
        }
        let mut queued = BTreeSet::new();
        for id in &lost_moves {
            let (Some(remote), Some(synced)) = (self.r.live(id), self.s.live(id)) else {
                continue;
            };
            let before = Shape {
                parent: remote.parent.clone(),
                name: remote.name.clone(),
                ..synced.clone()
            };
            if !self.set_synced_if_valid(id.clone(), Some(before.clone())) {
                queued.insert(id.clone());
                self.wait(id.clone(), Some(before), Origin::Other);
            }
        }
        for (id, _) in edited {
            let replaced = self.replaced_versions.get(&id);
            let waits_at = (self.waiting.get(&id))
                .filter(|_| queued.contains(&id))
                .and_then(|waiting| waiting.shape.as_ref());
            let Some(remote) = (self.r.get(&id))
                .filter(|remote| {
                    remote.content_rev <= held_through
                        || replaced.is_some_and(|versions| versions.contains(&remote.version_id))
                })
                .and_then(|remote| {
                    let s = waits_at.or(self.s.live(&id))?;
                    Some(Shape {
                        parent: s.parent.clone(),
                        name: s.name.clone(),
                        ..remote.shape.clone()
                    })
                })
            else {
                continue;
            };
            if !self.set_synced_if_valid(id.clone(), Some(remote.clone())) {
                queued.insert(id.clone());
                self.wait(id, Some(remote), Origin::Other);
            }
        }
        let synced: BTreeSet<NodeId> = synced.into_iter().collect();
        for id in synced.iter().cloned() {
            let replaced = self.replaced_versions.get(&id);
            let Some(remote) = (self.r.get(&id))
                .filter(|remote| !remote.trashed)
                .filter(|remote| {
                    replaced.is_some_and(|versions| versions.contains(&remote.version_id))
                })
                .filter(|remote| {
                    (self.s.live(&id))
                        .is_some_and(|s| s.same_position(&remote.shape) && !s.same(&remote.shape))
                })
                .map(|remote| remote.shape.clone())
            else {
                continue;
            };
            if self.set_synced_if_valid(id.clone(), Some(remote)) {
                self.resends.insert(id);
            }
        }
        let dropped: Vec<NodeId> = (self.waiting.iter())
            .filter(
                |(_, waiting)| matches!(waiting.origin, Origin::Commit(slot) if slot > accepted),
            )
            .map(|(id, _)| id.clone())
            .collect();
        for id in dropped {
            if self.r.get(&id).is_none() {
                self.reverted.remove(&id);
            }
            self.waiting.remove(&id);
            self.mark(&id);
        }
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (id, shape) in self.s.iter() {
            children.entry(&shape.parent).or_default().push(id);
        }
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (id, shape) in self.s.iter() {
            children.entry(&shape.parent).or_default().push(id);
        }
        let holds_live_below = |id: &str| {
            let mut below = vec![id];
            while let Some(id) = below.pop() {
                for &child in children.get(id).into_iter().flatten() {
                    if self.r.live(child).is_some() {
                        return true;
                    }
                    below.push(child);
                }
            }
            false
        };
        let candidates: BTreeSet<&str> = (untaken.iter())
            .filter(|(id, change)| *change == Untaken::Delete && !holds_live_below(id))
            .map(|(id, _)| id.as_str())
            .collect();
        let in_place = |id: &str| {
            let mut parent = self.s.get(id).map(|synced| synced.parent.as_str());
            while let Some(id) = parent {
                if self.r.get(id).is_some() {
                    return true;
                }
                let Some(synced) = self.s.get(id) else {
                    return true;
                };
                if !candidates.contains(id) {
                    return false;
                }
                parent = Some(synced.parent.as_str());
            }
            true
        };
        let deleted: BTreeSet<NodeId> = (candidates.iter())
            .filter(|id| in_place(id))
            .map(|id| (*id).to_owned())
            .collect();
        let taken_late: BTreeSet<NodeId> = (untaken.iter())
            .filter(|(_, change)| {
                *change
                    == Untaken::Edit {
                        moved_in_lost_commit: false,
                    }
            })
            .map(|(id, _)| id.clone())
            .collect();
        let untaken: BTreeMap<NodeId, Shape> = (untaken.into_iter())
            .filter_map(|(id, change)| {
                let Untaken::Edit {
                    moved_in_lost_commit,
                } = change
                else {
                    return None;
                };
                let remote = self.r.get(&id)?;
                let mut kept = self.s.get(&id)?.clone();
                if moved_in_lost_commit && !remote.trashed && remote.meta_rev <= held_through {
                    kept.parent = remote.shape.parent.clone();
                    kept.name = remote.shape.name.clone();
                }
                Some((id, kept))
            })
            .collect();
        let mut lost: BTreeSet<&str> = (self.s.iter())
            .filter(|(id, _)| !deleted.contains(*id))
            .filter(|(id, synced)| {
                let synced = (self.waiting.get(*id))
                    .filter(|_| queued.contains(*id))
                    .and_then(|waiting| waiting.shape.as_ref())
                    .unwrap_or(*synced);
                let moved = |remote: &Shape| {
                    let at = Shape {
                        parent: remote.parent.clone(),
                        name: remote.name.clone(),
                        ..(*synced).clone()
                    };
                    at.same(remote)
                };
                let survived =
                    (records.get(*id))
                        .zip(self.r.get(id))
                        .is_some_and(|(old, remote)| {
                            old.content_rev <= held_through
                                || (!old.version_id.is_empty()
                                    && old.version_id == remote.version_id)
                        });
                self.r.get(id).is_none()
                    || (!untaken.contains_key(*id)
                        && !survived
                        && (self.r.live(id))
                            .is_some_and(|remote| !remote.same(synced) && !moved(remote)))
            })
            .map(|(id, _)| id.as_str())
            .collect();
        let mut below: Vec<&str> = lost.iter().copied().collect();
        while let Some(id) = below.pop() {
            for &child in children.get(id).into_iter().flatten() {
                let trashed = self.r.get(child).is_some_and(|remote| remote.trashed);
                if trashed || deleted.contains(child) || !lost.insert(child) {
                    continue;
                }
                below.push(child);
            }
        }
        let mut waits = BTreeSet::new();
        for (_, shape) in self.s.iter().filter(|(id, _)| !lost.contains(id.as_str())) {
            let mut parent = shape.parent.as_str();
            while lost.contains(parent) && waits.insert(parent) {
                parent = &self.s.live(parent).expect("a lost node is in S").parent;
            }
        }
        let lost: Vec<(NodeId, bool)> = (lost.into_iter())
            .map(|id| (id.to_owned(), waits.contains(id)))
            .collect();
        let content = |shape: Option<&Shape>| shape.and_then(|shape| shape.content.clone());
        let in_trash: Vec<NodeId> = (self.s.iter())
            .filter(|(id, _)| self.r.get(id).is_some_and(|remote| remote.trashed))
            .filter(|(id, _)| self.edited(id))
            .map(|(id, _)| id.clone())
            .collect();
        let unsent: Vec<NodeId> = (lost.iter().map(|(id, _)| id))
            .chain(&lost_edits)
            .chain(&in_trash)
            .filter(|id| !untaken.contains_key(*id))
            .filter(|id| {
                let remote = self.r.get(id).map(|remote| &remote.shape);
                let (Some(local), Some(remote)) = (content(self.l.live(id)), content(remote))
                else {
                    return false;
                };
                let unchanged = content(self.s.live(id)).is_some_and(|synced| {
                    same_content(&local, &synced) && same_content(&local, &remote)
                });
                lost_edits.contains(*id) || !unchanged
            })
            .cloned()
            .collect();
        for id in unsent {
            self.mark(&id);
            self.unsent_edits.insert(id);
        }
        for (id, waits) in lost {
            let unchanged = (self.l.get(&id))
                .is_some_and(|local| (self.s.get(&id)).is_some_and(|s| s.same(&local.shape)));
            if unchanged && (synced.contains(&id) || taken_late.contains(&id)) {
                self.resends.insert(id.clone());
            }
            if self.r.get(&id).is_none() {
                self.reverted.remove(&id);
            }
            if waits {
                self.wait(id, None, Origin::Other);
            } else {
                self.set_synced(id, None);
            }
        }
        for (id, kept) in untaken {
            if self.s.get(&id) != Some(&kept)
                && !self.set_synced_if_valid(id.clone(), Some(kept.clone()))
            {
                self.wait(id, Some(kept), Origin::Other);
            }
        }
        self.yield_resends();
        let unproven: Vec<NodeId> = (self.watched_positions.iter())
            .filter(|(id, agreed)| {
                agreed.as_ref().is_some_and(|agreed| {
                    agreed.rev > held_through
                        && !(self.r.get(id)).is_some_and(|remote| {
                            !remote.trashed
                                && remote.meta_rev == agreed.rev
                                && remote.shape.parent == agreed.parent
                                && remote.shape.name == agreed.name
                        })
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in unproven {
            self.watched_positions.insert(id.clone(), None);
            self.note_position(&id);
        }
        debug_assert_eq!(self.s.check(Names::ByKey), Ok(()));
    }

    pub fn class(&self, id: &str) -> Class {
        self.class_against(id, self.s.live(id))
    }

    /// The node's class with `synced` standing in for S. A file whose content in L is an edit the
    /// server does not hold ([`Trees::unsent_edits`]) is changed locally, although S may hold the
    /// same bytes.
    pub fn class_against(&self, id: &str, synced: Option<&Shape>) -> Class {
        let class = classify(self.l.live(id), synced, self.r.live(id));
        if !self.unsent_edits.contains(id) {
            return class;
        }
        match class {
            Class::InSync => Class::Local,
            Class::Remote => Class::Concurrent,
            class => class,
        }
    }

    /// The files whose content in L is an edit of this device that the server does not hold,
    /// although S and R may hold the same bytes. A server loss leaves one ([`Trees::forget_lost`]),
    /// and so does an edit L holds of a file R holds in the trash. It stays until the server
    /// acknowledges an edit or a create of the file with L's content ([`Trees::sent_edit`]) or
    /// L no longer holds the file. A later edit in L does not end it,
    /// since the server holds none of them until one is acknowledged, and neither does R dropping
    /// the file: the edit beats the removal and the file is created again, as for any edit L holds.
    pub fn unsent_edits(&self) -> &BTreeSet<NodeId> {
        &self.unsent_edits
    }

    /// Records `ids` as files the commit in flight edited when the server lost commits of this
    /// device ([`TreesState::dropped_edits`]).
    pub fn record_dropped_edits(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        self.dropped_edits.extend(ids);
    }

    fn local_content_differs(&self, id: &str) -> bool {
        let local = self.l.live(id).and_then(|shape| shape.content.as_ref());
        let synced = self.s.live(id).and_then(|shape| shape.content.as_ref());
        match (local, synced) {
            (Some(local), Some(synced)) => !same_content(local, synced),
            (local, _) => local.is_some(),
        }
    }

    /// Records that the server acknowledged an edit of `id` to `sent`, so L's content is no longer
    /// an edit it lacks ([`Trees::unsent_edits`]), unless L has since taken other content.
    pub fn sent_edit(&mut self, id: &str, sent: Option<&Content>) {
        let local = self.l.live(id).and_then(|shape| shape.content.as_ref());
        if matches!((local, sent), (Some(local), Some(sent)) if !same_content(local, sent)) {
            return;
        }
        if self.unsent_edits.remove(id) {
            self.mark(id);
        }
    }

    fn record_edit_in_trash(&mut self, id: &str) {
        let edited = self.local_content_differs(id);
        let trashed = self.r.get(id).is_some_and(|remote| remote.trashed);
        if edited && trashed && !self.waiting.contains_key(id) {
            self.unsent_edits.insert(id.to_owned());
        }
    }

    pub(crate) fn acknowledged(&self, id: &str) -> Option<&Shape> {
        match self.waiting.get(id) {
            Some(waiting) => waiting.shape.as_ref(),
            None => self.s.live(id),
        }
    }

    pub(crate) fn acknowledged_class(&self, id: &str) -> Class {
        self.class_against(id, self.acknowledged(id))
    }

    /// Whether L holds content that S does not, or, while the node's S update waits, content the
    /// waiting shape does not: an edit not uploaded yet. So does a file of
    /// [`Trees::unsent_edits`].
    pub fn edited(&self, id: &str) -> bool {
        if self.unsent_edits.contains(id) {
            return true;
        }
        let content = self.l.get(id).and_then(|node| node.shape.content.as_ref());
        match (
            content,
            self.acknowledged(id)
                .and_then(|shape| shape.content.as_ref()),
        ) {
            (Some(local), Some(synced)) => !same_content(local, synced),
            (local, _) => local.is_some(),
        }
    }

    /// Settles what needs no decision: a node changed the same way on both sides takes S := R, and
    /// a node in sync leaves the dirty set unless it is parked. An excluded node that is not in
    /// sync stays dirty, and its S entry takes no change that only one side made, except that it
    /// gives up a position neither side holds any more (see [`Trees::excluded`]). Only the nodes
    /// marked since the last settle are classified again, since a node's class changes only with
    /// its own entries. The [`Class::Same`] nodes the last settle left, and the nodes whose S
    /// position neither side holds, are tried again. S stays a valid tree, so an update that would orphan a
    /// child, put a node under a missing or non-directory parent, close a cycle or take a sibling's
    /// `name_key` is made only together with the updates it depends on. Settling stops only when no
    /// non-empty set of the remaining [`Class::Same`] updates can be made together with S still valid,
    /// searching each group of updates that depend on each other exhaustively. A group of more
    /// than [`SETTLE_GROUP_LIMIT`] updates is not searched: its nodes stay dirty as [`Class::Same`] and
    /// [`Trees::unsearched`] lists them. A [`Class::Same`] node whose update is left still takes R's
    /// content into S, unless an acknowledged update of the node waits, since content never breaks S, so a later change on either side is compared
    /// with the content both sides already hold. [`Trees::work`] gives the dirty nodes that are left.
    pub fn settle(&mut self) {
        self.reverted
            .retain(|id| match (self.l.get(id), self.r.live(id)) {
                (Some(local), Some(remote)) => !local.shape.same_position(remote),
                (local, remote) => local.is_some() || remote.is_some(),
            });
        let mut pending = std::mem::take(&mut self.same);
        let mut unsettled = std::mem::take(&mut self.unsettled);
        unsettled.extend(
            (self.orphaned.iter())
                .filter(|id| self.dirty.contains(*id))
                .cloned(),
        );
        let mut queue: Vec<NodeId> = unsettled.into_iter().collect();
        let mut next = 0;
        while let Some(id) = queue.get(next).cloned() {
            next += 1;
            let released: Vec<Option<Shape>> = self.released(&id).into_iter().flatten().collect();
            let fitting = (released.iter()).find(|next| self.fits_synced(&id, next.as_ref()));
            match fitting {
                Some(next) => self.set_synced_unmarked(&id, next.clone()),
                None if !released.is_empty() => {
                    for node in self.release_subtree(&id) {
                        if !queue[next..].contains(&node) {
                            queue.push(node);
                        }
                    }
                }
                None => {}
            }
            if self.orphaned_in_s(&id) {
                self.orphaned.insert(id.clone());
            } else {
                self.orphaned.remove(&id);
            }
            pending.remove(&id);
            match self.class(&id) {
                Class::InSync if !self.parked(&id) => {
                    self.dirty.remove(&id);
                }
                Class::Same => {
                    self.dirty.insert(id.clone());
                    pending.insert(id);
                }
                _ => {
                    self.dirty.insert(id);
                }
            }
        }
        self.unsearched.clear();
        let seen: BTreeMap<NodeId, Shape> = (pending.iter())
            .filter(|id| self.l.get(id).is_none())
            .filter_map(|id| Some((id.clone(), self.s.get(id)?.clone())))
            .collect();
        let mut taken = Vec::new();
        loop {
            let before = pending.len();
            pending.retain(|id| {
                let fits = self.take_remote(id);
                if fits {
                    taken.push(id.clone());
                }
                !fits
            });
            if pending.is_empty() {
                break;
            }
            if pending.len() < before {
                continue;
            }
            let list: Vec<NodeId> = pending.iter().cloned().collect();
            let together = self.take_remote_together(&list);
            if together.is_empty() {
                break;
            }
            pending.retain(|id| !together.contains(id));
            taken.extend(together);
        }
        for id in taken {
            if self.l.get(&id).is_none()
                && let Some(remote) = self.r.get(&id).filter(|remote| remote.trashed)
                && seen
                    .get(&id)
                    .is_some_and(|seen| match (&seen.content, &remote.shape.content) {
                        (Some(seen), Some(held)) => same_content(seen, held),
                        (seen, held) => seen.is_none() && held.is_none(),
                    })
            {
                self.removed_here.insert(id.clone(), remote.meta_rev);
            }
            if !self.parked(&id) && !self.unsent_edits.contains(&id) {
                self.dirty.remove(&id);
            }
        }
        for id in &pending {
            let (Some(synced), Some(remote)) = (self.s.get(id), self.r.live(id)) else {
                continue;
            };
            if self.waiting.contains_key(id) {
                continue;
            }
            if synced.content != remote.content {
                let next = Shape {
                    content: remote.content.clone(),
                    ..synced.clone()
                };
                self.set_synced_unmarked(id, Some(next));
            }
        }
        self.same = pending;
    }

    /// Every dirty node with its class, in `node_id` order. This reads the whole dirty set.
    pub fn work(&self) -> Vec<(NodeId, Class)> {
        self.dirty
            .iter()
            .map(|id| (id.clone(), self.class(id)))
            .collect()
    }

    fn parked(&self, id: &str) -> bool {
        self.l.get(id).is_some_and(|node| node.parked)
    }

    /// Nodes whose local position the server will not take: a move that closes a cycle against R,
    /// a create or move the server renamed, a folder merged into one L holds elsewhere with the
    /// nodes created in it, and folders L deleted on the way from such a node to the root. The
    /// commit planner sends no position change for them but still sends their content edits and deletes; the
    /// effects put each at its R position without replacing its content, and [`Trees::settle`]
    /// forgets it once L has it there. A node the user moves anywhere else stops being reverted
    /// ([`Trees::set_local`]), and so does a create or move the server renamed when L has left the
    /// position the commit sent before the result arrives.
    pub fn reverted(&self) -> &BTreeSet<NodeId> {
        &self.reverted
    }

    pub fn revert(&mut self, id: NodeId) {
        self.mark(&id);
        self.reverted.insert(id);
    }

    /// The [`Class::Same`] nodes the last [`Trees::settle`] left without searching, because the
    /// group of updates they belong to has more than [`SETTLE_GROUP_LIMIT`] members.
    pub fn unsearched(&self) -> &BTreeSet<NodeId> {
        &self.unsearched
    }

    /// S := R together for a non-empty set of `pending` nodes whose updates leave S valid, when one
    /// exists. Returns the nodes it updated, or an empty set.
    fn take_remote_together(&mut self, pending: &[NodeId]) -> BTreeSet<NodeId> {
        let (taken, unsearched) = self.search_together(pending);
        self.unsearched.clear();
        self.unsearched.extend(unsearched);
        for id in &taken {
            let next = self.r.live(id).cloned();
            self.set_synced_unmarked(id, next);
        }
        debug_assert_eq!(self.s.check(Names::ByKey), Ok(()));
        taken
    }

    /// Finds a non-empty set of `pending` nodes whose updates leave S valid when made together, and
    /// the nodes it did not search.
    ///
    /// Nodes outside the candidates keep their S shape. A candidate whose update breaks S because
    /// of such a fixed node alone (a fixed child, a fixed parent that is not a live directory, a
    /// fixed sibling holding its name key, or a parent chain through fixed nodes that leads back to
    /// it) can never be updated, so it becomes fixed too, until no more do. An update then reaches
    /// other nodes only through the first candidate on its new or old parent chain and through the
    /// candidates holding its new name key in S. Those links split the candidates into groups
    /// where any valid set's part in one group is valid on its own, so searching every subset of
    /// each group, up to [`SETTLE_GROUP_LIMIT`] members, finds a valid set whenever one exists.
    fn search_together(&self, pending: &[NodeId]) -> (BTreeSet<NodeId>, BTreeSet<NodeId>) {
        let root = self.s.root.as_str();
        let mut candidates: BTreeSet<&str> = pending.iter().map(String::as_str).collect();
        loop {
            let never: Vec<&str> = candidates
                .iter()
                .copied()
                .filter(|&id| {
                    let fixed = |other: &&str| !candidates.contains(other);
                    let fixed_child = self.s_names.children(id).any(|c| fixed(&c.as_str()));
                    let Some(shape) = self.r.live(id) else {
                        return fixed_child;
                    };
                    (fixed_child && shape.kind != Kind::Dir)
                        || (shape.parent != root
                            && fixed(&shape.parent.as_str())
                            && !self
                                .s
                                .live(&shape.parent)
                                .is_some_and(|p| p.kind == Kind::Dir))
                        || self
                            .s_names
                            .holders(&shape.parent, &shape.name)
                            .any(|h| h != id && fixed(&h.as_str()))
                        || self
                            .above(&shape.parent, &candidates)
                            .is_none_or(|up| up == id)
                })
                .collect();
            if never.is_empty() {
                break;
            }
            for id in never {
                candidates.remove(id);
            }
        }
        let mut next: BTreeMap<(&str, bool), &str> = BTreeMap::new();
        let mut links: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for &id in &candidates {
            for (taken, shape) in [(true, self.r.live(id)), (false, self.s.live(id))] {
                let Some(up) = shape
                    .and_then(|shape| self.above(&shape.parent, &candidates))
                    .filter(|up| *up != root)
                else {
                    continue;
                };
                next.insert((id, taken), up);
                links.entry(id).or_default().push(up);
                links.entry(up).or_default().push(id);
            }
            if let Some(shape) = self.r.live(id) {
                for other in self.s_names.holders(&shape.parent, &shape.name) {
                    let other = other.as_str();
                    if other != id && candidates.contains(other) {
                        links.entry(id).or_default().push(other);
                        links.entry(other).or_default().push(id);
                    }
                }
            }
        }
        let mut unsearched = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for &start in &candidates {
            if !seen.insert(start) {
                continue;
            }
            let mut group = vec![start];
            let mut i = 0;
            while let Some(&id) = group.get(i) {
                i += 1;
                for &other in links.get(id).into_iter().flatten() {
                    if seen.insert(other) {
                        group.push(other);
                    }
                }
            }
            if group.len() > SETTLE_GROUP_LIMIT {
                unsearched.extend(group.iter().map(|id| id.to_string()));
                continue;
            }
            for mask in (1..1u32 << group.len()).rev() {
                let taken = |id: &str| {
                    group
                        .iter()
                        .position(|member| *member == id)
                        .is_some_and(|i| mask >> i & 1 == 1)
                };
                let entry = |id: &str| {
                    if taken(id) {
                        self.r.live(id)
                    } else {
                        self.s.live(id)
                    }
                };
                let fits = group.iter().all(|&id| {
                    let Some(shape) = entry(id) else {
                        return true;
                    };
                    if shape.parent != root
                        && !entry(&shape.parent).is_some_and(|p| p.kind == Kind::Dir)
                    {
                        return false;
                    }
                    if taken(id)
                        && self
                            .s_names
                            .holders(&shape.parent, &shape.name)
                            .any(|h| h != id && !taken(h))
                    {
                        return false;
                    }
                    let mut current = id;
                    for _ in 0..group.len() {
                        match next.get(&(current, taken(current))) {
                            Some(up) => current = up,
                            None => return true,
                        }
                    }
                    false
                });
                if fits {
                    let taken = group.iter().filter(|id| taken(id));
                    return (taken.map(|id| id.to_string()).collect(), unsearched);
                }
            }
        }
        (BTreeSet::new(), unsearched)
    }

    /// Follows S parents from `start` past nodes that are not candidates, and returns the first
    /// candidate or the root it reaches, or `None` when the chain breaks.
    fn above<'a>(&'a self, start: &'a str, candidates: &BTreeSet<&str>) -> Option<&'a str> {
        let mut current = start;
        for _ in 0..=self.s.len() {
            if current == self.s.root || candidates.contains(current) {
                return Some(current);
            }
            current = &self.s.live(current)?.parent;
        }
        None
    }

    /// S := R for one node, made only when S stays a valid tree.
    fn take_remote(&mut self, id: &str) -> bool {
        let next = self.r.live(id).cloned();
        if !self.fits_synced(id, next.as_ref()) {
            return false;
        }
        self.set_synced_unmarked(id, next);
        true
    }

    /// True when S stays a valid tree with `next` as the node's shape.
    pub(crate) fn fits_synced(&self, id: &str, next: Option<&Shape>) -> bool {
        let has_children = self.s_names.children(id).next().is_some();
        let Some(shape) = next else {
            return !has_children;
        };
        if has_children && shape.kind != Kind::Dir {
            return false;
        }
        if shape.parent != self.s.root {
            match self.s.live(&shape.parent) {
                Some(parent) if parent.kind == Kind::Dir => {}
                _ => return false,
            }
        }
        let mut ancestor = shape.parent.as_str();
        for _ in 0..=self.s.len() {
            if ancestor == self.s.root {
                break;
            }
            if ancestor == id {
                return false;
            }
            match self.s.live(ancestor) {
                Some(parent) => ancestor = &parent.parent,
                None => return false,
            }
        }
        if ancestor != self.s.root {
            return false;
        }
        !self
            .s_names
            .holders(&shape.parent, &shape.name)
            .any(|other| other != id)
    }

    /// Drops the folder `id` from S with every node S holds below it, when S can hold the folder nowhere else and
    /// every one of those nodes is ignored too ([`Trees::released`]). Their S entries keep the folder's from
    /// being dropped, and none of them is ever planned, so without this the folder would keep its name in S for good.
    /// A node below that is not ignored keeps the whole subtree in S. Returns the nodes below that it dropped.
    fn release_subtree(&mut self, id: &str) -> Vec<NodeId> {
        let mut below = vec![id.to_owned()];
        let mut next = 0;
        while let Some(folder) = below.get(next).cloned() {
            next += 1;
            below.extend(self.s_names.children(&folder).cloned());
        }
        if !below.iter().all(|node| self.ignored(node)) {
            return Vec::new();
        }
        for node in below.iter().rev() {
            self.set_synced_unmarked(node, None);
        }
        below.split_off(1)
    }

    /// Records, for a node whose position is watched, R's position and the revision of its record
    /// while L holds that position too. S plays no part: when the contents differ, S takes neither.
    fn note_position(&mut self, id: &str) {
        let Some(taken) = self.watched_positions.get_mut(id) else {
            return;
        };
        let Some(remote) = (self.r.get(id)).filter(|remote| !remote.trashed) else {
            return;
        };
        if (self.l.get(id)).is_some_and(|local| local.shape.same_position(&remote.shape)) {
            *taken = Some(Agreed {
                rev: remote.meta_rev,
                parent: remote.shape.parent.clone(),
                name: remote.shape.name.clone(),
            });
        }
    }

    fn set_synced_unmarked(&mut self, id: &str, shape: Option<Shape>) {
        self.waiting.remove(id);
        if (shape.as_ref())
            .is_some_and(|shape| (self.l.get(id)).is_some_and(|local| local.shape.same(shape)))
        {
            self.resends.remove(id);
            self.local_changes.remove(id);
        }
        self.changed.insert(id.to_owned());
        let old = match shape {
            Some(shape) => {
                self.s_names.insert(id, &shape);
                self.s.insert(id.to_owned(), shape)
            }
            None => self.s.remove(id),
        };
        let file = |shape: Option<&Shape>| u64::from(shape.is_some_and(|s| s.kind == Kind::File));
        self.s_files = self.s_files - file(old.as_ref()) + file(self.s.get(id));
        if let Some(changed) = self.watched.get_mut(id) {
            let before = old.as_ref().and_then(|shape| shape.content.as_ref());
            let after = self.s.get(id).and_then(|shape| shape.content.as_ref());
            *changed |= match (before, after) {
                (Some(before), Some(after)) => !same_content(before, after),
                (before, after) => before.is_some() != after.is_some(),
            };
        }
        if let Some(old) = old {
            let kept = self.s.get(id).is_some_and(|new| {
                new.parent == old.parent && name_key(&new.name) == name_key(&old.name)
            });
            if !kept {
                self.s_names.remove(id, &old);
            }
        }
    }

    /// The invariants of section 2.2 for all three trees.
    pub fn check(&self) -> Result<(), (char, TreeError)> {
        debug_assert_eq!(
            self.s_files,
            (self.s.iter())
                .filter(|(_, shape)| shape.kind == Kind::File)
                .count() as u64,
            "the kept count of S's files"
        );
        debug_assert_eq!(
            self.held_trash,
            (self.l.iter()).fold(BTreeMap::new(), |mut held, (id, _)| {
                if let Some(group) = self.held_trash_group(id) {
                    *held.entry(group).or_insert(0) += 1;
                }
                held
            }),
            "the kept count of the trashed nodes L holds"
        );
        debug_assert_eq!(
            self.trashed,
            (self.r.iter()).fold(BTreeMap::new(), |mut trashed, (_, node)| {
                if node.trashed {
                    *trashed.entry(node.trash_group.clone()).or_insert(0) += 1;
                }
                trashed
            }),
            "the kept count of the trashed nodes R holds"
        );
        debug_assert_eq!(
            (self.local_changes.iter())
                .find(|id| self.all_trees_hold(id))
                .cloned(),
            None,
            "a change of this device's user that L, S and R all hold"
        );
        self.r.check(Names::ByKey).map_err(|e| ('R', e))?;
        self.s.check(Names::ByKey).map_err(|e| ('S', e))?;
        self.l.check(Names::Exact).map_err(|e| ('L', e))
    }

    /// What the store keeps besides the nodes of R, L and S.
    pub fn state(&self) -> TreesState {
        TreesState {
            dirty: (*self.dirty).clone(),
            unsearched: (*self.unsearched).clone(),
            reverted: (*self.reverted).clone(),
            parked_as: (*self.parked_as).clone(),
            waiting: (*self.waiting).clone(),
            watched: (*self.watched).clone(),
            watched_positions: (*self.watched_positions).clone(),
            trashed_here: (*self.trashed_here).clone(),
            removed_here: (*self.removed_here).clone(),
            discarded: (*self.discarded).clone(),
            outside: (*self.outside).clone(),
            lost: (*self.lost).clone(),
            displaced: (*self.displaced).clone(),
            edited_here: (*self.edited_here).clone(),
            moved_here: (*self.moved_here).clone(),
            past_positions: (*self.past_positions).clone(),
            unsent_edits: (*self.unsent_edits).clone(),
            dropped_edits: (*self.dropped_edits).clone(),
            replaced_versions: (*self.replaced_versions).clone(),
            resends: (*self.resends).clone(),
            local_changes: (*self.local_changes).clone(),
            kept_originals: (*self.kept_originals).clone(),
        }
    }

    /// What changed in [`Trees::state`] since the last call, for the store to write.
    pub fn take_changes(&mut self) -> TreesChanges {
        let Self {
            r: _,
            l: _,
            s: _,
            s_files: _,
            held_trash: _,
            trashed: _,
            dirty,
            unsearched,
            reverted,
            parked_as,
            waiting,
            watched,
            watched_positions,
            trashed_here,
            removed_here,
            discarded,
            adopted: _,
            edited_here,
            moved_here,
            past_positions,
            unsent_edits,
            dropped_edits,
            replaced_versions,
            resends,
            local_changes,
            kept_originals,
            ignore: _,
            outside,
            lost,
            displaced,
            rules_known: _,
            unknown_rules: _,
            asked: _,
            unsettled: _,
            same: _,
            changed: _,
            l_names: _,
            l_identities: _,
            s_names: _,
            orphaned: _,
            remote_rules_files: _,
        } = self;
        TreesChanges {
            dirty: dirty.take_changes(),
            unsearched: unsearched.take_changes(),
            reverted: reverted.take_changes(),
            parked_as: parked_as.take_changes(),
            waiting: waiting.take_changes(),
            watched: watched.take_changes(),
            watched_positions: watched_positions.take_changes(),
            trashed_here: trashed_here.take_changes(),
            removed_here: removed_here.take_changes(),
            discarded: discarded.take_changes(),
            outside: outside.take_changes(),
            lost: lost.take_changes(),
            displaced: displaced.take_changes(),
            edited_here: edited_here.take_changes(),
            moved_here: moved_here.take_changes(),
            past_positions: past_positions.take_changes(),
            unsent_edits: unsent_edits.take_changes(),
            dropped_edits: dropped_edits.take_changes(),
            replaced_versions: replaced_versions.take_changes(),
            resends: resends.take_changes(),
            local_changes: local_changes.take_changes(),
            kept_originals: kept_originals.take_changes(),
        }
    }

    /// Replaces the dirty, unsearched, reverted, parked, waiting and watched nodes with `state`,
    /// once the store has loaded every node. The rules are unknown from here until
    /// [`Trees::rules_scanned`].
    /// Refuses trees that break section 2.2, a node that is not dirty
    /// although it is parked or not in sync, an unsearched node that is not dirty, and a parked
    /// name for a node L does not hold as parked.
    pub fn restore(&mut self, state: TreesState) -> Result<(), StateError> {
        self.check()
            .map_err(|(tree, error)| StateError::Tree(tree, error))?;
        let ids = (self.r.iter().map(|(id, _)| id))
            .chain(self.l.iter().map(|(id, _)| id))
            .chain(self.s.iter().map(|(id, _)| id));
        let settled = |id: &NodeId| {
            self.class(id) == Class::InSync
                && !self.l.get(id).is_some_and(|node| node.parked)
                && !state.unsent_edits.contains(id)
        };
        if let Some(id) = ids
            .filter(|id| !state.dirty.contains(*id))
            .find(|id| !settled(id))
        {
            return Err(StateError::NotDirty(id.clone()));
        }
        if let Some(id) = state.unsearched.difference(&state.dirty).next() {
            return Err(StateError::UnsearchedNotDirty(id.clone()));
        }
        let placed = |id: &&NodeId| !self.l.get(id).is_some_and(|node| node.parked);
        if let Some(id) = state.parked_as.keys().find(placed) {
            return Err(StateError::NotParked(id.clone()));
        }
        let trashed = |(id, seq): &(&NodeId, &u64)| {
            (self.r.get(id)).is_some_and(|remote| remote.trashed && remote.meta_rev == **seq)
        };
        if let Some((id, _)) = state.trashed_here.iter().find(|entry| !trashed(entry)) {
            return Err(StateError::NotTrashedHere(id.clone()));
        }
        if let Some((id, _)) = state.removed_here.iter().find(|entry| !trashed(entry)) {
            return Err(StateError::NotRemovedHere(id.clone()));
        }
        if let Some(id) = (state.outside.iter()).find(|id| self.l.get(id).is_none()) {
            return Err(StateError::NotIgnoredLocal(id.clone()));
        }
        if let Some(id) = (state.lost.iter()).find(|id| !state.outside.contains(*id)) {
            return Err(StateError::NotIgnoredLocal(id.clone()));
        }
        let kept = |id: &&NodeId| self.l.get(id).is_none() && self.r.live(id).is_some();
        if let Some(id) = state.displaced.keys().find(|id| !kept(id)) {
            return Err(StateError::NotDisplaced(id.clone()));
        }
        let edited = |(id, seq): &(&NodeId, &u64)| {
            (self.r.get(id)).is_some_and(|remote| !remote.trashed && remote.edit_rev == **seq)
        };
        if let Some((id, _)) = state.edited_here.iter().find(|entry| !edited(entry)) {
            return Err(StateError::NotEditedHere(id.clone()));
        }
        let moved = |(id, moved): &(&NodeId, &MovedHere)| {
            (self.r.get(id)).is_some_and(|remote| remote.meta_rev >= moved.seq)
        };
        if let Some((id, _)) = state.moved_here.iter().find(|entry| !moved(entry)) {
            return Err(StateError::NotMovedHere(id.clone()));
        }
        if let Some(id) = state
            .past_positions
            .keys()
            .find(|id| self.r.get(id).is_none())
        {
            return Err(StateError::NotPastPosition(id.clone()));
        }
        if let Some(id) = state
            .unsent_edits
            .iter()
            .find(|id| self.l.get(id).is_none())
        {
            return Err(StateError::NotUnsentEdit(id.clone()));
        }
        if let Some(id) = (state.replaced_versions.keys()).find(|id| self.r.get(id).is_none()) {
            return Err(StateError::NotRemote(id.clone()));
        }
        if let Some(id) = (state.resends.iter()).find(|id| self.l.get(id).is_none()) {
            return Err(StateError::NotResend(id.clone()));
        }
        if let Some(id) = (state.local_changes.iter()).find(|id| self.l.get(id).is_none()) {
            return Err(StateError::NotLocalChange(id.clone()));
        }
        let TreesState {
            dirty,
            unsearched,
            reverted,
            parked_as,
            waiting,
            watched,
            watched_positions,
            trashed_here,
            removed_here,
            discarded,
            outside,
            lost,
            displaced,
            edited_here,
            moved_here,
            past_positions,
            unsent_edits,
            dropped_edits,
            replaced_versions,
            resends,
            local_changes,
            kept_originals,
        } = state;
        self.unsettled = dirty.clone();
        self.same.clear();
        self.orphaned.clear();
        self.dirty = dirty.into();
        self.unsearched = unsearched.into();
        self.reverted = reverted.into();
        self.parked_as = parked_as.into();
        self.waiting = waiting.into();
        self.watched = watched.into();
        self.watched_positions = watched_positions.into();
        self.trashed_here = trashed_here.into();
        self.removed_here = removed_here.into();
        self.discarded = discarded.into();
        self.outside = outside.into();
        self.lost = lost.into();
        self.displaced = displaced.into();
        self.edited_here = edited_here.into();
        self.moved_here = moved_here.into();
        self.past_positions = past_positions.into();
        self.unsent_edits = unsent_edits.into();
        self.dropped_edits = dropped_edits.into();
        self.replaced_versions = replaced_versions.into();
        self.resends = resends.into();
        self.local_changes = local_changes.into();
        self.kept_originals = kept_originals.into();
        self.rules_known = false;
        self.unknown_rules.clear();
        Ok(())
    }
}

/// Counts a node of `group` in `counts`, `add` or not. A node of no group counts nowhere.
fn count_group(counts: &mut BTreeMap<String, u64>, group: Option<String>, add: bool) {
    let Some(group) = group else {
        return;
    };
    let count = counts.entry(group.clone()).or_insert(0);
    if add {
        *count += 1;
    } else {
        *count -= 1;
        if *count == 0 {
            counts.remove(&group);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Discard {
    Reclaimed,
    Kept,
    Pending,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Made {
    Elsewhere,
    Here,
    Edit,
    Answer,
}

#[cfg(test)]
mod tests;
