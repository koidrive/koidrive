// SPDX-License-Identifier: Apache-2.0

//! The engine state the store keeps between runs (docs/design/sync-engine.md section 2.8), as
//! plain values. The nodes of R, L and S are not here: the store keeps them per node and loads
//! them with [`Trees::set_remote`], [`Trees::set_local`] and [`Trees::set_synced`]. Everything
//! else the engine needs to continue where it stopped is in [`TreesState`], [`CommitsState`] and
//! [`EffectsState`]. Restoring checks the invariants the engine relies on and refuses a state
//! that breaks one, with a [`StateError`] that names it.
//!
//! [`Trees::set_remote`]: crate::classify::Trees::set_remote
//! [`Trees::set_local`]: crate::classify::Trees::set_local
//! [`Trees::set_synced`]: crate::classify::Trees::set_synced

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Waiting;
use crate::commit::{Halt, InFlight};
use crate::effects::{Effect, EffectRecord};
use crate::guard::{Counted, GuardState, RemoteDeletion};
use crate::question::{Held, Limit, Question};
use crate::restore::{Resolved, RestoreId, RestoreIntent};
use crate::tree::{FileIdentity, NodeId, Shape, TreeError};

/// A position L and R agreed on, with the revision of R's record that held it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agreed {
    pub rev: u64,
    pub parent: NodeId,
    pub name: String,
}

/// A position a create or a move of this device set: the seq of its commit, and whether the store
/// upgrade recorded it, without the positions R held before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovedHere {
    pub seq: u64,
    pub upgraded: bool,
}

/// The position R held a node at before a server loss, where a resend of the node creates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LostPosition {
    pub parent: NodeId,
    pub name: String,
}

/// What [`Trees`](crate::classify::Trees) holds besides the nodes of R, L and S.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreesState {
    /// Nodes whose entries changed since they were last found in sync.
    pub dirty: BTreeSet<NodeId>,
    /// Dirty nodes the last settle left without searching. Each is also in `dirty`.
    pub unsearched: BTreeSet<NodeId>,
    /// Nodes whose local position the server will not take.
    pub reverted: BTreeSet<NodeId>,
    /// For a parked node re-keyed while parked, the name of its entry in the parked directory.
    /// Each key is a node L holds as parked.
    pub parked_as: BTreeMap<NodeId, NodeId>,
    /// S updates of acknowledged ops that wait until S stays a valid tree with them, by node.
    /// `None` removes the node from S.
    pub waiting: BTreeMap<NodeId, Waiting>,
    /// The nodes whose content the commit in flight creates or updates, each with whether S's
    /// content changed since the commit was built.
    pub watched: BTreeMap<NodeId, bool>,
    /// The nodes the commit in flight creates or moves, each with the latest position L and R
    /// agreed on since the commit was built.
    pub watched_positions: BTreeMap<NodeId, Option<Agreed>>,
    /// The nodes a delete of this device trashed, each with the seq of its commit, while R holds
    /// them trashed by that commit.
    pub trashed_here: BTreeMap<NodeId, u64>,
    /// The nodes this device's user removed while R held them trashed by a delete of another
    /// device, each with the seq of that delete's commit, while R holds them trashed by it.
    pub removed_here: BTreeMap<NodeId, u64>,
    /// The nodes the person discarded in answer to a first-link question (section 6.4), each with
    /// the shape the question showed, while L, S or the S update waiting for them holds them.
    pub discarded: BTreeMap<NodeId, Shape>,
    /// The nodes whose local copy the user moved into an ignored folder that is no node, and the folders L keeps
    /// for them. L holds each at the position it had before.
    pub outside: BTreeSet<NodeId>,
    /// The folders of `outside` that the disk lost, which L keeps only for what is recorded below them.
    pub lost: BTreeSet<NodeId>,
    /// The nodes whose local copy is in an ignored folder and whose name in L another entry took, each with the
    /// identity L held for it. L holds none of them, and R holds each live.
    pub displaced: BTreeMap<NodeId, FileIdentity>,
    /// The files whose content an edit of this device set, each with the seq of its commit, while
    /// R holds that content.
    pub edited_here: BTreeMap<NodeId, u64>,
    /// The nodes whose position a create or a move of this device set, while R holds them at that
    /// position, also after a delete and a restore of the node.
    pub moved_here: BTreeMap<NodeId, MovedHere>,
    /// The earlier live records R held of each node, oldest first: the newest
    /// [`PAST_POSITIONS_KEPT`](crate::classify::PAST_POSITIONS_KEPT), and the one a move of
    /// `moved_here` replaced, which shows that a move and not a create set that position.
    pub past_positions: BTreeMap<NodeId, Vec<Agreed>>,
    /// The files whose content in L is an edit of this device that the server does not hold,
    /// although S and R may hold the same bytes (section 4.4).
    pub unsent_edits: BTreeSet<NodeId>,
    /// The files the commit in flight edited when this device learned that the server lost commits
    /// of it. The bootstrap after the loss takes each whose content in L still differs from S's
    /// as a lost edit (section 4.4).
    pub dropped_edits: BTreeSet<NodeId>,
    /// The nodes a delete of a conflict answer removes, each with the file the answer keeps, which the delete
    /// requires live (RFC 0015). Each is held while R holds the node trashed by that delete, or while its delete is
    /// sent again after the server lost it (section 4.4).
    pub kept_originals: BTreeMap<NodeId, NodeId>,
    /// For each file of R, the `version_id`s it held before its current one as the log moved it
    /// forward, oldest first, at most
    /// [`REPLACED_VERSIONS_KEPT`](crate::classify::REPLACED_VERSIONS_KEPT). A server never gives a
    /// version ID to two versions, also across a loss of commits (RFC 0016), so a version of this
    /// list that a restored server holds is one that the content S took from R replaced.
    pub replaced_versions: BTreeMap<NodeId, Vec<String>>,
    /// The nodes this device sends again after a server loss although no change of its user is in
    /// them (section 4.4): a file whose content S took back from R so that the content L holds is
    /// sent again, and a node S forgot that L holds as S did. A delete that R holds of the node, or
    /// of a folder L holds it under, beats the resend unless L holds a change of this device's user
    /// below it: S then takes L's shape, so the delete reaches L. So does R taking the node again,
    /// live with the content L holds, after R lacked it. The record also ends when S takes L's shape
    /// or L changes the node. A file whose resend ends with a change of this device's user holds an
    /// edit the server does not have ([`TreesState::unsent_edits`]). A folder S holds and R lacks
    /// that L holds such a change under is sent again as a resend.
    pub resends: BTreeSet<NodeId>,
    /// The nodes L holds a change of this device's user in: a scan found L's shape changed, a
    /// rekey made L's copy, L holds a shape other than the one the server took from a commit of
    /// this device, or a commit of this device that the server lost made it. A node leaves once S
    /// takes L's shape, a scan finds L back at the shape S holds, L, S and R hold the same shape,
    /// or L no longer holds it.
    pub local_changes: BTreeSet<NodeId>,
    /// For a node S forgot as a resend while R lacked it, the position R held it at before the loss
    /// when that differs from the position L and S held: a move another device made that L had not
    /// taken yet. A create of the node is made there, also after L's content changed, and once the
    /// server takes it S takes L's position with the content it sent, so L takes the move. A live
    /// record of the node that another device made moves the position to that record's. The record
    /// ends when L moves or removes the node, or when L and R hold it live at the same position.
    pub lost_positions: BTreeMap<NodeId, LostPosition>,
    /// The nodes S forgot after a server loss that R lacks and L holds as S did, whose position no
    /// move of this device set: a create of this device, if any, set it, and the server lost that
    /// create. Another device that creates such a node again held it, so R taking the node again,
    /// live with the content L holds, is a remote move: S takes L's shape. That includes a device
    /// that held only a position from before an earlier loss, which a later create of this device
    /// replaced. Unlike a resend, a delete made after the loss does not beat it. The record ends
    /// when R holds the node again, and when S takes L's shape or L changes or removes the node.
    pub lost_creates: BTreeSet<NodeId>,
}

/// A node this device sent under a pending name (docs/design/sync-engine.md section 2.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The pending name the node was sent under.
    pub sent: String,
    /// The name L gave the node, which it takes once it leaves the pending name.
    pub name: String,
    /// Whether R has held the node under `sent`. Until then the commit that sends it there may
    /// still be in flight, and R need not hold it there.
    pub landed: bool,
}

/// What [`Commits`](crate::commit::Commits) holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitsState {
    /// The accepted counter *a*, the `device_seq` of the last commit whose result was applied.
    pub accepted: u64,
    /// The commit in flight, with its result once that has arrived. Its `device_seq` is
    /// `accepted + 1`.
    pub in_flight: Option<InFlight>,
    /// Nodes an op was rejected for, left out of planning until cleared.
    pub problems: BTreeSet<NodeId>,
    /// Why no commit is built or resent, if committing has stopped.
    pub halted: Option<Halt>,
    /// The most ops the next commit holds after a `LIMIT_EXCEEDED`. It is at least 1 and below
    /// [`MAX_OPS`](crate::commit::MAX_OPS).
    pub cap: Option<usize>,
    /// Folders whose delete did not fit alone, deleted contents first.
    pub split: BTreeSet<NodeId>,
    /// Folders merged into one L holds elsewhere, by the folder they were merged into.
    pub aliases: BTreeMap<NodeId, NodeId>,
    /// The restores a person asked for that have no outcome yet, in request order. The commit in
    /// flight carries those its `Restore` ops name.
    pub restores: BTreeMap<RestoreId, RestoreIntent>,
    /// The restores that ended, oldest first, at most
    /// [`RESOLVED_KEPT`](crate::restore::RESOLVED_KEPT).
    pub resolved: Vec<Resolved>,
    /// The seq of each of this device's last [`SLOTS_KEPT`](crate::commit::SLOTS_KEPT)
    /// acknowledged commits, by slot.
    pub slots: BTreeMap<u64, u64>,
    /// The open questions, in the order they were asked.
    pub questions: Vec<Question>,
    /// The local entries a Publish answer to a first link's question released, until the server
    /// applies their create.
    pub published: BTreeSet<NodeId>,
    /// The nodes this device sent under a pending name, while R holds each at that name, live or
    /// in the trash, and until a move of this device that takes it off that name is acknowledged.
    pub pending: BTreeMap<NodeId, Pending>,
    /// The mass-delete guard's window and confirmed deletions.
    pub guard: GuardState,
}

/// What [`Effects`](crate::effects::Effects) holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectsState {
    /// The effects in flight, in the order they were planned. The `counter` of each is `cleared` plus their number.
    pub in_flight: Vec<EffectRecord>,
    /// The effect counter *e*, the number of effects cleared so far.
    pub cleared: u64,
    /// The effect that failed last for each blocked node.
    pub failed: BTreeMap<NodeId, Effect>,
}

/// What changed in a set since its changes were last taken: each key whose membership changed,
/// with whether the set holds it now.
pub type SetChanges<K> = Vec<(K, bool)>;

/// What changed in a map since its changes were last taken: each key whose value changed, with
/// its value now, or `None` when the map no longer holds the key.
pub type MapChanges<K, V> = Vec<(K, Option<V>)>;

/// What changed in [`TreesState`] since the changes were last taken
/// ([`Trees::take_changes`](crate::classify::Trees::take_changes)), field by field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreesChanges {
    pub dirty: SetChanges<NodeId>,
    pub unsearched: SetChanges<NodeId>,
    pub reverted: SetChanges<NodeId>,
    pub parked_as: MapChanges<NodeId, NodeId>,
    pub waiting: MapChanges<NodeId, Waiting>,
    pub watched: MapChanges<NodeId, bool>,
    pub watched_positions: MapChanges<NodeId, Option<Agreed>>,
    pub trashed_here: MapChanges<NodeId, u64>,
    pub removed_here: MapChanges<NodeId, u64>,
    pub discarded: MapChanges<NodeId, Shape>,
    pub outside: SetChanges<NodeId>,
    pub lost: SetChanges<NodeId>,
    pub displaced: MapChanges<NodeId, FileIdentity>,
    pub edited_here: MapChanges<NodeId, u64>,
    pub moved_here: MapChanges<NodeId, MovedHere>,
    pub past_positions: MapChanges<NodeId, Vec<Agreed>>,
    pub unsent_edits: SetChanges<NodeId>,
    pub dropped_edits: SetChanges<NodeId>,
    pub replaced_versions: MapChanges<NodeId, Vec<String>>,
    pub resends: SetChanges<NodeId>,
    pub local_changes: SetChanges<NodeId>,
    pub lost_positions: MapChanges<NodeId, LostPosition>,
    pub lost_creates: SetChanges<NodeId>,
    pub kept_originals: MapChanges<NodeId, NodeId>,
}

impl TreesChanges {
    /// The changes that take `before` to `after`.
    pub fn between(before: &TreesState, after: &TreesState) -> Self {
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
            lost_positions,
            lost_creates,
            kept_originals,
        } = after;
        Self {
            dirty: set_changes(&before.dirty, dirty),
            unsearched: set_changes(&before.unsearched, unsearched),
            reverted: set_changes(&before.reverted, reverted),
            parked_as: map_changes(&before.parked_as, parked_as),
            waiting: map_changes(&before.waiting, waiting),
            watched: map_changes(&before.watched, watched),
            watched_positions: map_changes(&before.watched_positions, watched_positions),
            trashed_here: map_changes(&before.trashed_here, trashed_here),
            removed_here: map_changes(&before.removed_here, removed_here),
            discarded: map_changes(&before.discarded, discarded),
            outside: set_changes(&before.outside, outside),
            lost: set_changes(&before.lost, lost),
            displaced: map_changes(&before.displaced, displaced),
            edited_here: map_changes(&before.edited_here, edited_here),
            moved_here: map_changes(&before.moved_here, moved_here),
            past_positions: map_changes(&before.past_positions, past_positions),
            unsent_edits: set_changes(&before.unsent_edits, unsent_edits),
            dropped_edits: set_changes(&before.dropped_edits, dropped_edits),
            replaced_versions: map_changes(&before.replaced_versions, replaced_versions),
            resends: set_changes(&before.resends, resends),
            local_changes: set_changes(&before.local_changes, local_changes),
            lost_positions: map_changes(&before.lost_positions, lost_positions),
            lost_creates: set_changes(&before.lost_creates, lost_creates),
            kept_originals: map_changes(&before.kept_originals, kept_originals),
        }
    }
}

/// What changed in [`CommitsState`] since the changes were last taken
/// ([`Commits::take_changes`](crate::commit::Commits::take_changes)). A value the store keeps
/// whole is `Some` when it changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitsChanges {
    pub accepted: Option<u64>,
    pub in_flight: Option<Option<InFlight>>,
    pub problems: SetChanges<NodeId>,
    pub halted: Option<Option<Halt>>,
    pub cap: Option<Option<usize>>,
    pub split: SetChanges<NodeId>,
    pub aliases: MapChanges<NodeId, NodeId>,
    pub restores: Option<BTreeMap<RestoreId, RestoreIntent>>,
    pub resolved: Option<Vec<Resolved>>,
    pub slots: MapChanges<u64, u64>,
    pub questions: Option<Vec<Question>>,
    pub published: SetChanges<NodeId>,
    pub pending: MapChanges<NodeId, Pending>,
    pub guard: GuardChanges,
}

impl CommitsChanges {
    /// The changes that take `before` to `after`.
    pub fn between(before: &CommitsState, after: &CommitsState) -> Self {
        let CommitsState {
            accepted,
            in_flight,
            problems,
            halted,
            cap,
            split,
            aliases,
            restores,
            resolved,
            slots,
            questions,
            published,
            pending,
            guard,
        } = after;
        Self {
            accepted: value_change(&before.accepted, accepted),
            in_flight: value_change(&before.in_flight, in_flight),
            problems: set_changes(&before.problems, problems),
            halted: value_change(&before.halted, halted),
            cap: value_change(&before.cap, cap),
            split: set_changes(&before.split, split),
            aliases: map_changes(&before.aliases, aliases),
            restores: value_change(&before.restores, restores),
            resolved: value_change(&before.resolved, resolved),
            slots: map_changes(&before.slots, slots),
            questions: value_change(&before.questions, questions),
            published: set_changes(&before.published, published),
            pending: map_changes(&before.pending, pending),
            guard: GuardChanges::between(&before.guard, guard),
        }
    }
}

/// What changed in [`GuardState`] since the changes were last taken.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardChanges {
    pub ledger: MapChanges<NodeId, Counted>,
    pub confirmed: MapChanges<NodeId, Held>,
    pub confirmed_limits: Option<BTreeSet<Limit>>,
    pub remote: MapChanges<RemoteDeletion, Counted>,
    pub dismissed: SetChanges<RemoteDeletion>,
}

impl GuardChanges {
    /// The changes that take `before` to `after`.
    pub fn between(before: &GuardState, after: &GuardState) -> Self {
        let GuardState {
            ledger,
            confirmed,
            confirmed_limits,
            remote,
            dismissed,
        } = after;
        Self {
            ledger: map_changes(&before.ledger, ledger),
            confirmed: map_changes(&before.confirmed, confirmed),
            confirmed_limits: value_change(&before.confirmed_limits, confirmed_limits),
            remote: map_changes(&before.remote, remote),
            dismissed: set_changes(&before.dismissed, dismissed),
        }
    }
}

/// What changed in [`EffectsState`] since the changes were last taken
/// ([`Effects::take_changes`](crate::effects::Effects::take_changes)).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectsChanges {
    pub in_flight: Option<Vec<EffectRecord>>,
    pub cleared: Option<u64>,
    pub failed: MapChanges<NodeId, Effect>,
}

impl EffectsChanges {
    /// The changes that take `before` to `after`.
    pub fn between(before: &EffectsState, after: &EffectsState) -> Self {
        let EffectsState {
            in_flight,
            cleared,
            failed,
        } = after;
        Self {
            in_flight: value_change(&before.in_flight, in_flight),
            cleared: value_change(&before.cleared, cleared),
            failed: map_changes(&before.failed, failed),
        }
    }
}

fn set_changes<K: Ord + Clone>(before: &BTreeSet<K>, after: &BTreeSet<K>) -> SetChanges<K> {
    (before.symmetric_difference(after))
        .map(|key| (key.clone(), after.contains(key)))
        .collect()
}

fn map_changes<K: Ord + Clone, V: Clone + PartialEq>(
    before: &BTreeMap<K, V>,
    after: &BTreeMap<K, V>,
) -> MapChanges<K, V> {
    let keys: BTreeSet<&K> = before.keys().chain(after.keys()).collect();
    (keys.into_iter())
        .filter_map(|key| {
            let value = after.get(key);
            (before.get(key) != value).then(|| (key.clone(), value.cloned()))
        })
        .collect()
}

fn value_change<T: Clone + PartialEq>(before: &T, after: &T) -> Option<T> {
    (before != after).then(|| after.clone())
}

/// An invariant a restored state breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    /// R, L or S, named by its letter, is not a valid tree.
    Tree(char, TreeError),
    /// A node is parked or not in sync but not dirty, so settling and planning would skip it.
    NotDirty(NodeId),
    /// A node is unsearched but not dirty.
    UnsearchedNotDirty(NodeId),
    /// A node has a parked name but L does not hold it as parked.
    NotParked(NodeId),
    /// A node is recorded as trashed by a delete of this device, but R does not hold it trashed by
    /// that commit.
    NotTrashedHere(NodeId),
    /// A node is recorded as removed by this device's user while a delete of another device
    /// trashed it, but R does not hold it trashed by that commit.
    NotRemovedHere(NodeId),
    /// A node is recorded as moved into an ignored folder, or as a folder the disk lost, but L does not hold it or the
    /// lost folder is not recorded.
    NotIgnoredLocal(NodeId),
    /// A node is recorded as displaced, but L holds it or R does not hold it live.
    NotDisplaced(NodeId),
    /// A file is recorded as edited by this device, but R does not hold the content of that
    /// commit.
    NotEditedHere(NodeId),
    /// A node is recorded as moved by this device, but R does not hold it at a revision at or after
    /// that commit.
    NotMovedHere(NodeId),
    /// Earlier records are kept for a node R does not hold.
    NotPastPosition(NodeId),
    /// A file is recorded with an edit the server does not hold, but L does not hold it.
    NotUnsentEdit(NodeId),
    NotRemote(NodeId),
    /// A node is recorded as sent again after a server loss, but L does not hold it.
    NotResend(NodeId),
    /// A node is recorded as holding a change of this device's user, but L does not hold it.
    NotLocalChange(NodeId),
    /// A lost position is recorded for a node L does not hold.
    NotLostPosition(NodeId),
    /// A node is recorded as a lost create, but R holds it or L does not.
    NotLostCreate(NodeId),
    /// The commit in flight does not take the slot after the accepted counter.
    DeviceSeq {
        accepted: u64,
        device_seq: u64,
    },
    /// The result of the commit in flight does not have one outcome per op.
    ResultMismatch,
    /// Planned deletes are listed for a node that no `Delete` op of the commit in flight targets.
    DeleteWithoutOp(NodeId),
    /// The op cap is 0, or not below `MAX_OPS`.
    Cap(usize),
    /// A folder is merged into itself.
    SelfAlias(NodeId),
    /// The commit in flight holds a `Restore` op, at this index, that no queued restore asks for.
    RestoreNotQueued(usize),
    /// A restore is queued and also has an outcome, or has two outcomes.
    RestoreResolved(RestoreId),
    /// An effect in flight does not carry the effects cleared plus the effects in flight as its counter.
    EffectCounter {
        cleared: u64,
        counter: u64,
    },
}
